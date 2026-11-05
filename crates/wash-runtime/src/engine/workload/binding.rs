//! Binding host plugins to a workload:
//! matching each item's imports to the plugin that serves them,
//! after whatever the workload serves itself.

use super::*;

/// Type alias for tracking bound plugins with their matched interfaces during binding.
/// Tuple: (plugin, matched_interfaces, component_ids)
pub(super) type BoundPluginWithInterfaces = (
    Arc<dyn HostPlugin + 'static>,
    HashSet<WitInterface>,
    Vec<String>,
);

/// The names a plugin bound for `interface` occupies in a component's linker,
/// spelled exactly as the linker sees its imports:
/// the `(implements ..)` label when the entry has one,
/// otherwise one `ns:pkg/iface[@version]` per interface.
///
/// Recorded during bind in [`WorkloadMetadata::plugin_bound_instances`]
/// and read back by [`should_link_component_import`](super::linking::should_link_component_import),
/// so linking skips the imports a plugin already serves.
/// The spelling here and the linker's import names must match,
/// or linking would try to define an instance twice.
pub(super) fn plugin_bound_instance_names(interface: &WitInterface) -> Vec<String> {
    if let Some(name) = &interface.name {
        return vec![name.clone()];
    }
    if interface.interfaces.is_empty() {
        return vec![interface.instance()];
    }
    interface
        .interfaces
        .iter()
        .map(|name| {
            let base = format!("{}:{}/{name}", interface.namespace, interface.package);
            match &interface.version {
                Some(version) => format!("{base}@{version}"),
                None => base,
            }
        })
        .collect()
}

/// Whether `world` exports an interface this entry names.
pub(super) fn exports_any_of(entry: &WitInterface, world: &WitWorld) -> bool {
    entry.interfaces.iter().any(|interface| {
        world
            .exports
            .iter()
            .any(|ex| entry.same_package(ex) && ex.interfaces.contains(interface))
    })
}

/// Whether `world` imports an interface this entry names, under `label`.
pub(super) fn imports_under_label(entry: &WitInterface, world: &WitWorld, label: &str) -> bool {
    world.imports.iter().any(|im| {
        im.name.as_deref() == Some(label)
            && entry.same_package(im)
            && entry.interfaces.iter().any(|i| im.interfaces.contains(i))
    })
}

