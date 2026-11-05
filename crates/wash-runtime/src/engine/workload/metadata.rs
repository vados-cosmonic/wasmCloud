//! Per-item metadata shared by components and services:
//! identity, plugins, resources, and the linker and link state ([`LinkState`])
//! that resolution writes to and sealing freezes.

use super::*;

/// Metadata associated with components and services within a workload.
#[derive(Clone)]
pub struct WorkloadMetadata {
    /// The unique identifier for this component
    pub(crate) id: Arc<str>,
    /// The unique identifier for the workload this component belongs to
    pub(crate) workload_id: Arc<str>,
    /// The name of the workload this component belongs to
    pub(super) workload_name: Arc<str>,
    /// The namespace of the workload this component belongs to
    pub(super) workload_namespace: Arc<str>,
    /// The actual wasmtime [`Component`] that can be instantiated
    pub(super) component: Component,
    /// Whether a store of this component may need a GC heap, as the engine
    /// read off its source; `true`, the safe answer, until the engine says.
    pub(crate) may_use_gc: bool,
    /// The component's linker and the link built from it; see [`LinkState`].
    pub(super) link: LinkState,
    /// The volume mounts requested by this component
    pub(crate) volume_mounts: Vec<(PathBuf, VolumeMount)>,
    /// Pinned directory handles with guest mount names and permissions.
    pub(crate) resolved_volume_mounts: Vec<ResolvedVolumeMount>,
    /// The local resources requested by this component
    pub(crate) local_resources: LocalResources,
    /// The plugins available to this component
    pub(crate) plugins: Option<HashMap<&'static str, Arc<dyn HostPlugin + Send + Sync>>>,
    /// Workload loopback
    pub(crate) loopback: Arc<std::sync::Mutex<loopback::Network>>,
    /// The host-level half of this component's socket policy: enforcement mode,
    /// address ranges, whether the host-loopback door is open at all, the
    /// host's port table, and the connection budget. Installed by the engine
    /// from [`crate::engine::Engine`]'s configuration; the workload-level half
    /// (`allowedHosts`, `allowedHostLoopbackPorts`) comes from `local_resources`.
    pub(crate) socket_policy: Arc<crate::sockets::policy::SocketPolicy>,
    /// The host-wide guest memory budget every store built for this component
    /// draws on. Installed by the engine alongside `socket_policy`; the
    /// [`Default`] here is an unmetered budget, so a metadata built outside an
    /// engine is neither charged nor limited.
    pub(crate) guest_memory: Arc<crate::engine::guest_memory::GuestMemoryBudget>,
    /// What every store built for this component is configured with, from
    /// the engine. [`Default`] here is wasmtime's own defaults.
    pub(crate) store_settings: crate::engine::store_settings::StoreSettings,
    /// The remediation hints this workload deploy has already logged. One set
    /// is shared by every component of a deploy, so a redeploy starts fresh.
    pub(crate) trap_hints: Arc<crate::engine::trap_remediation::TrapHints>,
    /// Every component this one's store must instantiate alongside it:
    /// the *transitive* closure of its links
    /// (see [`expand_link_closure`]),
    /// not only the components it imports from directly.
    /// Store admission and instance pooling rely on it being complete.
    pub(super) linked_components: HashSet<Arc<str>>,
    /// Workload annotations
    pub(crate) annotations: Arc<HashMap<String, String>>,
    /// Top-level imports a host plugin already defined in this component's linker,
    /// spelled as the linker sees them
    /// (an `(implements ..)` label, or `ns:pkg/iface[@version]`).
    /// Written during plugin binding with [`plugin_bound_instance_names`];
    /// read by [`should_link_component_import`] so linking leaves them alone.
    pub(super) plugin_bound_instances: HashSet<String>,
    /// Keeps the engine reusing this compile for any workload that loads the
    /// same digest while this one runs (see [`crate::engine::LiveCompile`]).
    pub(crate) compiled: Option<Arc<crate::engine::LiveCompile>>,
}

