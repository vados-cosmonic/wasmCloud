//! The two kinds of item a workload runs —
//! [`WorkloadComponent`]s and the optional long-lived [`WorkloadService`] —
//! and [`WorkloadItem`], a borrow of either.

use super::*;

/// A [`WorkloadService`] is a component that is part of a workload that
/// runs once, either to completion or for the duration of the workload lifecycle.
#[derive(Clone)]
pub struct WorkloadService {
    /// The [`WorkloadMetadata`] for this service
    pub(crate) metadata: WorkloadMetadata,
    /// The maximum number of restarts for this service
    pub(super) max_restarts: u64,
    /// The [`JoinHandle`] for the running service
    pub(super) handle: Option<Arc<JoinHandle<()>>>,
}

impl WorkloadService {
    /// Create a new [`WorkloadService`] with the given workload ID,
    /// wasmtime [`Component`], [`Linker`], volume mounts, and instance limits.
    ///
    /// `volume_mounts` are trusted: they are opened as given, refusing only
    /// kernel filesystems, without the engine's `hostPath` policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workload_id: impl Into<Arc<str>>,
        workload_name: impl Into<Arc<str>>,
        workload_namespace: impl Into<Arc<str>>,
        component: Component,
        linker: impl Into<Arc<Linker<SharedCtx>>>,
        volume_mounts: Vec<(PathBuf, VolumeMount)>,
        local_resources: LocalResources,
        max_restarts: u64,
        loopback: Arc<std::sync::Mutex<loopback::Network>>,
    ) -> Self {
        Self {
            metadata: WorkloadMetadata {
                id: uuid::Uuid::now_v7().to_string().into(),
                workload_id: workload_id.into(),
                workload_name: workload_name.into(),
                workload_namespace: workload_namespace.into(),
                component,
                may_use_gc: true,
                link: LinkState::Open {
                    linker: linker.into(),
                    pre: std::sync::OnceLock::new(),
                    written: false,
                },
                volume_mounts,
                resolved_volume_mounts: Vec::new(),
                local_resources,
                plugins: None,
                loopback,
                socket_policy: Arc::default(),
                guest_memory: Arc::default(),
                store_settings: Default::default(),
                trap_hints: Arc::default(),
                linked_components: Default::default(),
                annotations: Arc::default(),
                plugin_bound_instances: Default::default(),
                compiled: None,
            },
            handle: None,
            max_restarts,
        }
    }

    /// Attach workload annotations.
    pub fn with_annotations(
        mut self,
        annotations: impl Into<Arc<HashMap<String, String>>>,
    ) -> Self {
        self.metadata.annotations = annotations.into();
        self
    }

    /// Share the workload deploy's trap hints with this component.
    pub(crate) fn with_trap_hints(
        mut self,
        hints: &Arc<crate::engine::trap_remediation::TrapHints>,
    ) -> Self {
        self.metadata.trap_hints = Arc::clone(hints);
        self
    }

    /// Pre-instantiate the component to prepare for execution.
    ///
    /// Reuses the linked component the service keeps once it is sealed (see
    /// [`WorkloadMetadata::is_sealed`]), so service restarts do not need to
    /// retain a linker copy.
    pub fn pre_instantiate(&self) -> anyhow::Result<CommandPre<SharedCtx>> {
        let command = CommandPre::new(self.metadata.linked_pre()?)?;
        Ok(command)
    }

    /// Pre-instantiate the component for P3 execution using the same cached
    /// link as [`Self::pre_instantiate`].
    pub fn pre_instantiate_p3(
        &self,
    ) -> anyhow::Result<wasmtime_wasi::p3::bindings::CommandPre<SharedCtx>> {
        let command = wasmtime_wasi::p3::bindings::CommandPre::new(self.metadata.linked_pre()?)?;
        Ok(command)
    }

    /// Pre-instantiate the raw component, leaving binding-view construction
    /// (e.g. both cli `Command` and http `Service`) to the caller. Used when a
    /// p3 service also serves HTTP and must drive both exports on one instance.
    pub fn pre_instantiate_raw(
        &self,
    ) -> anyhow::Result<wasmtime::component::InstancePre<SharedCtx>> {
        Ok(self.metadata.linked_pre()?)
    }

    /// Whether or not the service is currently running.
    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }
}

/// A [`WorkloadComponent`] is a component that is part of a workload.
///
/// It contains the actual [`Component`] that can be instantiated,
/// the [`Linker`] for creating stores and instances, the available
/// [`VolumeMount`]s to be passed as filesystem preopens, and the
/// full list of [`HostPlugin`]s that the component depends on.
#[derive(Clone)]
pub struct WorkloadComponent {
    /// Component name
    ///
    /// This is the name used for `(implements <name>)` imports.
    pub(super) name: ComponentName,
    /// The [`WorkloadMetadata`] for this component
    pub(crate) metadata: WorkloadMetadata,
    /// Instances kept warm between ephemeral linked calls. Shared by every
    /// clone of this component, so all importers draw on the one warm set.
    pub(crate) instances: Arc<InstancePool>,
}

