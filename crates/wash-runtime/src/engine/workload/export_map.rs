//! Which component exports which interface,
//! and how an import — plain or `(implements ..)`-labelled — picks its exporter.

use super::*;

/// A component's manifest `name`: what an `(implements <name>)` import is
/// written against.
pub(super) type ComponentName = Arc<str>;

/// A component's runtime id: what the workload's component map is keyed by.
pub(super) type ComponentId = Arc<str>;

/// One component that exports an interface. Both halves are needed: an
/// `(implements ..)` label matches the `name`, and linking needs the `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ComponentExporter {
    pub(super) name: ComponentName,
    pub(super) id: ComponentId,
}

/// What [`build_export_map`] returns.
pub(super) struct ExportMaps {
    /// Interface -> the one component exporting it. An interface exported by
    /// more than one component is absent here and listed in
    /// [`Self::ambiguous`] instead.
    pub(super) unambiguous: HashMap<String, ComponentId>,
    /// Interfaces exported by more than one component.
    pub(super) ambiguous: HashSet<String>,
    /// Every exporter of every interface, so an `(implements ..)` label can
    /// still choose among them after [`Self::unambiguous`] has dropped it.
    pub(super) exporters: HashMap<String, Vec<ComponentExporter>>,
}

/// Builds the export->component map used to wire intra-workload component
/// imports. An interface exported by exactly one component is registered for
/// linking; an interface exported by more than one component is "ambiguous",
/// left out of [`ExportMaps::unambiguous`] and listed in
/// [`ExportMaps::ambiguous`] instead.
///
/// Ambiguity is not an error on its own: host-invoked exports (e.g.
/// `wasmcloud:messaging/handler`, `wasi:http/incoming-handler`) are consumed
/// by host plugins and never imported by another component, so multiple
/// exporters are expected. The importer side ([`resolve_workload_imports`])
/// errors only if a component actually imports an ambiguous interface.
pub(super) fn build_export_map(
    component_exports: &[(ComponentExporter, Vec<String>)],
) -> ExportMaps {
    let mut unambiguous: HashMap<String, ComponentId> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    // Every exporter of every interface, kept whole. `unambiguous` collapses
    // to nothing the moment two components export the same interface, which is
    // exactly the case an `(implements ..)` label exists to resolve — so the
    // label has to be able to look past that collapse to the candidates.
    let mut exporters: HashMap<String, Vec<ComponentExporter>> = HashMap::new();

    for (exporter, names) in component_exports {
        let component_id = &exporter.id;
        for name in names {
            exporters
                .entry(name.clone())
                .or_default()
                .push(exporter.clone());
            if ambiguous.contains(name) {
                continue;
            }
            if unambiguous.remove(name).is_some() {
                // A second exporter for this interface: mark it ambiguous.
                trace!(
                    name,
                    "interface exported by multiple components; deferring to import side"
                );
                ambiguous.insert(name.clone());
            } else {
                trace!(name, "registering component export for linking");
                unambiguous.insert(name.clone(), component_id.clone());
            }
        }
    }

    ExportMaps {
        unambiguous,
        ambiguous,
        exporters,
    }
}

/// What an intra-workload import resolves to: the interface being asked for,
/// and the `(implements ..)` label if the import carried one.
///
/// A plain import names its interface directly. A labelled import names the
/// LABEL, and carries the interface identity in its `implements` annotation —
/// so reading the import name alone asks the export map for "feed-a" when the
/// map is keyed by "example:feeds/reader@0.1.0", and misses.
pub(super) fn resolve_import<'a>(
    import_name: &'a str,
    implements: Option<&'a str>,
) -> (&'a str, Option<&'a str>) {
    match implements {
        Some(interface) => (interface, Some(import_name)),
        None => (import_name, None),
    }
}

/// The component and export name a labelled import routes to.
/// Compatible versions can use different import and export names.
///
/// This mirrors the rule already used for host interfaces, where a label
/// matching a registered plugin id routes straight to that plugin because "the
/// id IS the routing key". The component analogue is the same shape and needs
/// no manifest change. An operator-declared binding tier can layer on top later
/// exactly as `declaring_plugin` does for plugins, without disturbing this.
pub(super) fn labelled_exporter<'a>(
    exporters: &'a HashMap<String, Vec<ComponentExporter>>,
    interface: &str,
    label: &str,
) -> anyhow::Result<Option<(&'a Arc<str>, &'a str)>> {
    let mut candidates: Vec<_> = exporters
        .get_key_value(interface)
        .into_iter()
        .flat_map(|(name, entries)| entries.iter().map(move |entry| (entry, name.as_str())))
        .filter(|(entry, _)| entry.name.as_ref() == label)
        .collect();
    if candidates.is_empty() {
        let imported = WitInterface::from(interface);
        candidates = exporters
            .iter()
            .filter(|(name, _)| WitInterface::from(name.as_str()).contains(&imported))
            .flat_map(|(name, entries)| entries.iter().map(move |entry| (entry, name.as_str())))
            .filter(|(entry, _)| entry.name.as_ref() == label)
            .collect();
    }
    let mut matches = candidates.iter();
    let first = matches.next();
    if first.is_some() && matches.next().is_some() {
        // A label must select one compatible export.
        anyhow::bail!(
            "`(implements {label})` is ambiguous: {} compatible exports named '{label}' \
             provide '{interface}'. A named import must select one export.",
            candidates.len()
        );
    }
    Ok(first.map(|(entry, name)| (&entry.id, *name)))
}