/// The component's linker, cached link, and write access in one state.
#[derive(Clone)]
pub(super) enum LinkState {
    /// Binding and linking are under way.
    ///
    /// - `linker` is the engine's shared base until something writes to it,
    ///   and a copy of its own from then on (`written`).
    /// - `pre` is the link, kept once a caller asks for it; a write forgets it.
    Open {
        linker: Arc<Linker<SharedCtx>>,
        pre: std::sync::OnceLock<InstancePre<SharedCtx>>,
        written: bool,
    },
    /// Resolved, and nothing ever wrote to the linker:
    /// it is still the engine's shared base, so dropping it would free nothing.
    /// The link is made on first use, as before sealing.
    Shared {
        linker: Arc<Linker<SharedCtx>>,
        pre: std::sync::OnceLock<InstancePre<SharedCtx>>,
    },
    /// Resolved and linked: the linker copy is gone, and the link stands.
    Linked(InstancePre<SharedCtx>),
    /// Resolved without a link (an import nothing satisfied).
    /// Only wasmtime's error is kept, rendered, so each use repeats it.
    Unlinked(Arc<str>),
}

impl WorkloadMetadata {
    pub(super) async fn resolve_volume_mounts(&mut self) -> anyhow::Result<()> {
        let mut resolved = Vec::with_capacity(self.volume_mounts.len());
        for (host_path, mount) in &self.volume_mounts {
            resolved.push(ResolvedVolumeMount::from_mount(host_path, mount).await?);
        }
        self.resolved_volume_mounts = resolved;
        Ok(())
    }

    /// Returns the unique identifier for this component.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the ID of the workload this component belongs to.
    pub fn workload_id(&self) -> &str {
        &self.workload_id
    }

    /// Returns the name of the workload this component belongs to.
    pub fn workload_name(&self) -> &str {
        &self.workload_name
    }

    /// Returns the stable workload name across replicas, if a deployment,
    /// daemonset or replicaset annotation was supplied by the orchestrator.
    /// Falls back to `workload_name`.
    pub fn stable_workload_name(&self) -> &str {
        stable_name(&self.annotations, &self.workload_name)
    }

    /// Returns the workload's annotations.
    pub fn annotations(&self) -> &HashMap<String, String> {
        &self.annotations
    }

    /// Returns the namespace of the workload this component belongs to.
    pub fn workload_namespace(&self) -> &str {
        &self.workload_namespace
    }

    /// Returns a reference to the wasmtime engine used to compile this component.
    pub fn engine(&self) -> &wasmtime::Engine {
        self.component.engine()
    }

    /// Returns the mutable linker before workload resolution.
    ///
    /// Copies the shared base on first write and invalidates the cached link.
    /// Plugins bind in [`HostPlugin::on_workload_item_bind`], before resolution.
    ///
    /// # Errors
    ///
    /// Returns an error if workload resolution has sealed the linker.
    pub fn linker(&mut self) -> anyhow::Result<&mut Linker<SharedCtx>> {
        let LinkState::Open {
            linker,
            pre,
            written,
        } = &mut self.link
        else {
            bail!(
                "linker for component {} in workload {} is sealed; plugins must bind in \
                 on_workload_item_bind, before resolution",
                self.id,
                self.workload_id,
            );
        };
        *pre = std::sync::OnceLock::new();
        *written = true;
        Ok(Arc::make_mut(linker))
    }

    /// Whether workload resolution has sealed the linker against writes.
    pub fn is_sealed(&self) -> bool {
        !matches!(self.link, LinkState::Open { .. })
    }

