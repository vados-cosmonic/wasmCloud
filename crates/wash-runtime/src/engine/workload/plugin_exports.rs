//! How a host component plugin reaches the exports of a workload's items:
//! finding which item exports an interface the plugin imports,
//! and building the invocations that call it.

use super::*;

/// One function of an interface a host component plugin imports, named and
/// typed as the plugin's own component type declares it. The types are what
/// [`ResolvedWorkload::external_export_invocations`] classifies to decide how a
/// call's arguments and results cross the store boundary.
pub(crate) struct ExternalCallFunc<'a> {
    pub(crate) name: &'a str,
    pub(crate) param_tys: &'a [wasmtime::component::types::Type],
    pub(crate) result_tys: &'a [wasmtime::component::types::Type],
}

/// What [`ResolvedWorkload::item_exporting`] found about the one item it was
/// asked about: whether that item exports an interface a host component plugin
/// imports, and how to address the export.
pub(crate) enum ItemExport {
    /// A component exports it.
    Component {
        /// The component's manifest name, for the tie-break when two of them
        /// export the same interface.
        name: Arc<str>,
        /// The component's own name for the export, which is what addresses it
        /// on the instance. Not necessarily the plugin's name for the import:
        /// matching tolerates one side omitting the version, so a plugin whose
        /// package is unversioned can be satisfied by a versioned export. The
        /// matched name has to travel with the answer, or looking the export up
        /// by the plugin's name misses it.
        export: Arc<str>,
    },
    /// The workload's long-lived service exports it. Reached by routing into
    /// the instance already running, the way inbound HTTP and messaging are —
    /// never by instantiating a second copy, whose state would not be the one
    /// the service has been accumulating.
    Service {
        /// The service's own name for the export, as for a component.
        export: Arc<str>,
    },
    /// Neither — this item bound to the plugin for a capability it imports.
    None,
}

/// The item's own name for the export serving `interface`, if it has one.
///
/// Not the plugin's name for the import: the two agree except where matching
/// tolerated a version on one side only, and addressing the export on an
/// instance needs the name the item actually used.
pub(super) fn export_named(
    exports: Option<Vec<(String, ComponentItem)>>,
    interface: &WitInterface,
) -> Option<Arc<str>> {
    exports?
        .into_iter()
        .find(|(name, _)| WitInterface::from(name.as_str()).contains(interface))
        .map(|(name, _)| Arc::from(name))
}

impl ResolvedWorkload {
    /// Which item of the workload exports `interface` — what a host component
    /// plugin checks before claiming one as the item serving an interface it
    /// imports.
    pub(crate) async fn item_exporting(
        &self,
        item_id: &str,
        interface: &WitInterface,
    ) -> ItemExport {
        let exports = |world: &WitWorld| world.exports.iter().any(|e| e.contains(interface));
        // A workload's long-lived service is bound as an item like any
        // component, and `bind_plugins` hands its id here the same way, but it
        // lives outside the component map — so check it explicitly rather than
        // letting the lookup below miss and read as "exports nothing".
        if let Some(service) = &self.service
            && service.id() == item_id
        {
            if !exports(&service.world()) {
                return ItemExport::None;
            }
            return match export_named(service.component_exports().ok(), interface) {
                Some(export) => ItemExport::Service { export },
                // The world says it serves this and the component does not name
                // it so: a version the two spell differently, or exports that
                // would not parse. Loudly, because everything downstream reads
                // the same as a service that simply does not export it — the
                // workload deploys healthy and every call naming it fails with
                // nothing to connect that to the service.
                None => {
                    tracing::warn!(
                        workload_id = %self.id,
                        service = item_id,
                        %interface,
                        "the workload\'s service declares this interface but exports no \
                         instance matching it; deploying without that route"
                    );
                    ItemExport::None
                }
            };
        }
        let components = self.components.read().await;
        let Some(component) = components.get(item_id) else {
            return ItemExport::None;
        };
        let Some(export) = export_named(component.component_exports().ok(), interface) else {
            return ItemExport::None;
        };
        ItemExport::Component {
            name: Arc::from(component.name()),
            export,
        }
    }