/// Whether the workload answers `entry` for `item_id` out of its own
/// components, so the host is never asked for it: `item_id` imports every one of
/// the entry's interfaces, and each is exported by exactly one *other* component
/// of the same workload.
///
/// Such an import is wired component-to-component by
/// [`ResolvedWorkload::link_components`]. Keeping it away from plugin matching
/// is what makes the workload's own component win, because whoever matches first
/// installs the shim: plugins bind before components are linked, and a plugin
/// that claimed the interface leaves the intra-workload link with nowhere to go
/// — the linker instance is already defined, the link is skipped, and calls
/// silently leave the workload for the plugin.
///
/// Only the *importing* side is filtered. A component that exports the interface
/// keeps the entry, because an export is also how the host reaches into a
/// workload — a `wasi:http` handler, a messaging handler, an interface a host
/// component plugin calls — and where an import points is a separate question
/// from whether the host may call an export.
///
/// Exactly one other exporter, OR several where the importing component names
/// one of them in an `(implements ..)` label. Two unlabelled exporters are the
/// ambiguity [`build_export_map`] declines to resolve, and leaving those to a
/// plugin keeps a workload that deploys today deploying.
///
/// A label the operator declared under `hostInterfaces` is the exception to
/// that exception: it belongs to the host even when a component shares the
/// name. Declaring the label is an explicit statement about routing; a
/// component happening to be named the same thing is not. Without this a
/// component would outrank a declared label and capture traffic the manifest
/// asked the host to serve, which is both a regression against what deploys
/// today and a way for a guest to answer for the host. Zero exporters is
/// unchanged.
pub(super) fn served_within_workload(
    entry: &WitInterface,
    item_id: &str,
    item_world: &WitWorld,
    component_worlds: &[(Arc<str>, Arc<str>, WitWorld)],
) -> bool {
    // An entry naming no interface matches every interface of its package, so
    // there is nothing specific to have found a provider for.
    if entry.interfaces.is_empty() {
        return false;
    }
    // An item exporting any of the entry's interfaces keeps the entry, so the
    // host can reach that export.
    if exports_any_of(entry, item_world) {
        return false;
    }
    // Of the names left, only the ones this item imports are its to answer: the
    // entry covers what the whole workload uses (see [`WitWorld::uses`]).
    let imported = entry.interfaces.iter().filter(|interface| {
        item_world
            .imports
            .iter()
            .any(|im| entry.same_package(im) && im.interfaces.contains(*interface))
    });

    let mut imports_any = false;
    for interface in imported {
        imports_any = true;
        // Names, not ids: the label is written against the manifest name while
        // the map is keyed by a per-component UUID. Comparing a label to an id
        // is never true, which is what made the first version of this inert.
        let exporter_names: Vec<&str> = component_worlds
            .iter()
            .filter(|(id, _, world)| {
                id.as_ref() != item_id
                    && world
                        .exports
                        .iter()
                        .any(|ex| entry.same_package(ex) && ex.interfaces.contains(interface))
            })
            .map(|(_, name, _)| name.as_ref())
            .collect();
        // EVERY import of this interface has to be answered inside the
        // workload, not merely one of them. This function returns one verdict
        // per ENTRY, while an item may import the same interface more than
        // once: a label naming a sibling alongside a label meant for a host
        // plugin. Asking whether *any* import is served in-workload lets the
        // sibling label speak for the plugin label too, the entry is dropped
        // from `unmatched_interfaces`, the plugin never binds, and the plugin
        // label is left with no provider anywhere.
        //
        // A labelled import is answered only by a label naming an exporter; an
        // unlabelled one only by a single unambiguous exporter, since several
        // is the ambiguity `build_export_map` declines to resolve. Anything
        // else still needs the host, so the entry stays available to plugins.
        //
        // Honouring the label here as well as at link time is what keeps a
        // plugin from claiming an interface a sibling is meant to serve: the
        // linker instance would already be defined, the link would be skipped,
        // and the calls would quietly leave the workload.
        let every_import_answered = item_world
            .imports
            .iter()
            .filter(|im| entry.same_package(im) && im.interfaces.contains(interface))
            .all(|im| match im.name.as_deref() {
                // A label the operator declared under `hostInterfaces` belongs
                // to the host, even when a component happens to share the name.
                // Declaring the label is an explicit statement about routing;
                // a component being named the same thing is not. Without this,
                // the component silently captures traffic the manifest asked
                // the host to serve, and the manifest still reads as correct.
                //
                // Returning false here keeps the entry in
                // `unmatched_interfaces`, so a plugin binds it and the label
                // lands in `plugin_bound_instances`, which is what makes
                // `should_link_component_import` decline to link the sibling.
                Some(label) if entry.name.as_deref() == Some(label) => false,
                Some(label) => exporter_names.contains(&label),
                None => exporter_names.len() == 1,
            });
        if !every_import_answered {
            return false;
        }
    }
    imports_any
}

/// Whether the host links everything `world` uses of an unnamed WASI base
/// `entry` itself. Judged by the versions and labels `world` actually uses,
/// since an entry may pin a different compatible version or none, and the host
/// only defines the plain instance.
pub(super) fn served_by_host(entry: &WitInterface, world: &WitWorld) -> bool {
    entry.name.is_none()
        && entry.is_host_builtin()
        && world
            .imports
            .iter()
            .chain(&world.exports)
            .filter(|used| {
                entry.same_package(used)
                    && (entry.interfaces.is_empty()
                        || entry.interfaces.iter().any(|i| used.interfaces.contains(i)))
            })
            .all(|used| used.name.is_none() && used.is_host_builtin())
}

