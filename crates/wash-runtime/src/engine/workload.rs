//! Workloads: what a host runs, and how it gets from an [`UnresolvedWorkload`]
//! (components, a service, and the host interfaces they need)
//! to a [`ResolvedWorkload`] ready to serve calls.
//!
//! This file holds the data model — both workload types,
//! their constructors and accessors, and the annotation keys.
//! The behavior lives in child modules, one concern each:
//!
//! - [`metadata`] — per-item identity, resources, plugins, and link state.
//! - [`item`] — components, the service, and [`WorkloadItem`].
//! - [`resolve`] — from unresolved to resolved.
//! - [`binding`] — matching imports to host plugins.
//! - [`export_map`] — which component exports what, and import lookup.
//! - [`linking`] — linking components to each other, sealing, pre-instantiation.
//! - [`stores`] — building stores and admitting calls.
//! - [`call_routing`] — sending a call to the service or a pooled instance.
//! - [`service`] — running and supervising the long-lived service.
//! - `plugin_exports` — exports reached by host component plugins
//!   (`host-component-plugins` feature).
//! - [`teardown`] — releasing a workload and its resources.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::Arc,
};

use crate::{plugin::WitInterfaces, sockets::loopback};
use anyhow::{bail, ensure};
use tokio::{sync::RwLock, task::JoinHandle};
use tracing::{Instrument, debug, error, info, instrument, trace, warn};
use wasmtime::component::{
    Component, InstancePre, Linker, LinkerInstance, ResourceAny, ResourceType, types::ComponentItem,
};
use wasmtime::error::Context as _;
use wasmtime_wasi::p2::bindings::CommandPre;

#[cfg(feature = "wasi-tls")]
use crate::engine::ctx::SharedTlsProvider;
#[cfg(feature = "host-component-plugins")]
use crate::engine::linked_call::{
    ServiceExportCall, types_are_bridge_safe, types_are_ephemeral_safe,
};
use crate::{
    engine::{
        ctx::SharedCtx,
        dispatch::{DispatchTarget, INGRESS_BACKLOG, ServiceCalls, ServiceClaim},
        instance_pool::{self, InstancePolicy, InstancePool},
        linked_call::{
            ComponentCtxTemplate, EphemeralCallMode, EphemeralLinkedCall, LinkedExportInvocation,
            LinkedTarget, func_is_bridge_safe, func_is_ephemeral_safe, invoke_linked_async_export,
            invoke_linked_sync_export, new_store_from_templates,
        },
        volumes::{ResolvedVolumeMount, resolve_component_volume_mounts_in_map},
    },
    plugin::HostPlugin,
    types::{LocalResources, VolumeMount},
    wit::{WitInterface, WitWorld},
};

mod binding;
mod call_routing;
mod export_map;
mod item;
mod linking;
mod metadata;
#[cfg(feature = "host-component-plugins")]
mod plugin_exports;
mod resolve;
mod service;
mod stores;
mod teardown;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

// Children that hold only `impl` blocks export no names and are not glob-imported.
use self::{binding::*, export_map::*, linking::*, metadata::*, teardown::*};
// `workload_runtime_tests` reaches this through `super::*`.
#[cfg(test)]
use self::service::build_trigger_ingresses;

pub use self::item::{WorkloadComponent, WorkloadItem, WorkloadService};
pub use self::metadata::WorkloadMetadata;
#[cfg(feature = "host-component-plugins")]
pub(crate) use self::plugin_exports::{ExternalCallFunc, ItemExport};
pub(crate) use self::teardown::WorkloadResources;

/// Kubernetes annotation set by the runtime-operator for the parent WorkloadDeployment.
pub const WORKLOAD_DEPLOYMENT_ANNOTATION: &str = "runtime.wasmcloud.dev/workload-deployment";

/// Kubernetes annotation set by the runtime-operator for the parent WorkloadDaemonSet.
pub const WORKLOAD_DAEMONSET_ANNOTATION: &str = "runtime.wasmcloud.dev/workload-daemonset";

/// Kubernetes annotation set by the runtime-operator for the parent WorkloadReplicaSet.
pub const WORKLOAD_REPLICASET_ANNOTATION: &str = "runtime.wasmcloud.dev/workload-replicaset-name";

/// Alternate WorkloadReplicaSet annotation key.
pub const WORKLOAD_REPLICASET_LEGACY_ANNOTATION: &str = "runtime.wasmcloud.dev/workload-replicaset";

/// Standard Kubernetes application name annotation.
pub const K8S_APP_NAME_ANNOTATION: &str = "app.kubernetes.io/name";