    /// Build the invocations a host component plugin uses to call `interface`
    /// on `component_id`'s export — the reverse of the capability direction,
    /// where the plugin serves what a workload imports.
    ///
    /// A plugin runs in a store of its own, so such a call can never share one
    /// with the callee: arguments and results always move across the boundary.
    /// A component is called in a store of its own — one of its warm instances,
    /// or one built for the call — and the workload's service on the instance
    /// it already runs as.
    ///
    /// A function the callee does not export, or whose signature carries a
    /// `resource` or `error-context` handle that cannot cross, fails the
    /// workload's deploy. The alternative — installing a partial route and
    /// letting the omitted function error when called — moves the failure onto
    /// the plugin's call path, where an error has nowhere to go: a
    /// workload-exported import has no error result, so the host returning one
    /// traps the plugin guest and restarts the store every tenant shares. One
    /// workload failing to deploy over a WIT its plugin disagrees with is the
    /// contained outcome.
    pub(crate) async fn external_export_invocations(
        &self,
        component_id: &str,
        interface: &str,
        funcs: &[ExternalCallFunc<'_>],
    ) -> anyhow::Result<BTreeMap<Arc<str>, LinkedExportInvocation>> {
        let (component, engine, linked_component_ids) = {
            let components = self.components.read().await;
            let component = components
                .get(component_id)
                .context("component ID not found in workload")?;
            (
                component.metadata.component.clone(),
                component.metadata.engine().clone(),
                component
                    .metadata
                    .linked_components
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let Some((ComponentItem::ComponentInstance(_), instance_idx)) =
            component.get_export(None, interface)
        else {
            bail!("component '{component_id}' does not export {interface}");
        };
        // Taken after the read guard above is dropped: this needs the write lock.
        let pre = self.instantiate_pre(component_id).await?;
        let component_id: Arc<str> = Arc::from(component_id);

        let mut invocations = BTreeMap::new();
        for func in funcs {
            let Some((ComponentItem::ComponentFunc(_), func_idx)) =
                component.get_export(Some(&instance_idx), func.name)
            else {
                bail!(
                    "component '{component_id}' exports {interface} but not {}, which a host \
                     component plugin imports; the workload's copy of the interface disagrees \
                     with the plugin's",
                    func.name
                );
            };
            let mode = if types_are_ephemeral_safe(func.param_tys)
                && types_are_ephemeral_safe(func.result_tys)
            {
                EphemeralCallMode::PlainValue
            } else if types_are_bridge_safe(func.param_tys, &self.stream_payloads)
                && types_are_bridge_safe(func.result_tys, &self.stream_payloads)
            {
                EphemeralCallMode::Relocated {
                    param_tys: func.param_tys.into(),
                    result_tys: func.result_tys.into(),
                }
            } else {
                bail!(
                    "{interface}#{} carries a handle that cannot cross the boundary between a \
                     plugin's store and a workload's; only plain values, `stream<T>`, and \
                     `future<T>` can; a `stream<T>`/`future<T>` of an \
                     application-defined payload needs `Engine::stream_payloads().register`",
                    func.name
                );
            };
            invocations.insert(
                Arc::from(func.name),
                LinkedExportInvocation {
                    import_name: Arc::from(interface),
                    export_name: Arc::from(func.name),
                    plugin_component_id: Arc::clone(&component_id),
                    func_idx,
                    param_tys: Arc::default(),
                    target: LinkedTarget::Ephemeral(Arc::new(EphemeralLinkedCall {
                        pre: pre.clone(),
                        invocation: self.invocation.clone(),
                        engine: engine.clone(),
                        stream_payloads: self.stream_payloads.clone(),
                        http_handler: self.http_handler.clone(),
                        components: Arc::downgrade(&self.components),
                        active_component_id: Arc::clone(&component_id),
                        linked_component_ids: linked_component_ids.clone(),
                        #[cfg(feature = "wasi-tls")]
                        tls_provider: self.tls_provider.clone(),
                        mode,
                    })),
                },
            );
        }
        Ok(invocations)
    }

    /// [`Self::external_export_invocations`] for the workload's service.
    ///
    /// The service already runs; a call reaches it over the ingress `claim`
    /// holds open, on the instance driving `cli/run`, so there is no store to
    /// build and no `InstancePre` to instantiate.
    ///
    /// The caller brings the claim rather than one being taken here, because the
    /// claim is what makes the service run with an ingress at all: a caller that
    /// resolves these invocations and then discards them — a plugin routing to a
    /// component instead — must be able to take that back.
    ///
    /// Every argument and result crosses as a relocated value, so the same
    /// signatures a component may serve are exactly the ones a service may:
    /// plain values, `stream<T>` and `future<T>`, and nothing carrying a
    /// `resource` handle.
    pub(crate) fn service_export_invocations(
        &self,
        claim: &ServiceClaim,
        interface: &str,
        funcs: &[ExternalCallFunc<'_>],
    ) -> anyhow::Result<BTreeMap<Arc<str>, LinkedExportInvocation>> {
        // No `service_can_dispatch` check here: holding a claim is proof of it,
        // since `claim_service_dispatch` refuses to mint one otherwise.
        let service = self
            .service
            .as_ref()
            .context("workload has no service to call into")?;
        let component = &service.metadata.component;
        let payloads = &self.stream_payloads;
        let Some((ComponentItem::ComponentInstance(_), instance_idx)) =
            component.get_export(None, interface)
        else {
            bail!("the workload's service does not export {interface}");
        };
        let service_id: Arc<str> = Arc::from(service.id());
        let service_identity =
            crate::observability::WorkloadIdentity::new(self.namespace(), self.name(), "service");

        let mut invocations = BTreeMap::new();
        for func in funcs {
            let Some((ComponentItem::ComponentFunc(_), func_idx)) =
                component.get_export(Some(&instance_idx), func.name)
            else {
                bail!(
                    "the workload's service exports {interface} but not {}, which a host \
                     component plugin imports; the workload's copy of the interface disagrees \
                     with the plugin's",
                    func.name
                );
            };
            ensure!(
                types_are_bridge_safe(func.param_tys, payloads)
                    && types_are_bridge_safe(func.result_tys, payloads),
                "{interface}#{} carries a handle that cannot cross the boundary between a \
                 plugin's store and a workload's; only plain values, `stream<T>`, and \
                 `future<T>` can; a `stream<T>`/`future<T>` of an \
                 application-defined payload needs `Engine::stream_payloads().register`",
                func.name
            );
            let plain = types_are_ephemeral_safe(func.param_tys)
                && types_are_ephemeral_safe(func.result_tys);
            invocations.insert(
                Arc::from(func.name),
                LinkedExportInvocation {
                    import_name: Arc::from(interface),
                    export_name: Arc::from(func.name),
                    plugin_component_id: Arc::clone(&service_id),
                    func_idx,
                    param_tys: Arc::default(),
                    target: LinkedTarget::Service(Arc::new(ServiceExportCall {
                        calls: Arc::clone(claim.calls()),
                        param_tys: func.param_tys.into(),
                        result_tys: func.result_tys.into(),
                        plain,
                        attributes: service_identity
                            .attributes("linked", &format!("{interface}#{}", func.name)),
                    })),
                },
            );
        }
        Ok(invocations)
    }
}