/// Returns whether some *other* registered plugin (not `self_id`) can take
/// `iface` off this one's hands: with `want_named`, one that supports named
/// instances; without, one that serves plain imports itself rather than
/// deferring them ([`HostPlugin::defers_unnamed_instances`]).
///
/// A plugin that would refuse `iface` via [`HostPlugin::claims`] is not a
/// deferral target: deferring to it would leave the interface bound by nobody.
pub(super) fn other_plugin_serves(
    plugins: &HashMap<&'static str, Arc<dyn HostPlugin + 'static>>,
    self_id: &str,
    iface: &WitInterface,
    want_named: bool,
) -> bool {
    plugins.iter().any(|(id, q)| {
        *id != self_id
            && (if want_named {
                q.supports_named_instances()
            } else {
                !q.defers_unnamed_instances()
            })
            && q.world().includes_bidirectional(iface)
            && q.claims(iface)
    })
}

/// The plugin whose `host.plugins` entry declares `label` and that serves
/// `iface`, if any — lowest id first, so the answer is stable. An operator's
/// declaration is the routing key: it beats id order and whatever the plugins
/// report for named-instance support.
pub(super) fn declaring_plugin(
    plugins: &HashMap<&'static str, Arc<dyn HostPlugin + 'static>>,
    plugin_bindings: &crate::plugin::PluginBindings,
    label: &str,
    iface: &WitInterface,
) -> Option<&'static str> {
    plugin_bindings.plugin_ids().find_map(|id| {
        let (id, q) = plugins.get_key_value(id)?;
        let declares = plugin_bindings
            .for_plugin(id)
            .binding_names()
            .any(|name| name == label);
        (declares && q.world().includes_bidirectional(iface) && q.claims(iface)).then_some(*id)
    })
}

// Helper enum to differentiate between component and service IDs
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(super) enum IdFlavor {
    Component(Arc<str>),
    Service(Arc<str>),
}

impl AsRef<str> for IdFlavor {
    fn as_ref(&self) -> &str {
        match self {
            IdFlavor::Component(id) => id.as_ref(),
            IdFlavor::Service(id) => id.as_ref(),
        }
    }
}

impl std::fmt::Display for IdFlavor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdFlavor::Component(id) => write!(f, "Component({id})"),
            IdFlavor::Service(id) => write!(f, "Service({id})"),
        }
    }
}

impl UnresolvedWorkload {
    /// Bind this workload to the host plugins based on the requested
    /// interfaces. Returns a list of plugins and the component IDs they were bound to.
    #[allow(clippy::type_complexity)]
    pub async fn bind_plugins(
        &mut self,
        plugins: &HashMap<&'static str, Arc<dyn HostPlugin + 'static>>,
        plugin_bindings: &crate::plugin::PluginBindings,
    ) -> anyhow::Result<Vec<(Arc<dyn HostPlugin + 'static>, Vec<String>)>> {
        self.bind_plugins_for_start(plugins, plugin_bindings, None)
            .await
    }