fn annotation<'a>(annotations: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    annotations
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The name the orchestrator gave the workload's replicas: its deployment's,
/// else its daemonset's, else its replicaset's, if the annotations name one.
fn replica_group(annotations: &HashMap<String, String>) -> Option<&str> {
    annotation(annotations, WORKLOAD_DEPLOYMENT_ANNOTATION)
        .or_else(|| annotation(annotations, WORKLOAD_DAEMONSET_ANNOTATION))
        .or_else(|| annotation(annotations, WORKLOAD_REPLICASET_ANNOTATION))
        .or_else(|| annotation(annotations, WORKLOAD_REPLICASET_LEGACY_ANNOTATION))
}

/// The workload's name across replicas: [`replica_group`], else its
/// application name annotation, else `name`.
pub(crate) fn stable_name<'a>(annotations: &'a HashMap<String, String>, name: &'a str) -> &'a str {
    replica_group(annotations)
        .or_else(|| annotation(annotations, K8S_APP_NAME_ANNOTATION))
        .unwrap_or(name)
}

/// A fully resolved workload ready for execution.
///
/// A `ResolvedWorkload` contains all components that have been validated,
/// bound to plugins, and had their dependencies resolved. This is the final
/// state of a workload before execution.
#[derive(Clone)]
pub struct ResolvedWorkload {
    /// Engine-local adapters shared by linking and every store built for this workload.
    stream_payloads: Arc<super::stream_payload::StreamPayloads>,
    /// The unique identifier of the workload, supplied by whoever starts it
    id: Arc<str>,
    /// The name of the workload
    name: Arc<str>,
    /// The namespace of the workload
    namespace: Arc<str>,
    /// All components in the workload. Behind a `RwLock` because resolving
    /// writes to them (binding and linking, then sealing each linker) while
    /// the cleanup snapshot shares them; resolved, they are only read.
    components: Arc<RwLock<BTreeMap<Arc<str>, WorkloadComponent>>>,
    /// How many entries `components` holds, which never changes once resolved.
    component_count: usize,
    /// The HTTP handler for outgoing HTTP requests. See
    /// [`crate::host::http::live_handler`] for why this is weak and how the
    /// two kinds of caller differ.
    http_handler: crate::host::HostRef,
    /// The meter of the host running this workload, stamped onto every store
    /// built for it. Reaches the engine the same way `http_handler` does,
    /// because a store has no other way back to the host that owns it.
    invocation: crate::observability::InvocationMeter,
    /// An optional service component that runs once to completion or for the duration of the workload
    ///
    /// Shared so workload clones reuse the service metadata and cached link.
    /// The few writes after resolution go through `Arc::make_mut`,
    /// so a clone taken earlier keeps what it saw, as a by-value copy did.
    service: Option<Arc<WorkloadService>>,
    /// The ingress plugin-dispatched calls reach the service on, once one has
    /// claimed it as a [`DispatchTarget`]. Present whether or not the workload
    /// has a service — an unclaimed one costs a lock and an empty option.
    service_calls: Arc<ServiceCalls>,
    /// Set when the workload is being torn down. A [`DispatchTarget`] outlives
    /// the workload it names — a plugin holds one until it is unbound, and is
    /// unbound only after teardown has begun — so a dispatch already on its way
    /// checks this rather than instantiating into a workload that is going away.
    released: Arc<std::sync::atomic::AtomicBool>,
    /// The requested host [`WitInterface`]s to resolve this workload
    /// Shared, because a resolved workload is cloned per request and per
    /// plugin binding and never changes these.
    host_interfaces: Arc<Vec<WitInterface>>,
    /// Native bindings retained after a successful start so a cancelled stop
    /// can resume the same per-plugin teardown journal.
    teardown_bindings: Arc<[PendingBinding]>,
    /// TLS provider override for `wasi:tls` client connections in this workload.
    #[cfg(feature = "wasi-tls")]
    tls_provider: Option<SharedTlsProvider>,
    /// Where this workload's `emptyDir` volumes live; removed on release.
    scratch: Option<Arc<crate::engine::scratch::ScratchDir>>,
}

impl std::fmt::Debug for ResolvedWorkload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("ResolvedWorkload");
        s.field("id", &self.id)
            .field("name", &self.name)
            .field("namespace", &self.namespace)
            .field("host_interfaces", &self.host_interfaces)
            .field("has_service", &self.service.is_some())
            .field("component_count", &self.component_count);
        #[cfg(feature = "wasi-tls")]
        s.field("tls_provider", &self.tls_provider.is_some());
        s.finish_non_exhaustive()
    }
}