    /// The link, for both component invocations and service starts:
    /// kept once made, and forgotten by a write before sealing.
    pub(super) fn linked_pre(&self) -> wasmtime::Result<InstancePre<SharedCtx>> {
        match &self.link {
            LinkState::Open { linker, pre, .. } | LinkState::Shared { linker, pre } => {
                if let Some(kept) = pre.get() {
                    return Ok(kept.clone());
                }
                let linked = linker.instantiate_pre(&self.component)?;
                // Losing a race to another caller leaves the same answer stored.
                let _ = pre.set(linked.clone());
                Ok(linked)
            }
            LinkState::Linked(kept) => Ok(kept.clone()),
            LinkState::Unlinked(error) => Err(wasmtime::format_err!("{error}")),
        }
    }

    /// Seal the linker against further writes.
    ///
    /// - **Nothing wrote to the linker:** it is still the engine's shared base,
    ///   so it is kept, and the link is made on first use, as before.
    ///   Dropping it would free nothing.
    /// - **Something wrote to it:** the component holds a copy of the base,
    ///   every WASI and `wasi:http` definition included: about 140 KiB,
    ///   against the ~10 KiB the rest of an idle workload costs.
    ///   The [`InstancePre`] keeps only what this component imports,
    ///   so the link is made here and the copy let go.
    /// - **It does not link** (an import nothing satisfied):
    ///   only the error is kept, so each use repeats it,
    ///   and the workload still runs, as it did before sealing.
    ///
    /// Returns whether a linker copy was let go for a kept link.
    pub(crate) fn seal_linker(&mut self) -> bool {
        let LinkState::Open {
            linker,
            pre,
            written,
        } = &self.link
        else {
            return matches!(self.link, LinkState::Linked(_));
        };
        if !written {
            self.link = LinkState::Shared {
                linker: Arc::clone(linker),
                pre: pre.clone(),
            };
            return false;
        }
        self.link = match self.linked_pre() {
            Ok(linked) => LinkState::Linked(linked),
            Err(error) => {
                tracing::warn!(
                    workload_id = %self.workload_id,
                    component_id = %self.id,
                    error = format!("{error:#}"),
                    "component does not link; every call to it will fail with this error"
                );
                LinkState::Unlinked(format!("{error:#}").into())
            }
        };
        matches!(self.link, LinkState::Linked(_))
    }