impl WorkloadComponent {
    /// Create a new [`WorkloadComponent`] with the given workload ID,
    /// wasmtime [`Component`], [`Linker`], volume mounts, and instance limits.
    ///
    /// `volume_mounts` are trusted: they are opened as given, refusing only
    /// kernel filesystems, without the engine's `hostPath` policy.
    ///
    /// `instances` is what the component asked for by way of instance reuse.
    /// [`InstancePolicy::Ephemeral`] keeps the default behavior of a fresh
    /// store per call; [`InstancePolicy::Warm`] parks instances between calls
    /// so guest state survives them.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workload_id: impl Into<Arc<str>>,
        workload_name: impl Into<Arc<str>>,
        workload_namespace: impl Into<Arc<str>>,
        component_name: impl Into<Arc<str>>,
        component: Component,
        linker: impl Into<Arc<Linker<SharedCtx>>>,
        volume_mounts: Vec<(PathBuf, VolumeMount)>,
        local_resources: LocalResources,
        loopback: Arc<std::sync::Mutex<loopback::Network>>,
        instances: InstancePolicy,
    ) -> Self {
        Self {
            metadata: WorkloadMetadata {
                id: uuid::Uuid::now_v7().to_string().into(),
                workload_id: workload_id.into(),
                workload_name: workload_name.into(),
                workload_namespace: workload_namespace.into(),
                component,
                may_use_gc: true,
                link: LinkState::Open {
                    linker: linker.into(),
                    pre: std::sync::OnceLock::new(),
                    written: false,
                },
                volume_mounts,
                resolved_volume_mounts: Vec::new(),
                local_resources,
                plugins: None,
                loopback,
                socket_policy: Arc::default(),
                guest_memory: Arc::default(),
                store_settings: Default::default(),
                trap_hints: Arc::default(),
                linked_components: Default::default(),
                annotations: Arc::default(),
                plugin_bound_instances: Default::default(),
                compiled: None,
            },
            name: component_name.into(),
            instances: Arc::new(InstancePool::new(instances)),
        }
    }

    /// Attach workload annotations.
    pub fn with_annotations(
        mut self,
        annotations: impl Into<Arc<HashMap<String, String>>>,
    ) -> Self {
        self.metadata.annotations = annotations.into();
        self
    }

    /// Share the workload deploy's trap hints with this component.
    pub(crate) fn with_trap_hints(
        mut self,
        hints: &Arc<crate::engine::trap_remediation::TrapHints>,
    ) -> Self {
        self.metadata.trap_hints = Arc::clone(hints);
        self
    }

    /// Pre-instantiate the component to prepare for instantiation. The same
    /// as [`Self::pre_instantiate_ref`]; both names are kept.
    pub fn pre_instantiate(&self) -> wasmtime::Result<InstancePre<SharedCtx>> {
        self.pre_instantiate_ref()
    }

    /// The component's kept link, ready to instantiate.
    ///
    /// Linked once and reused: a request store or dispatch asks for this every
    /// time, and checking every import against the linker per request is work
    /// whose answer cannot change while nothing takes the linker mutably
    /// (see [`WorkloadMetadata::linker`], which forgets it). Once the workload
    /// is resolved the link is sealed, and the linker gone if the component
    /// linked (see [`WorkloadMetadata::is_sealed`]).
    pub fn pre_instantiate_ref(&self) -> wasmtime::Result<InstancePre<SharedCtx>> {
        self.metadata.linked_pre()
    }

    pub fn metadata(&self) -> &WorkloadMetadata {
        &self.metadata
    }

    /// Component name, as written in the manifest.
    ///
    /// This is the name an `(implements <name>)` import routes by, not
    /// [`Self::id`], which is a runtime UUID.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Represents an item within a workload, such as a component or a service.
#[derive(Debug)]
pub enum WorkloadItem<'a> {
    Component(&'a mut WorkloadComponent),
    Service(&'a mut WorkloadService),
}

impl<'a> WorkloadItem<'a> {
    /// Returns true if the item is a component.
    pub fn is_component(&self) -> bool {
        matches!(self, WorkloadItem::Component(_))
    }

    /// Returns true if the item is a service.
    pub fn is_service(&self) -> bool {
        matches!(self, WorkloadItem::Service(_))
    }
}

impl<'a> Deref for WorkloadItem<'a> {
    type Target = WorkloadMetadata;

    fn deref(&self) -> &Self::Target {
        match self {
            WorkloadItem::Component(component) => component,
            WorkloadItem::Service(service) => service,
        }
    }
}

impl<'a> DerefMut for WorkloadItem<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            WorkloadItem::Component(component) => component,
            WorkloadItem::Service(service) => service,
        }
    }
}

impl std::fmt::Debug for WorkloadComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkloadComponent")
            .field("id", &self.metadata.id.as_ref())
            .field("workload_id", &self.metadata.workload_id.as_ref())
            .field("volume_mounts", &self.metadata.volume_mounts)
            .field("has_warm_instances", &self.instances.warms_instances())
            .finish()
    }
}

impl std::fmt::Debug for WorkloadService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkloadService")
            .field("id", &self.metadata.id.as_ref())
            .field("workload_name", &self.metadata.workload_name.as_ref())
            .field(
                "workload_namespace",
                &self.metadata.workload_namespace.as_ref(),
            )
            .field("workload_id", &self.metadata.workload_id.as_ref())
            .field("volume_mounts", &self.metadata.volume_mounts)
            .field("is_running", &self.is_running())
            .finish()
    }
}

impl Deref for WorkloadComponent {
    type Target = WorkloadMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl DerefMut for WorkloadComponent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.metadata
    }
}

impl Deref for WorkloadService {
    type Target = WorkloadMetadata;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl DerefMut for WorkloadService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.metadata
    }
}