/// An unresolved workload that has been initialized but not yet bound to plugins.
///
/// An `UnresolvedWorkload` represents a workload that has been validated and compiled
/// but has not yet been bound to host plugins or had its dependencies resolved.
/// This is an intermediate state in the workload lifecycle before becoming a
/// [`ResolvedWorkload`] that can be executed.
///
/// # Lifecycle
///
/// 1. **Creation**: Built from a [`Workload`] specification via [`Engine::initialize_workload`]
/// 2. **Plugin Binding**: Components are bound to required host plugins
/// 3. **Resolution**: Dependencies are resolved and the workload becomes [`ResolvedWorkload`]
/// 4. **Execution**: The resolved workload can create component instances and handle requests
///
/// # Plugin Resolution
///
/// During resolution, the workload will:
/// - Match required interfaces with available plugins
/// - Configure component linkers with plugin implementations
/// - Validate that all dependencies can be satisfied
/// - Create the final executable workload representation
pub struct UnresolvedWorkload {
    /// Preserved through resolution so signature checks and stores use the same adapters.
    stream_payloads: Arc<super::stream_payload::StreamPayloads>,
    /// The unique identifier of the workload, supplied by whoever starts it
    id: Arc<str>,
    /// The name of the workload
    name: Arc<str>,
    /// The namespace of the workload
    namespace: Arc<str>,
    /// The requested host [`WitInterface`]s to resolve this workload
    host_interfaces: Vec<WitInterface>,
    /// The [`WorkloadService`] associated with this workload, if any
    service: Option<WorkloadService>,
    /// All [`WorkloadComponent`]s in the workload
    components: BTreeMap<Arc<str>, WorkloadComponent>,
    /// Component IDs in manifest order. Used to pick the first in the manifest
    /// whenever the host can dispatch an export to only one component
    component_order: Vec<Arc<str>>,
    /// TLS provider override for `wasi:tls` client connections in this workload.
    #[cfg(feature = "wasi-tls")]
    tls_provider: Option<SharedTlsProvider>,
    /// Where this workload's `emptyDir` volumes live, if it has any.
    scratch: Option<Arc<crate::engine::scratch::ScratchDir>>,
    /// The name the orchestrator gave its replicas, if any. See
    /// [`replica_group`].
    replica_group: Option<Arc<str>>,
}

type PendingBinding = (Arc<dyn HostPlugin>, HashSet<WitInterface>);

impl ResolvedWorkload {
    /// Access the resolved components and their metadata.
    ///
    /// The map remains writable for metadata updates, but resolution seals
    /// each linker: [`WorkloadMetadata::linker`] then returns an error.
    /// Adding or removing components after resolution is unsupported: the
    /// component count and bindings are settled when the workload is resolved.
    pub fn components(&self) -> Arc<RwLock<BTreeMap<Arc<str>, WorkloadComponent>>> {
        self.components.clone()
    }

    pub fn host_interfaces(&self) -> &Vec<WitInterface> {
        &self.host_interfaces
    }

    /// Gets the unique identifier of the workload
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The host-side handler this workload's ingress registers with (the HTTP
    /// server / trigger-service messaging registry). Lets a host plugin (e.g.
    /// the messaging subscriber) deliver an inbound message to a long-lived
    /// trigger-service instance instead of instantiating a component per
    /// message.
    ///
    /// Held weakly, so this fails once the host that owns the handler is gone.
    /// For a caller that would rather skip than fail, see
    /// [`Self::try_http_handler`].
    pub fn http_handler(&self) -> anyhow::Result<Arc<dyn crate::host::http::HostHandler>> {
        self.try_http_handler()
            .ok_or_else(|| anyhow::anyhow!("host HTTP handler is no longer available"))
    }

    /// [`Self::http_handler`] for a path that has something else to do when the
    /// host is gone — a message still deliverable to a component of its own,
    /// say. No error is built for an answer the caller only branches on.
    pub fn try_http_handler(&self) -> Option<Arc<dyn crate::host::http::HostHandler>> {
        self.http_handler.handler()
    }

    /// Gets the name of the workload
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Gets the namespace of the workload
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Id of this workload's long-lived service, if it has one. A plugin is
    /// handed item ids without being told which kind each is; this is how one
    /// tells the service apart from a component.
    pub fn service_id(&self) -> Option<&str> {
        self.service.as_ref().map(|s| s.id())
    }

    /// Returns the number of components in this workload.
    /// Does not include the service component if one is defined.
    ///
    /// Read without the components lock: the set is fixed once resolved, and
    /// the host heartbeat asks every running workload for it.
    pub fn component_count(&self) -> usize {
        self.component_count
    }
}

