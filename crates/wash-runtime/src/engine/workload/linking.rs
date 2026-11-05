//! Linking a workload's components to each other:
//! ordering them so exporters come first,
//! resolving each import against the workload's exports,
//! sealing linkers, and pre-instantiating.

use super::*;

/// Whether an import is left for the linker to wire to a sibling component:
/// it is not one of the [`WorkloadMetadata::plugin_bound_instances`]
/// a plugin already defined in the linker during bind.
pub(super) fn should_link_component_import(
    import_name: &str,
    plugin_bound_instances: &HashSet<String>,
) -> bool {
    !plugin_bound_instances.contains(import_name)
}

/// Topologically sort components (Kahn's algorithm) so exporters come before
/// importers, a component's linker must be fully configured before anything
/// that imports from it is pre-instantiated. Errors on a dependency cycle.
pub(super) fn topological_sort_components(
    dependencies: &HashMap<Arc<str>, HashSet<Arc<str>>>,
) -> anyhow::Result<Vec<Arc<str>>> {
    // Build in-degree map: count how many dependencies each component has
    // (only counting dependencies on other components within this workload)
    let mut in_degree: HashMap<Arc<str>, usize> = HashMap::new();

    for (component_id, deps) in dependencies {
        // Count only dependencies that are part of this workload
        let dep_count = deps
            .iter()
            .filter(|d| dependencies.contains_key(*d))
            .count();
        in_degree.insert(component_id.clone(), dep_count);
    }

    // Start with components that have no dependencies (in-degree == 0)
    // Sort for deterministic ordering
    let mut queue: Vec<Arc<str>> = in_degree
        .iter()
        .filter(|&(_, degree)| *degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    queue.sort();

    let mut result = Vec::with_capacity(dependencies.len());

    while let Some(component_id) = queue.pop() {
        result.push(component_id.clone());

        // Find components that depend on this one and decrease their in-degree
        for (other_id, deps) in dependencies {
            if deps.contains(&component_id)
                && let Some(degree) = in_degree.get_mut(other_id)
            {
                *degree = degree.saturating_sub(1);
                if *degree == 0 && !result.contains(other_id) {
                    queue.push(other_id.clone());
                    // Re-sort to maintain determinism
                    queue.sort();
                }
            }
        }
    }

    // Check for circular dependencies
    if result.len() != dependencies.len() {
        let unprocessed: Vec<_> = dependencies
            .keys()
            .filter(|id| !result.contains(id))
            .map(|id| id.as_ref())
            .collect();
        bail!("circular dependency detected among components: {unprocessed:?}");
    }

    Ok(result)
}

/// Expand `direct_links` into the full transitive closure. One hop suffices
/// because components are processed in topological order, so each
/// `resolved_links[dep]` already holds that dep's complete closure.
pub(super) fn expand_link_closure(
    direct_links: &HashSet<Arc<str>>,
    resolved_links: &HashMap<Arc<str>, HashSet<Arc<str>>>,
) -> HashSet<Arc<str>> {
    let mut linked_components = direct_links.clone();
    for linked_component_id in direct_links {
        if let Some(nested_links) = resolved_links.get(linked_component_id) {
            linked_components.extend(nested_links.iter().cloned());
        }
    }
    linked_components
}

impl ResolvedWorkload {
    pub(super) async fn resolve_component_volume_mounts(
        &self,
        component_ids: &[Arc<str>],
    ) -> anyhow::Result<()> {
        resolve_component_volume_mounts_in_map(&self.components, component_ids).await
    }

    pub(super) async fn resolve_service_volume_mounts(&mut self) -> anyhow::Result<()> {
        let linked_component_ids = self
            .service
            .as_ref()
            .map(|service| {
                service
                    .metadata
                    .linked_components
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.resolve_component_volume_mounts(&linked_component_ids)
            .await?;
        if let Some(service) = self.service_mut()
            && service.metadata.resolved_volume_mounts.is_empty()
            && !service.metadata.volume_mounts.is_empty()
        {
            service.metadata.resolve_volume_mounts().await?;
        }
        Ok(())
    }

    pub(super) fn component_ctx_template(
        &self,
        metadata: &WorkloadMetadata,
    ) -> ComponentCtxTemplate {
        #[cfg(feature = "wasi-tls")]
        {
            crate::engine::linked_call::component_ctx_template_from_metadata_with_tls(
                metadata,
                self.tls_provider.clone(),
            )
        }
        #[cfg(not(feature = "wasi-tls"))]
        {
            crate::engine::linked_call::component_ctx_template_from_metadata(metadata)
        }
    }

    #[instrument(name="link_components", skip_all, fields(workload.id = self.id.as_ref(), workload.name = self.name.as_ref(), workload.namespace = self.namespace.as_ref()))]
    pub(super) async fn link_components(&mut self) -> anyhow::Result<()> {
        // Collect each component's exported component-instance interfaces.
        let mut component_exports: Vec<(ComponentExporter, Vec<String>)> = Vec::new();
        for c in self.components.read().await.values() {
            let mut names = Vec::new();
            for (name, item) in c.component_exports()? {
                // TODO(#11): It's probably a good idea to skip registering wasi@0.2 interfaces
                if name == "wasmcloud:wash/plugin" || name.starts_with("wasmcloud:wash/plugin@") {
                    trace!(name, "skipping internal plugin export");
                    continue;
                }
                if let ComponentItem::ComponentInstance(_) = item {
                    names.push(name);
                } else {
                    warn!(name, "exported item is not a component instance, skipping");
                }
            }
            component_exports.push((
                ComponentExporter {
                    name: Arc::from(c.name()),
                    id: Arc::from(c.id()),
                },
                names,
            ));
        }

        // Build the export→component map for intra-workload linking. An
        // interface exported by more than one component is left out of the map
        // and tracked as ambiguous; it only causes an error if a component
        // actually imports it (host-invoked exports such as
        // `wasmcloud:messaging/handler` or `wasi:http/incoming-handler` are
        // consumed by host plugins, never imported intra-workload, so multiple
        // exporters are fine).
        let exports = build_export_map(&component_exports);

        self.resolve_workload_imports(&exports).await?;

        Ok(())
    }

    /// This function plugs a components imports with the exports of other components
    /// that are already loaded in the plugin system.
    ///
    /// Components are processed in topological order based on their inter-component
    /// dependencies. This ensures that when a component imports from another component,
    /// the exporting component has already had its imports resolved and can be
    /// pre-instantiated.
    pub(super) async fn resolve_workload_imports(
        &mut self,
        exports: &ExportMaps,
    ) -> anyhow::Result<()> {
        // Build a dependency graph: for each component, track which other components it imports from
        let mut dependencies: HashMap<Arc<str>, HashSet<Arc<str>>> = HashMap::new();

        {
            let components = self.components.read().await;
            for (component_id, component) in components.iter() {
                let mut deps = HashSet::new();
                let ty = component.metadata.component.component_type();
                for (import_name, import_item) in ty.imports(component.metadata.component.engine())
                {
                    if !matches!(import_item.ty, ComponentItem::ComponentInstance(_)) {
                        continue;
                    }
                    let (interface, label) = resolve_import(import_name, import_item.implements);
                    match label {
                        // For labeled imports without a labeled exporter, we
                        // record no dependency, as resolution will happen
                        // against the host.
                        //
                        // If the import is labeled and has a matching
                        // exporter, then we must record the exporter as a
                        // dependency.
                        Some(l) => {
                            // A label a plugin already bound is the host's (see
                            // `served_within_workload`), so it is not an
                            // intra-workload dependency. `bind_plugins` has run
                            // by now, so this is the same answer the link step
                            // reaches through `should_link_component_import`;
                            // recording an edge here anyway would order the
                            // graph around a link that never happens, and can
                            // manufacture a false circular-dependency bail.
                            if should_link_component_import(
                                import_name,
                                &component.metadata.plugin_bound_instances,
                            ) && let Some((labelled, _)) =
                                labelled_exporter(&exports.exporters, interface, l)?
                                && labelled != component_id
                            {
                                deps.insert(labelled.clone());
                            }
                        }
                        None => {
                            // An interface exported by multiple components can't be
                            // resolved to a single provider for an intra-workload
                            // import: that's the genuine ambiguity the link check
                            // guards against.
                            if exports.ambiguous.contains(interface) {
                                anyhow::bail!(
                                    "component '{component_id}' imports interface \
                                     '{interface}', which is exported by multiple \
                                     components in the workload; cannot disambiguate the \
                                     provider. Import it under an `(implements ..)` label \
                                     naming the component to use."
                                );
                            }
                            if let Some(exporter_id) = exports.unambiguous.get(interface)
                                && exporter_id != component_id
                            {
                                // This import is provided by another component in the workload
                                deps.insert(exporter_id.clone());
                            }
                        }
                    }
                }
                dependencies.insert(component_id.clone(), deps);
            }
        }

        // Topologically sort components: components with no dependencies (or dependencies
        // already processed) come first. This ensures that when we process a component
        // that imports from another component, the exporter has already been resolved.
        let sorted_component_ids = anyhow::Context::context(
            topological_sort_components(&dependencies),
            "failed to determine component processing order - possible circular dependency",
        )?;

        trace!(
            order = ?sorted_component_ids.iter().map(|id| id.as_ref()).collect::<Vec<_>>(),
            "processing components in topological order"
        );

        let mut resolved_links: HashMap<Arc<str>, HashSet<Arc<str>>> = HashMap::new();

        for component_id in sorted_component_ids {
            // In order to have mutable access to both the workload component and components that need
            // to be instantiated as "plugins" during linking, we remove and re-add the component to the list.
            let mut workload_component = {
                self.components
                    .write()
                    .await
                    .remove(&component_id)
                    .context("component not found during import resolution")?
            };

            let component = workload_component.metadata.component.clone();
            let plugin_bound_instances = workload_component.metadata.plugin_bound_instances.clone();
            let res = self
                .resolve_component_imports(
                    &component,
                    &mut workload_component.metadata,
                    exports,
                    &plugin_bound_instances,
                    Some(component_id.as_ref()),
                )
                .await
                .map(|direct_links| {
                    workload_component.linked_components =
                        expand_link_closure(&direct_links, &resolved_links);
                });

            // Put the component back before propagating `res`:
            // teardown walks `self.components`,
            // so a component dropped here on error would never be released.
            let linked_components = workload_component.linked_components.clone();
            let workload_component_id = workload_component.metadata.id.clone();

            self.components
                .write()
                .await
                .insert(workload_component.metadata.id.clone(), workload_component);
            resolved_links.insert(workload_component_id, linked_components);
            // Propagate any errors encountered during import resolution
            res?;
        }

        if let Some(mut shared) = self.service.take() {
            let service = Arc::make_mut(&mut shared);
            let component = service.metadata.component.clone();
            let plugin_bound_instances = service.metadata.plugin_bound_instances.clone();
            let res = self
                .resolve_component_imports(
                    &component,
                    &mut service.metadata,
                    exports,
                    &plugin_bound_instances,
                    None,
                )
                .await
                .map(|direct_links| {
                    service.metadata.linked_components =
                        expand_link_closure(&direct_links, &resolved_links);
                });

            // As above: restore the service before propagating `res`, so teardown still sees it.
            self.service = Some(shared);

            // Propagate any errors encountered during import resolution
            res?;
        }

        Ok(())
    }

    pub(super) async fn resolve_component_imports(
        &self,
        component: &wasmtime::component::Component,
        // Its linker is taken mutably only to link an import another component
        // exports: taking it copies the engine's shared one.
        metadata: &mut WorkloadMetadata,
        exports: &ExportMaps,
        plugin_bound_instances: &HashSet<String>,
        // The importer's own id. A component resolving an import to ITSELF is a
        // no-op rather than a lookup failure: it has already been removed from
        // the component map by the time the exporter is fetched below, so the
        // fetch would `bail!` with a UUID in it. This applies to the unlabelled
        // path too, where it turns that hard deploy failure into a silent skip
        // to host - which is what the unlabelled case meant all along.
        importer_id: Option<&str>,
    ) -> anyhow::Result<HashSet<Arc<str>>> {
        let mut linked_components = HashSet::new();
        let ty = component.component_type();
        let imports: Vec<_> = ty.imports(component.engine()).collect();
        // The cross-store stream bridge engages only for a p3-service workload:
        // there a long-lived service store must never be pinned/frozen by a
        // stream-carrying backend call, so such calls run ephemerally (relocated)
        // instead of on the shared store. In service-less / per-request workloads
        // stream-carrying calls stay on the shared store.
        let is_service_workload = self
            .service
            .as_ref()
            .is_some_and(|s| s.metadata.targets_p3());

        for (import_name, import_item) in imports.into_iter() {
            match import_item.ty {
                ComponentItem::ComponentInstance(import_instance_ty) => {
                    trace!(name = import_name, "processing component instance import");
                    if !should_link_component_import(import_name, plugin_bound_instances) {
                        trace!(
                            name = import_name,
                            "host plugin already bound import, skipping"
                        );
                        continue;
                    }
                    let mut all_components = self.components.write().await;
                    let (
                        plugin_component,
                        instance_idx,
                        plugin_component_id,
                        nested_linked_component_ids,
                        plugin_engine,
                    ) = {
                        let (interface, label) =
                            resolve_import(import_name, import_item.implements);
                        // A labelled import resolves ONLY through its label. It
                        // must not fall back to the unlabelled single-exporter
                        // route: a label that names no component here is one the
                        // operator meant for a host interface, and quietly
                        // handing it a guest component instead is the kind of
                        // wrong that deploys.
                        let resolved = match label {
                            Some(l) => labelled_exporter(&exports.exporters, interface, l)?
                                .map(|(id, export_name)| (id.clone(), export_name.to_string())),
                            None => exports
                                .unambiguous
                                .get(interface)
                                .map(|id| (id.clone(), interface.to_string())),
                        }
                        .filter(|(exporter, _)| Some(exporter.as_ref()) != importer_id);
                        let Some((exporter_component, export_name)) = resolved else {
                            // Import not provided by another component in the workload.
                            // This is expected for host-provided interfaces (e.g. wasi:*).
                            // If it's not host-provided, linking will fail later with a
                            // clear error from wasmtime.
                            if label.is_some() {
                                // A label naming nothing is the case that deploys and then
                                // fails at instantiation - the failure this routing exists
                                // to remove - so it is worth more than a debug line. Log
                                // the interface too: the label alone is the one string
                                // that does not say what was wanted.
                                warn!(
                                    label = import_name,
                                    interface,
                                    "`(implements ..)` label names no component in this \
                                     workload exporting the interface; assuming \
                                     host-provided"
                                );
                            } else {
                                debug!(
                                    name = import_name,
                                    interface,
                                    "import not found in component exports, assuming host-provided"
                                );
                            }
                            continue;
                        };
                        let nested_linked_component_ids = {
                            let Some(exporter_workload_component) =
                                all_components.get(&exporter_component)
                            else {
                                anyhow::bail!(
                                    "exporting component '{exporter_component}' for import '{import_name}' not found"
                                );
                            };
                            exporter_workload_component
                                .metadata
                                .linked_components
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                        };
                        let Some(plugin_component) = all_components.get_mut(&exporter_component)
                        else {
                            anyhow::bail!(
                                "exporting component '{exporter_component}' for import '{import_name}' not found"
                            );
                        };
                        let plugin_component_id = plugin_component.id.clone();
                        let plugin_engine = plugin_component.metadata.engine().clone();
                        // The exporter uses its own interface name. The importer
                        // linker uses the raw import label.
                        let Some((ComponentItem::ComponentInstance(_), idx)) = plugin_component
                            .metadata
                            .component
                            .get_export(None, &export_name)
                        else {
                            trace!(
                                name = import_name,
                                interface, "skipping non-instance import"
                            );
                            continue;
                        };
                        (
                            plugin_component,
                            idx,
                            plugin_component_id,
                            nested_linked_component_ids,
                            plugin_engine,
                        )
                    };
                    trace!(name = import_name, index = ?instance_idx, "found import at index");

                    // Preinstantiate the plugin instance so we can use it later
                    let pre = plugin_component.pre_instantiate().map_err(|e| {
                        e.context("failed to pre-instantiate during component linking")
                    })?;

                    let mut linker_instance = match metadata.linker()?.instance(import_name) {
                        Ok(i) => i,
                        Err(e) => {
                            trace!(name = import_name, error = %e, "error finding instance in linker, skipping");
                            continue;
                        }
                    };

                    for (export_name, export_ty) in
                        import_instance_ty.exports(plugin_component.metadata.component.engine())
                    {
                        match export_ty.ty {
                            ComponentItem::ComponentFunc(func_ty) => {
                                let (item, func_idx) = match plugin_component
                                    .metadata
                                    .component
                                    .get_export(Some(&instance_idx), export_name)
                                {
                                    Some(res) => res,
                                    None => {
                                        trace!(
                                            name = import_name,
                                            fn_name = export_name,
                                            "failed to get export index, skipping"
                                        );
                                        continue;
                                    }
                                };
                                ensure!(
                                    matches!(item, ComponentItem::ComponentFunc(..)),
                                    "expected function export, found other"
                                );
                                trace!(
                                    name = import_name,
                                    fn_name = export_name,
                                    "linking function import"
                                );
                                let export_is_async = func_ty.async_();
                                // Plain-value async calls always run ephemerally
                                // (params copied). In a p3-service workload, a call
                                // carrying only relocatable `stream<T>` handles also
                                // runs ephemerally — its args/results are relocated
                                // (see `relocate`) rather than copied — so a
                                // stream-carrying backend call can't pin or freeze
                                // the service store.
                                let plain_safe = func_is_ephemeral_safe(&func_ty);
                                let relocate = !plain_safe
                                    && is_service_workload
                                    && func_is_bridge_safe(&func_ty, &self.stream_payloads);
                                let target = if export_is_async && (plain_safe || relocate) {
                                    let mode = if relocate {
                                        EphemeralCallMode::Relocated {
                                            param_tys: func_ty.params().map(|(_, ty)| ty).collect(),
                                            result_tys: func_ty.results().collect(),
                                        }
                                    } else {
                                        EphemeralCallMode::PlainValue
                                    };
                                    LinkedTarget::Ephemeral(Arc::new(EphemeralLinkedCall {
                                        pre: pre.clone(),
                                        invocation: self.invocation.clone(),
                                        engine: plugin_engine.clone(),
                                        stream_payloads: self.stream_payloads.clone(),
                                        http_handler: self.http_handler.clone(),
                                        components: Arc::downgrade(&self.components),
                                        active_component_id: plugin_component_id.clone(),
                                        linked_component_ids: nested_linked_component_ids.clone(),
                                        #[cfg(feature = "wasi-tls")]
                                        tls_provider: self.tls_provider.clone(),
                                        mode,
                                    }))
                                } else {
                                    LinkedTarget::SharedStore
                                };

                                let inv = LinkedExportInvocation {
                                    import_name: import_name.into(),
                                    export_name: export_name.into(),
                                    plugin_component_id: plugin_component.id.clone(),
                                    func_idx,
                                    param_tys: Arc::default(),
                                    target,
                                };

                                linked_components.insert(inv.plugin_component_id.clone());

                                let export_name = inv.export_name.clone();
                                if export_is_async {
                                    linker_instance
                                        .func_new_concurrent(
                                            export_name.as_ref(),
                                            move |accessor, _func_ty, params, results| {
                                                let inv = inv.clone();
                                                Box::pin(async move {
                                                    invoke_linked_async_export(
                                                        accessor, params, results, &inv,
                                                    )
                                                    .await
                                                })
                                            },
                                        )
                                        .map_err(|e| {
                                            e.context("failed to create concurrent func")
                                        })?;
                                } else {
                                    linker_instance
                                        .func_new_async(
                                            export_name.as_ref(),
                                            move |store, _func_ty, params, results| {
                                                let inv = inv.clone();
                                                Box::new(async move {
                                                    invoke_linked_sync_export(
                                                        store, params, results, &inv,
                                                    )
                                                    .await
                                                })
                                            },
                                        )
                                        .map_err(|e| {
                                            e.context("failed to wrap sync func in async func")
                                        })?;
                                }
                            }
                            ComponentItem::Resource(resource_ty) => {
                                let (item, _idx) = match plugin_component
                                    .metadata
                                    .component
                                    .get_export(Some(&instance_idx), export_name)
                                {
                                    Some(res) => res,
                                    None => {
                                        trace!(
                                            name = import_name,
                                            resource = export_name,
                                            "failed to get resource index, skipping"
                                        );
                                        continue;
                                    }
                                };
                                let ComponentItem::Resource(_) = item else {
                                    trace!(
                                        name = import_name,
                                        resource = export_name,
                                        "expected resource export, found non-resource, skipping"
                                    );
                                    continue;
                                };

                                // TODO: This should be a comparison of the ComponentItem to the
                                // host resource type, but for some reason the comparison fails.
                                if export_name == "output-stream"
                                    || export_name == "input-stream"
                                    || export_name == "pollable"
                                    || export_name == "tcp-socket"
                                    || export_name == "incoming-value-async-body"
                                {
                                    trace!(
                                        name = import_name,
                                        resource = export_name,
                                        "skipping stream link as it is a host resource type"
                                    );
                                    continue;
                                }

                                trace!(name = import_name, resource = export_name, ty = ?resource_ty, "linking resource import");

                                linker_instance
                                        .resource(export_name, ResourceType::host::<ResourceAny>(), |_, _| Ok(()))
                                        .map_err(|e| {
                                            e.context(format!(
                                                "failed to define resource import: {import_name}.{export_name}"
                                            ))
                                        })
                                        .unwrap_or_else(|e| {
                                            trace!(name = import_name, resource = export_name, error = %e, "error defining resource import, skipping");
                                        });
                            }
                            _ => {
                                trace!(
                                    name = import_name,
                                    fn_name = export_name,
                                    "skipping non-function non-resource import"
                                );
                                continue;
                            }
                        }
                    }
                }
                ComponentItem::Resource(resource_ty) => {
                    trace!(
                        name = import_name,
                        ty = ?resource_ty,
                        "component import is a resource, which is not supported in this context. skipping."
                    );
                }
                _ => continue,
            }
        }

        Ok(linked_components)
    }

    /// Seal component and service linkers after their imports are resolved
    /// (see [`WorkloadMetadata::seal_linker`]), then point `cleanup` at the
    /// sealed workload. Components share their lock with the snapshot, but
    /// linking may have copied the service, so the snapshot taken before
    /// would otherwise keep the old copy's open linker.
    pub(super) async fn seal_linkers(&mut self, cleanup: Option<&WorkloadResources>) {
        let mut components = self.components.write().await;
        let sealed = components
            .values_mut()
            .map(|component| component.metadata.seal_linker())
            .filter(|sealed| *sealed)
            .count();
        trace!(sealed, total = components.len(), "sealed component linkers");
        drop(components);
        if let Some(service) = &mut self.service {
            let sealed = Arc::make_mut(service).metadata.seal_linker();
            trace!(sealed, "sealed service linker");
        }
        if let Some(cleanup) = cleanup {
            cleanup.resolved(self);
        }
    }

    /// The kept link for `component_id`, ready to instantiate into a store
    /// the caller owns.
    ///
    /// Only a *component* has one: the workload's service is already
    /// instantiated, so a plugin that would call into either goes through
    /// [`Self::dispatch_target`], which covers both and keeps warm instances in
    /// play.
    pub async fn instantiate_pre(
        &self,
        component_id: &str,
    ) -> anyhow::Result<wasmtime::component::InstancePre<SharedCtx>> {
        // A read lock: the link is kept, so reading it needs only `&self`,
        // and a write lock here would serialize every request and dispatch.
        let components = self.components.read().await;
        let component = components
            .get(component_id)
            .context("component ID not found in workload")?;
        Ok(component.pre_instantiate_ref()?)
    }
}