    /// Reopen a host-linked WASI interface for a declared plugin override.
    ///
    /// Use this from [`HostPlugin::on_workload_item_bind`] after declaring the
    /// interface in [`HostPlugin::wasi_overrides`]. Name one interface with a
    /// version, such as `wasi:clocks/monotonic-clock@0.2.0`.
    ///
    /// Resolves compatible revisions to the exact instance the host registered,
    /// preserving every function and resource the plugin does not redefine.
    /// Calling `linker()?.instance(...)` with a different revision creates a
    /// separate instance: it neither overrides the host's newer name nor
    /// inherits the host's remaining definitions.
    pub fn wasi_override_instance(
        &mut self,
        interface: &str,
    ) -> anyhow::Result<LinkerInstance<'_, SharedCtx>> {
        let interface = WitInterface::from(interface);
        ensure!(
            interface.is_host_builtin() && interface.interfaces.len() == 1,
            "a WASI override must name one host-linked interface at a supported version"
        );
        let requested = interface
            .version
            .as_ref()
            .context("a WASI override requires an explicit version to select WASIp2 or WASIp3")?;
        let version = crate::engine::WASI_LINKER_VERSIONS
            .into_iter()
            .find(|linked| crate::wit::versions_compatible(Some(requested), Some(linked)))
            .context("unsupported WASI override version")?;
        let name = interface
            .interfaces
            .iter()
            .next()
            .context("missing WASI interface name")?;
        let instance = format!("wasi:{}/{name}@{version}", interface.package);
        Ok(self.linker()?.instance(&instance)?)
    }

    /// Returns a reference to the wasmtime [`Component`].
    pub fn component(&self) -> &Component {
        &self.component
    }

    /// Returns a reference to component local resources.
    pub fn local_resources(&self) -> &LocalResources {
        &self.local_resources
    }

    /// Returns a reference to the plugins associated with this component.
    pub fn plugins(&self) -> &Option<HashMap<&'static str, Arc<dyn HostPlugin + Send + Sync>>> {
        &self.plugins
    }

    /// Adds a [`HostPlugin`] to the component.
    pub fn add_plugin(&mut self, id: &'static str, plugin: Arc<dyn HostPlugin + Send + Sync>) {
        if let Some(ref mut plugins) = self.plugins {
            plugins.insert(id, plugin);
        } else {
            let mut plugins = HashMap::new();
            plugins.insert(id, plugin);
            self.plugins = Some(plugins);
        }
    }

    /// Replaces all plugins for this component with the provided set.
    pub fn with_plugins(
        &mut self,
        plugins: HashMap<&'static str, Arc<dyn HostPlugin + Send + Sync>>,
    ) {
        self.plugins = Some(plugins);
    }

    /// Extracts the [`ComponentItem::ComponentInstance`]s that the component exports.
    pub fn component_exports(&self) -> anyhow::Result<Vec<(String, ComponentItem)>> {
        Ok(self
            .component
            .component_type()
            .exports(self.component.engine())
            .map(|(name, item)| (name, item.ty))
            .filter_map(|(name, item)| {
                if matches!(item, ComponentItem::ComponentInstance(_)) {
                    Some((name.to_string(), item))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>())
    }

    pub fn uses_wasi_http(&self) -> bool {
        crate::engine::uses_wasi_http(&self.component)
    }

    pub fn imports_wasi_http(&self) -> bool {
        crate::engine::imports_wasi_http(&self.component)
    }

    pub fn exports_wasi_http(&self) -> bool {
        crate::engine::exports_wasi_http(&self.component)
    }

    /// Returns whether this component targets WASIP3.
    pub fn targets_p3(&self) -> bool {
        crate::engine::targets_wasip3(&self.component)
    }

    /// Computes and returns the [`WitWorld`] of this component.
    pub fn world(&self) -> WitWorld {
        let mut imports = HashMap::new();
        let mut exports = HashMap::new();

        // Iterate over imports, merging interfaces when namespace:package@version matches
        for (import_name, import_item) in self
            .component
            .component_type()
            .imports(self.component.engine())
        {
            if let ComponentItem::ComponentInstance(_) = &import_item.ty {
                // An `(implements <id>)` import carries the real interface
                // identity in its `implements` annotation; the import name is
                // the multiplex label (e.g. `team-a`). Build the interface from
                // the annotation and record the label as the routing `name` so
                // it lines up with a named host interface. A plain import uses
                // its name as the identity directly.
                let interface = match import_item.implements {
                    Some(implements_id) => {
                        let mut iface = WitInterface::from(implements_id);
                        iface.name = Some(import_name.to_string());
                        iface
                    }
                    None => WitInterface::from(import_name),
                };
                let k = interface.instance();
                imports
                    .entry(k)
                    .and_modify(|existing: &mut WitInterface| {
                        existing.merge(&interface);
                    })
                    .or_insert(interface);
            } else {
                debug!(
                    import_name,
                    "imported item is not a component instance, skipping"
                );
            }
        }

        // Iterate over exports, merging interfaces when namespace:package@version matches
        for (export_name, export_item) in self
            .component
            .component_type()
            .exports(self.component.engine())
        {
            if let ComponentItem::ComponentInstance(_) = export_item.ty {
                let interface = WitInterface::from(export_name);
                let k = interface.instance();
                exports
                    .entry(k)
                    .and_modify(|existing: &mut WitInterface| {
                        existing.merge(&interface);
                    })
                    .or_insert(interface);
            } else {
                debug!(
                    export_name,
                    "exported item is not a component instance, skipping"
                );
            }
        }

        WitWorld {
            imports: imports.into_values().collect(),
            exports: exports.into_values().collect(),
        }
    }
}