impl UnresolvedWorkload {
    /// Creates a new unresolved workload from its constituent parts.
    ///
    /// # Arguments
    /// * `id` - Unique identifier for this workload instance
    /// * `name` - Human-readable name of the workload
    /// * `namespace` - Namespace for workload organization
    /// * `engine` - The WebAssembly engine for compilation and execution
    /// * `service` - Optional long-running service component
    /// * `components` - Iterator of components that make up this workload
    /// * `host_interfaces` - Required WIT interfaces that must be provided by host plugins
    ///
    /// # Returns
    /// A new `UnresolvedWorkload` ready for plugin binding and resolution.
    pub fn new(
        id: impl Into<Arc<str>>,
        name: impl Into<Arc<str>>,
        namespace: impl Into<Arc<str>>,
        service: Option<WorkloadService>,
        components: impl IntoIterator<Item = WorkloadComponent>,
        host_interfaces: Vec<WitInterface>,
    ) -> Self {
        let mut component_order = Vec::new();
        Self {
            stream_payloads: Arc::default(),
            id: id.into(),
            name: name.into(),
            namespace: namespace.into(),
            service,
            components: components
                .into_iter()
                .map(|c| {
                    let id: Arc<str> = Arc::from(c.id());
                    component_order.push(id.clone());
                    (id, c)
                })
                .collect(),
            component_order,
            host_interfaces,
            #[cfg(feature = "wasi-tls")]
            tls_provider: None,
            scratch: None,
            replica_group: None,
        }
    }

    /// Use the engine's stream/future payload adapters for linking and store construction.
    ///
    /// [`super::Engine::initialize_workload`] sets this automatically. When building
    /// a workload directly, pass a clone of [`super::Engine::stream_payloads`].
    /// Register payload types before resolving the workload or loading plugins.
    pub fn with_stream_payloads(
        mut self,
        stream_payloads: Arc<super::stream_payload::StreamPayloads>,
    ) -> Self {
        self.stream_payloads = stream_payloads;
        self
    }

    pub(crate) fn with_scratch(
        mut self,
        scratch: Option<Arc<crate::engine::scratch::ScratchDir>>,
    ) -> Self {
        self.scratch = scratch;
        self
    }

    /// Take the name the orchestrator gave its replicas from `annotations`.
    /// See [`replica_group`].
    pub(crate) fn with_annotations(mut self, annotations: &HashMap<String, String>) -> Self {
        self.replica_group = replica_group(annotations).map(Arc::from);
        self
    }

    /// The name it keeps across replicas, what a plugin secret's grant
    /// matches: the orchestrator's for its replicas, else its own.
    pub fn grant_name(&self) -> &str {
        self.replica_group.as_deref().unwrap_or(&self.name)
    }

    /// Override the TLS provider used for `wasi:tls` client connections in this workload.
    ///
    /// Use this to plug in an alternative TLS backend, install a custom root
    /// certificate store (corporate CAs, certificate pinning), or integrate
    /// with HSM-backed key material.
    #[cfg(feature = "wasi-tls")]
    pub fn with_tls_provider(mut self, provider: SharedTlsProvider) -> Self {
        self.tls_provider = Some(provider);
        self
    }

    /// Apply an optional TLS provider override. No-op when `None`.
    #[cfg(feature = "wasi-tls")]
    pub fn maybe_with_tls_provider(self, provider: Option<SharedTlsProvider>) -> Self {
        match provider {
            Some(p) => self.with_tls_provider(p),
            None => self,
        }
    }

    /// Gets the unique identifier of the workload
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Gets the name of the workload
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Gets the namespace of the workload
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Id of this workload's service, if it has one.
    pub fn service_id(&self) -> Option<Arc<str>> {
        self.service.as_ref().map(|s| Arc::from(s.id()))
    }

    /// Ids of this workload's components, in sorted order. Excludes the
    /// service — see [`Self::service_id`].
    pub fn component_ids(&self) -> Vec<Arc<str>> {
        self.components.keys().cloned().collect()
    }

    /// Retrieves the interface configuration for a given WIT interface, if it exists.
    pub fn interface_config(&self, interface: &WitInterface) -> Option<&crate::wit::Config> {
        self.host_interfaces
            .iter()
            .find(|i| i.contains(interface))
            .map(|i| &i.config)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "workload_supervision_tests.rs"]
mod supervision_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "workload_wasi_resource_tests.rs"]
mod wasi_resource_tests;

#[cfg(test)]
#[path = "workload_lifecycle_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "workload_contract_tests.rs"]
mod contract_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "workload_runtime_tests.rs"]
mod runtime_tests;