    #[allow(clippy::type_complexity)]
    #[instrument(name = "bind_plugins", skip_all)]
    pub(super) async fn bind_plugins_for_start(
        &mut self,
        plugins: &HashMap<&'static str, Arc<dyn HostPlugin>>,
        plugin_bindings: &crate::plugin::PluginBindings,
        cleanup: Option<&WorkloadResources>,
    ) -> anyhow::Result<Vec<(Arc<dyn HostPlugin>, Vec<String>)>> {
        // Before anything binds, so there is nothing to unbind: two plugins
        // overriding one WASI interface is a host misconfiguration.
        crate::plugin::check_wasi_overrides(plugins)?;
        // Track bound plugins with their matched interfaces for cleanup on failure
        let mut bound_plugins_with_interfaces: Vec<BoundPluginWithInterfaces> = Vec::new();
        let mut bound_plugins: Vec<(Arc<dyn HostPlugin + 'static>, Vec<String>)> = Vec::new();

        // Collect all component's required (unmatched) host interfaces
        // This tracks which interfaces each component still needs to be bound
        let mut unmatched_interfaces: HashMap<IdFlavor, HashSet<WitInterface>> = HashMap::new();
        let host_interfaces = {
            // filter out Plugins fulfilled by host. `handler` is the unified
            // WASI P3 http interface (covers both incoming and outgoing);
            // `incoming-handler`/`outgoing-handler` are its P2 equivalents.
            let http_iface =
                WitInterface::from("wasi:http/incoming-handler,outgoing-handler,handler");
            self.host_interfaces
                .iter()
                .filter(|wit_interface| !http_iface.contains(wit_interface))
                .cloned()
                .collect::<Vec<_>>()
        };

        trace!(host_interfaces = ?host_interfaces, "determining missing guest interfaces");

        // A workload serves itself before the host does. Every component's world
        // up front, so an interface one component imports and another exports can
        // be recognised as already answered from inside the workload — see
        // [`served_within_workload`].
        let component_worlds: Vec<(Arc<str>, Arc<str>, WitWorld)> = self
            .components
            .iter()
            .map(|(id, component)| (id.clone(), Arc::from(component.name()), component.world()))
            .collect();

        // A component named after a declared `hostInterfaces` label shadows it.
        // The host wins (see `served_within_workload`), but the manifest reads
        // as though the component should serve that label. Only worth saying
        // when something actually imports under the label: a component that
        // merely exports a host-served interface is the ordinary case, and is
        // how the host reaches into a workload at all.
        for entry in &host_interfaces {
            let Some(label) = entry.name.as_deref() else {
                continue;
            };
            // An entry naming no interface matches every interface of its
            // package and never routes in-workload, so nothing can shadow it.
            if entry.interfaces.is_empty() {
                continue;
            }
            let shadowed = component_worlds
                .iter()
                .any(|(_, name, world)| name.as_ref() == label && exports_any_of(entry, world));
            let imported = component_worlds
                .iter()
                .any(|(_, _, world)| imports_under_label(entry, world, label))
                || self
                    .service
                    .as_ref()
                    .is_some_and(|s| imports_under_label(entry, &s.world(), label));
            if shadowed && imported {
                warn!(
                    label,
                    "a component shares the name of an `(implements ..)` label declared \
                     under `hostInterfaces`; the host serves that label"
                );
            }
        }

        if let Some(service) = self.service.as_ref() {
            let world = service.world();

            trace!(?world, "comparing service world to host interfaces");
            // An entry this item uses any of, not every name on it: the entry
            // covers what the whole workload uses (see [`WitWorld::uses`]).
            let required_interfaces: HashSet<WitInterface> = host_interfaces
                .iter()
                .filter(|wit_interface| world.uses(wit_interface))
                .filter(|wit_interface| !served_by_host(wit_interface, &world))
                .filter(|wit_interface| {
                    !served_within_workload(wit_interface, service.id(), &world, &component_worlds)
                })
                .cloned()
                .collect();

            if !required_interfaces.is_empty() {
                unmatched_interfaces.insert(
                    IdFlavor::Service(Arc::from(service.id())),
                    required_interfaces,
                );
            }
        }

        for (id, _name, world) in &component_worlds {
            trace!(?world, "comparing component world to host interfaces");
            let required_interfaces: HashSet<WitInterface> = host_interfaces
                .iter()
                .filter(|wit_interface| world.uses(wit_interface))
                .filter(|wit_interface| !served_by_host(wit_interface, world))
                .filter(|wit_interface| {
                    !served_within_workload(wit_interface, id, world, &component_worlds)
                })
                .cloned()
                .collect();

            if !required_interfaces.is_empty() {
                unmatched_interfaces.insert(IdFlavor::Component(id.clone()), required_interfaces);
            }
        }

        // A plugin that overrides WASI interfaces (see
        // [`HostPlugin::wasi_overrides`]) is required by every workload item that
        // imports one of them, for each `hostInterfaces` entry the plugin
        // serves. The entry is what opts the workload in; the matching below
        // then binds it like any other required interface.
        for (plugin_id, p) in plugins.iter() {
            let overrides = p.wasi_overrides();
            if overrides.imports.is_empty() {
                continue;
            }
            let world = p.world();
            for entry in &host_interfaces {
                if !world.includes_bidirectional(entry) || !p.claims(entry) {
                    continue;
                }
                if let Some(service) = &self.service {
                    let service_world = service.world();
                    if overrides.imports.iter().any(|o| service_world.uses(o)) {
                        unmatched_interfaces
                            .entry(IdFlavor::Service(Arc::from(service.id())))
                            .or_default()
                            .insert(entry.clone());
                    }
                }
                for (id, _name, component_world) in &component_worlds {
                    if !overrides.imports.iter().any(|o| component_world.uses(o)) {
                        continue;
                    }
                    trace!(
                        plugin_id,
                        interface = %entry,
                        component_id = id.as_ref(),
                        "component imports a WASI interface the plugin overrides; requiring the plugin"
                    );
                    unmatched_interfaces
                        .entry(IdFlavor::Component(id.clone()))
                        .or_default()
                        .insert(entry.clone());
                }
            }
        }

        trace!(?unmatched_interfaces, "resolving unmatched interfaces");

        // Iterate through each plugin first, then check every component for matching worlds.
        //
        // In plugin-id order, not `HashMap` order: where two plugins both match
        // an interface the first one reached claims it, because a matched
        // interface leaves `unmatched_interfaces` once its item bind succeeds.
        // Leaving that to hash iteration makes the same manifest bind
        // differently across process restarts — and with a plugin that *calls*
        // an interface rather than serving it, one of those outcomes fails the
        // deploy. A deterministic order at least makes the result reproducible.
        // Plugins that override WASI bind after the rest, so that nothing binds
        // over a redefinition and shadowing is on only while they bind.
        let mut plugins_in_order: Vec<(&&'static str, &Arc<dyn HostPlugin>)> =
            plugins.iter().collect();
        plugins_in_order.sort_by_key(|(id, p)| (!p.wasi_overrides().imports.is_empty(), **id));
        for (plugin_id, p) in plugins_in_order {
            let plugin_interfaces = p.world();
            trace!(plugin_id = plugin_id, plugin_interfaces = ?plugin_interfaces, "checking plugin interfaces");

            // Collect bindings for this plugin across all components
            let mut plugin_component_bindings = Vec::new();

            // Check each component to see if this plugin matches any of their required interfaces
            for (component_id, required_interfaces) in unmatched_interfaces.iter() {
                // Find interfaces that this plugin can satisfy for this component
                let mut matching_interfaces = HashSet::new();
                for wit_interface in required_interfaces.iter() {
                    // A named binding whose name matches a registered plugin
                    // id directly routes to exactly that plugin — the id IS
                    // the routing key, so this bypasses the
                    // named/unnamed `supports_named_instances()` deferral
                    // below entirely (that deferral is for the closed,
                    // native-only multiplexer's own `(implements ..)`
                    // labels, a separate routing axis). A plugin still has to
                    // actually serve the interface to be matched; a name that
                    // doesn't resolve to any registered plugin id falls
                    // through to the existing world-matching path unchanged
                    // (e.g. a `(implements ..)` multiplex label).
                    if let Some(name) = &wit_interface.name
                        && plugins.contains_key(name.as_str())
                    {
                        if name.as_str() == *plugin_id
                            && plugin_interfaces.includes_bidirectional(wit_interface)
                            && p.claims(wit_interface)
                        {
                            matching_interfaces.insert(wit_interface.clone());
                        }
                        continue;
                    }
                    // Check if plugin supports this interface. `claims` is the
                    // plugin's own veto over a world match it cannot serve
                    // (e.g. an entry that pins no version, which matches every
                    // revision); a refusal leaves the interface unmatched so a
                    // sibling plugin gets it.
                    if plugin_interfaces.includes_bidirectional(wit_interface)
                        && p.claims(wit_interface)
                    {
                        // An `(implements ..)` label routes to the plugin
                        // whose `host.plugins` entry declares it; failing
                        // that, to one that supports named instances. A plain
                        // import stays with a plugin that serves it plainly.
                        let defer_to_other = if let Some(label) = wit_interface.name.as_deref() {
                            match declaring_plugin(plugins, plugin_bindings, label, wit_interface) {
                                Some(owner) => owner != *plugin_id,
                                None => {
                                    !p.supports_named_instances()
                                        && other_plugin_serves(
                                            plugins,
                                            plugin_id,
                                            wit_interface,
                                            true,
                                        )
                                }
                            }
                        } else {
                            p.defers_unnamed_instances()
                                && other_plugin_serves(plugins, plugin_id, wit_interface, false)
                        };
                        if defer_to_other {
                            continue;
                        }
                        matching_interfaces.insert(wit_interface.clone());
                    }
                }

                if !matching_interfaces.is_empty() {
                    plugin_component_bindings.push((component_id.clone(), matching_interfaces));
                }
            }

            // If this plugin matches any components, bind them
            if !plugin_component_bindings.is_empty() {
                // The operator's declaration for this plugin, applied before
                // any of it reaches the plugin, so `on_workload_bind` and
                // `on_workload_item_bind` see the same resolved map and no
                // plugin has to know the policy exists. The pre-resolution
                // interfaces stay in `plugin_component_bindings`: they are what
                // `unmatched_interfaces` is keyed by.
                //
                // Resolution is keyed by binding name across the *whole*
                // workload, not per component: one label is one binding — one
                // connection, one grant — however many entries or components a
                // manifest splits it across.
                let declared = plugin_bindings.for_plugin(plugin_id);
                let schema = p.binding_schema();
                let narrows =
                    |key: &str, host: &str, workload: &str| p.narrows(key, host, workload);
                let requested_interfaces: HashSet<WitInterface> = plugin_component_bindings
                    .iter()
                    .flat_map(|(_, interfaces)| interfaces.clone())
                    .collect();
                // Plugin-backed secrets become operator config before the
                // policy runs, so it judges them like any other host key. The
                // plugin checked those layers at host start without the values,
                // so it checks the entry and the named bindings this bind
                // filled again; the unnamed binding's layer is checked by the
                // plugin's own bind, as it is without secrets. From here on an
                // error may quote a value or follow from a secret withheld from
                // this workload, so each goes through `explain`.
                let requesting = crate::plugin::RequestingWorkload {
                    id: self.id().to_string(),
                    name: self.grant_name().to_string(),
                    namespace: self.namespace().to_string(),
                };
                let filled = crate::plugin::resolve_plugin_secrets(
                    plugins,
                    &requesting,
                    declared,
                    &requested_interfaces,
                    &schema,
                )
                .await
                .and_then(|filled| {
                    if let Some(refilled) = &filled.refilled {
                        p.validate_bindings(&filled.set.only_bindings(refilled))
                            .map_err(|e| {
                                filled
                                    .errors
                                    .explain(e)
                                    .context("invalid `host.plugins` declaration")
                            })?;
                    }
                    Ok(filled)
                });
                let (declared, bind_errors) = match filled {
                    Ok(filled) => (filled.set, filled.errors),
                    Err(e) => {
                        unbind_all(
                            self.id(),
                            &bound_plugins_with_interfaces,
                            "plugin secret",
                            cleanup,
                        )
                        .await;
                        bail!(e.context(format!(
                            "workload {} cannot bind plugin '{plugin_id}'",
                            self.id()
                        )))
                    }
                };
                let resolved_by_name =
                    match declared.resolve_by_name(&requested_interfaces, &schema, &narrows) {
                        Ok(resolved) => resolved,
                        Err(e) => {
                            let e = bind_errors.explain(e);
                            unbind_all(
                                self.id(),
                                &bound_plugins_with_interfaces,
                                "binding policy",
                                cleanup,
                            )
                            .await;
                            bail!(e.context(format!(
                                "workload {} cannot bind plugin '{plugin_id}'",
                                self.id()
                            )))
                        }
                    };

                let resolved_component_bindings: Vec<HashSet<WitInterface>> =
                    plugin_component_bindings
                        .iter()
                        .map(|(_, interfaces)| {
                            declared.apply_resolved(interfaces, &resolved_by_name)
                        })
                        .collect();

                // Collect all unique interfaces across all component bindings for on_workload_bind
                let plugin_matched_interfaces: HashSet<WitInterface> = resolved_component_bindings
                    .iter()
                    .flat_map(Clone::clone)
                    .collect();

                // A name selects one backend of a package, so two bindings for
                // one package need two backends to route between. A plugin
                // serving a single instance has one, and collapsing them into
                // it delivers one binding's configuration under both names.
                // The operator declaring names under the plugin's
                // `host.plugins` entry says it routes them, whatever it
                // reports on its own.
                let routes_names =
                    p.supports_named_instances() || declared.binding_names().next().is_some();
                let mut ns_pkg_bindings: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
                for iface in &plugin_matched_interfaces {
                    let binding =
                        crate::plugin::binding_of(plugin_id, iface.name.as_deref()).unwrap_or("");
                    ns_pkg_bindings
                        .entry((iface.namespace.as_str(), iface.package.as_str()))
                        .or_default()
                        .insert(binding);
                }
                for ((ns, pkg), bindings) in ns_pkg_bindings {
                    if bindings.len() > 1 && !routes_names {
                        let named: Vec<&str> =
                            bindings.iter().copied().filter(|b| !b.is_empty()).collect();
                        // Plugins bind in id order, so earlier ones are already
                        // holding state — a connection, a subscription — for a
                        // workload that is not going to deploy.
                        unbind_all(
                            self.id(),
                            &bound_plugins_with_interfaces,
                            "binding policy",
                            cleanup,
                        )
                        .await;
                        bail!(
                            "plugin '{plugin_id}' does not support named instances, but the \
                             workload declares {} bindings for {ns}:{pkg} (named: {}). Each \
                             name routes to its own backend and this plugin serves one, so \
                             give those entries a single binding, or register a plugin that \
                             implements supports_named_instances().",
                            bindings.len(),
                            named.join(", "),
                        );
                    }
                }

                debug!(
                    plugin_id = plugin_id,
                    interfaces = ?plugin_matched_interfaces,
                    "binding plugin to workload"
                );

                let bind_span = tracing::span!(
                    tracing::Level::INFO,
                    "plugin_on_workload_bind",
                    plugin_id = plugin_id,
                );

                // Journal the binding *before* the await, so a cancelled or failed bind
                // is still unbound by `WorkloadResources::release`.
                // Without a journal, `ResolvedWorkload::unbind_all_plugins` recomputes this set
                // instead of using it, and the two can differ.
                if let Some(cleanup) = cleanup {
                    cleanup.binding(Arc::clone(p), plugin_matched_interfaces.clone());
                }
                if let Err(e) = p
                    .on_workload_bind(self, WitInterfaces::new(&plugin_matched_interfaces))
                    .instrument(bind_span)
                    .await
                {
                    let e = bind_errors.explain(e);
                    tracing::error!(
                        plugin_id = plugin_id,
                        err = ?e,
                        "failed to bind plugin to workload"
                    );
                    // Clean up plugin that just failed first.
                    if cleanup.is_none()
                        && let Err(cleanup_err) = p
                            .on_workload_unbind(
                                self.id(),
                                WitInterfaces::new(&plugin_matched_interfaces),
                            )
                            .await
                    {
                        warn!(
                            plugin_id = plugin_id,
                            error = ?cleanup_err,
                            "failed to cleanup partially bound plugin after bind failure"
                        );
                    }
                    unbind_all(
                        self.id(),
                        &bound_plugins_with_interfaces,
                        "bind failure",
                        cleanup,
                    )
                    .await;
                    bail!(e)
                }

                // Collect component IDs for this plugin
                let mut plugin_component_ids = Vec::new();

                // Now bind each component, with the resolved config the
                // policy produced and the original interfaces to strike off
                // `unmatched_interfaces`.
                for ((id, requested_interfaces), matching_interfaces) in plugin_component_bindings
                    .into_iter()
                    .zip(resolved_component_bindings)
                {
                    let mut workload_item = match &id {
                        IdFlavor::Component(component_id) => WorkloadItem::Component(
                            self.components
                                .get_mut(component_id)
                                .context("component not found during plugin binding")?,
                        ),
                        IdFlavor::Service(_) => {
                            WorkloadItem::Service(self.service.as_mut().ok_or_else(|| {
                                anyhow::anyhow!("Infallible. Service was presented before")
                            })?)
                        }
                    };

                    debug!(
                        plugin_id = plugin_id,
                        component_id = workload_item.id(),
                        interfaces = ?matching_interfaces,
                        "binding plugin to workload item"
                    );

                    let item_bind_span = tracing::span!(
                        tracing::Level::INFO,
                        "plugin_on_workload_item_bind",
                        plugin_id = plugin_id,
                    );
                    // A WASI override redefines names the host already linked; the
                    // linker allows that only for the duration of this bind.
                    let overriding = !p.wasi_overrides().imports.is_empty();
                    let bound = async {
                        if overriding {
                            workload_item.linker()?.allow_shadowing(true);
                        }
                        let bound = p
                            .on_workload_item_bind(
                                &mut workload_item,
                                WitInterfaces::new(&matching_interfaces),
                            )
                            .instrument(item_bind_span)
                            .await;
                        if overriding {
                            workload_item.linker()?.allow_shadowing(false);
                        }
                        bound
                    }
                    .await;
                    if let Err(e) = bound {
                        let e = bind_errors.explain(e);
                        tracing::error!(
                            plugin_id = plugin_id,
                            component_id = workload_item.id(),
                            err = ?e,
                            "failed to bind workload item to plugin"
                        );
                        // This plugin's own on_workload_bind succeeded, so it can hold a state until unbind.
                        // Go ahead and unbind immediately before the completed plugins.
                        if cleanup.is_none()
                            && let Err(cleanup_err) = p
                                .on_workload_unbind(
                                    self.id(),
                                    WitInterfaces::new(&plugin_matched_interfaces),
                                )
                                .await
                        {
                            warn!(
                                plugin_id = plugin_id,
                                error = ?cleanup_err,
                                "failed to cleanup partially bound plugin after item bind failure"
                            );
                        }
                        unbind_all(
                            self.id(),
                            &bound_plugins_with_interfaces,
                            "component bind failure",
                            cleanup,
                        )
                        .await;
                        bail!(e)
                    } else {
                        trace!(
                            plugin_id = plugin_id,
                            component_id = workload_item.id(),
                            "successfully bound plugin to component"
                        );
                        workload_item.add_plugin(plugin_id, p.clone());
                        workload_item.plugin_bound_instances.extend(
                            matching_interfaces
                                .iter()
                                .flat_map(plugin_bound_instance_names),
                        );
                        plugin_component_ids.push(workload_item.id().to_string());

                        // Remove matched interfaces from unmatched set
                        if let Some(unmatched) = unmatched_interfaces.get_mut(&id) {
                            for interface in requested_interfaces.iter() {
                                unmatched.remove(interface);
                            }
                        }
                    }
                }

                // Add this plugin with all its bound component IDs
                bound_plugins.push((p.clone(), plugin_component_ids.clone()));
                bound_plugins_with_interfaces.push((
                    p.clone(),
                    plugin_matched_interfaces,
                    plugin_component_ids,
                ));
            }
        }

        // Check if all required interfaces were matched
        for (component_id, unmatched) in unmatched_interfaces.iter() {
            if !unmatched.is_empty() {
                tracing::error!(
                    component_id = component_id.as_ref(),
                    interfaces = ?unmatched,
                    "no plugins found for requested interfaces"
                );
                unbind_all(
                    self.id(),
                    &bound_plugins_with_interfaces,
                    "unmatched interfaces",
                    cleanup,
                )
                .await;
                bail!(
                    "workload component {component_id} requested interfaces that are not available on this host: {unmatched:?}",
                )
            }
        }

        Ok(bound_plugins)
    }
}
