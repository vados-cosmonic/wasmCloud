use super::*;

#[test]
fn plugin_bound_instances_match_linker_import_names() {
    let plain = WitInterface::from("wasi:keyvalue/store,atomics@0.2.0");
    assert_eq!(
        plugin_bound_instance_names(&plain)
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([
            "wasi:keyvalue/store@0.2.0".to_string(),
            "wasi:keyvalue/atomics@0.2.0".to_string(),
        ])
    );

    let mut named = plain;
    named.name = Some("primary".to_string());
    assert_eq!(plugin_bound_instance_names(&named), vec!["primary"]);
}

#[test]
fn plugin_bound_import_is_not_linked_again() {
    let interface = WitInterface::from("test:marker/api@1.0.0");
    let bound = plugin_bound_instance_names(&interface)
        .into_iter()
        .collect::<HashSet<_>>();
    assert!(!should_link_component_import(
        "test:marker/api@1.0.0",
        &bound
    ));
    assert!(should_link_component_import("test:other/api@1.0.0", &bound));
}
use crate::plugin::HostPlugin;
use crate::wit::{WitInterface, WitWorld};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wasmtime::component::{Component, Linker};

/// Records a single plugin method call for testing callback order and parameters.
#[derive(Debug, Clone)]
struct CallRecord {
    #[allow(unused)]
    plugin_id: String,
    method: String,
    component_id: Option<String>,
    #[allow(unused)]
    interfaces: Vec<String>,
}

/// Mock plugin implementation for testing workload binding behavior.
/// Tracks all method calls and counts for verification of callback order and frequency.
struct MockPlugin {
    id: &'static str,
    world: WitWorld,
    call_records: Arc<Mutex<Vec<CallRecord>>>,
    on_workload_bind_count: Arc<AtomicUsize>,
    on_workload_item_bind_count: Arc<AtomicUsize>,
    on_workload_resolved_count: Arc<AtomicUsize>,
    named_instance_support: bool,
    /// Serves labeled and unlabeled imports off one backend, so it never
    /// defers a plain import to a single-backend plugin.
    serves_unnamed_too: bool,
    /// Minimum version this plugin will claim, mirroring a plugin whose
    /// `world()` cannot express the constraint. `None` claims everything.
    claims_from: Option<semver::Version>,
    /// Keys this plugin declares host-owned in code.
    host_owned: Vec<&'static str>,
    /// Every interface handed to `on_workload_bind` /
    /// `on_workload_item_bind`, with the config the plugin actually saw.
    bound_interfaces: Arc<Mutex<Vec<WitInterface>>>,
    /// WASI interfaces this plugin claims to redefine.
    wasi_overrides: WitWorld,
    /// Serves plugin-backed `secrets:` entries naming this plugin.
    secret_source: Option<Arc<dyn crate::plugin::HostSecretSource>>,
}

impl MockPlugin {
    /// Creates a new mock plugin with the specified interfaces it can import/export.
    fn new(id: &'static str, imports: Vec<WitInterface>, exports: Vec<WitInterface>) -> Self {
        Self {
            id,
            world: WitWorld {
                imports: imports.into_iter().collect(),
                exports: exports.into_iter().collect(),
            },
            call_records: Arc::new(Mutex::new(Vec::new())),
            on_workload_bind_count: Arc::new(AtomicUsize::new(0)),
            on_workload_item_bind_count: Arc::new(AtomicUsize::new(0)),
            on_workload_resolved_count: Arc::new(AtomicUsize::new(0)),
            named_instance_support: false,
            serves_unnamed_too: false,
            claims_from: None,
            host_owned: Vec::new(),
            bound_interfaces: Arc::new(Mutex::new(Vec::new())),
            wasi_overrides: WitWorld::default(),
            secret_source: None,
        }
    }

    /// Serve plugin secrets through `source`, the way a component plugin
    /// exporting `wasmcloud:host/secret-source` does.
    fn serving_secrets(mut self, source: Arc<dyn crate::plugin::HostSecretSource>) -> Self {
        self.secret_source = Some(source);
        self
    }
    /// Declare that this plugin redefines `interfaces` (a WASI entry such
    /// as `wasi:filesystem/preopens,types@0.2.0`), the way a plugin
    /// shaping a filesystem view does.
    fn overriding(mut self, interfaces: &str) -> Self {
        self.wasi_overrides
            .imports
            .insert(WitInterface::from(interfaces));
        self
    }

    /// Declare `keys` host-owned in code, the way a real plugin's
    /// `binding_schema()` does.
    fn owning(mut self, keys: &[&'static str]) -> Self {
        self.host_owned = keys.to_vec();
        self
    }

    /// The config this plugin saw for `binding`, from whichever callback
    /// ran first.
    fn seen_config(&self, binding: Option<&str>) -> crate::wit::Config {
        self.bound_interfaces
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.name.as_deref() == binding)
            .unwrap_or_else(|| panic!("plugin never saw a binding named {binding:?}"))
            .config
            .clone()
    }

    fn with_named_instance_support(mut self) -> Self {
        self.named_instance_support = true;
        self
    }

    /// Serves labeled and unlabeled imports off one backend, the way a host
    /// component plugin does, so it never defers a plain import.
    fn serving_unnamed_too(mut self) -> Self {
        self.serves_unnamed_too = true;
        self
    }

    /// Claim only interfaces pinning a version at or above `min`, the way
    /// the async messaging multiplexer claims only `>= 0.3.0`.
    fn claiming_from(mut self, min: &str) -> Self {
        self.claims_from = Some(semver::Version::parse(min).unwrap());
        self
    }

    /// Returns the number of times the specified method was called.
    fn get_call_count(&self, method: &str) -> usize {
        match method {
            "on_workload_bind" => self.on_workload_bind_count.load(Ordering::SeqCst),
            "on_workload_item_bind" => self.on_workload_item_bind_count.load(Ordering::SeqCst),
            "on_workload_resolved" => self.on_workload_resolved_count.load(Ordering::SeqCst),
            _ => 0,
        }
    }

    /// Returns all recorded method calls in chronological order.
    fn get_call_records(&self) -> Vec<CallRecord> {
        self.call_records.lock().unwrap().clone()
    }
}

#[async_trait]
impl HostPlugin for MockPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn world(&self) -> WitWorld {
        self.world.clone()
    }

    fn supports_named_instances(&self) -> bool {
        self.named_instance_support
    }

    fn defers_unnamed_instances(&self) -> bool {
        self.named_instance_support && !self.serves_unnamed_too
    }

    fn claims(&self, interface: &WitInterface) -> bool {
        match &self.claims_from {
            Some(min) => interface.version.as_ref().is_some_and(|v| v >= min),
            None => true,
        }
    }

    fn wasi_overrides(&self) -> WitWorld {
        self.wasi_overrides.clone()
    }

    fn binding_schema(&self) -> crate::plugin::BindingSchema {
        crate::plugin::BindingSchema::with_host_owned_keys(&self.host_owned)
    }

    fn secret_source(&self) -> Option<Arc<dyn crate::plugin::HostSecretSource>> {
        self.secret_source.clone()
    }

    async fn on_workload_bind(
        &self,
        _workload: &UnresolvedWorkload,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        self.on_workload_bind_count.fetch_add(1, Ordering::SeqCst);
        self.bound_interfaces
            .lock()
            .unwrap()
            .extend(interfaces.iter().cloned());
        self.call_records.lock().unwrap().push(CallRecord {
            plugin_id: self.id.to_string(),
            method: "on_workload_bind".to_string(),
            component_id: None,
            interfaces: interfaces.iter().map(|i| i.to_string()).collect(),
        });
        Ok(())
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        self.on_workload_item_bind_count
            .fetch_add(1, Ordering::SeqCst);
        self.bound_interfaces
            .lock()
            .unwrap()
            .extend(interfaces.iter().cloned());
        self.call_records.lock().unwrap().push(CallRecord {
            plugin_id: self.id.to_string(),
            method: "on_workload_item_bind".to_string(),
            component_id: Some(item.id().to_string()),
            interfaces: interfaces.iter().map(|i| i.to_string()).collect(),
        });
        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        workload_id: &str,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        self.call_records.lock().unwrap().push(CallRecord {
            plugin_id: self.id.to_string(),
            method: "on_workload_unbind".to_string(),
            component_id: Some(workload_id.to_string()),
            interfaces: interfaces.iter().map(|i| i.to_string()).collect(),
        });
        Ok(())
    }

    async fn on_workload_resolved(
        &self,
        _workload: &ResolvedWorkload,
        component_id: &str,
    ) -> anyhow::Result<()> {
        self.on_workload_resolved_count
            .fetch_add(1, Ordering::SeqCst);
        self.call_records.lock().unwrap().push(CallRecord {
            plugin_id: self.id.to_string(),
            method: "on_workload_resolved".to_string(),
            component_id: Some(component_id.to_string()),
            interfaces: Vec::new(),
        });
        Ok(())
    }
}

struct RetriedHttpCleanup {
    workload_unbinds: AtomicUsize,
    service_http_unbinds: AtomicUsize,
    messaging_unbinds: AtomicUsize,
    service_http_entered: tokio::sync::Notify,
    service_http_gate: tokio::sync::Semaphore,
    fail_service_http: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl crate::host::http::HostHandler for RetriedHttpCleanup {
    async fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn port(&self) -> u16 {
        0
    }

    async fn on_workload_resolved(
        &self,
        _workload: &ResolvedWorkload,
        _component_id: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn on_workload_unbind(&self, _workload_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.workload_unbinds.fetch_add(1, Ordering::SeqCst) == 0,
            "workload already unbound"
        );
        Ok(())
    }

    async fn on_service_http_unbind(&self, _workload_id: &str) -> anyhow::Result<()> {
        self.service_http_unbinds.fetch_add(1, Ordering::SeqCst);
        self.service_http_entered.notify_one();
        let permit = self.service_http_gate.acquire().await?;
        permit.forget();
        anyhow::ensure!(
            !self.fail_service_http.swap(false, Ordering::SeqCst),
            "service HTTP cleanup failed once"
        );
        Ok(())
    }

    async fn on_trigger_service_messaging_unbind(&self, _workload_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.messaging_unbinds.fetch_add(1, Ordering::SeqCst) == 0,
            "messaging already unbound"
        );
        Ok(())
    }

    fn outgoing_request(
        &self,
        workload_id: &str,
        request: hyper::Request<wasmtime_wasi_http::WasiBody>,
        options: Option<wasmtime_wasi_http::RequestOptions>,
        fut: crate::host::http::RequestIoFuture,
        allowed_hosts: &[crate::host::allowed_hosts::AllowedHost],
    ) -> crate::host::http::SendFuture {
        crate::host::http::NullServer::default().outgoing_request(
            workload_id,
            request,
            options,
            fut,
            allowed_hosts,
        )
    }
}

#[tokio::test]
async fn start_cleanup_retries_only_unfinished_http_hooks() -> anyhow::Result<()> {
    for cancel in [false, true] {
        let handler = Arc::new(RetriedHttpCleanup {
            workload_unbinds: AtomicUsize::new(0),
            service_http_unbinds: AtomicUsize::new(0),
            messaging_unbinds: AtomicUsize::new(0),
            service_http_entered: tokio::sync::Notify::new(),
            service_http_gate: tokio::sync::Semaphore::new(if cancel { 0 } else { 2 }),
            fail_service_http: std::sync::atomic::AtomicBool::new(!cancel),
        });
        let engine = wasmtime::Engine::default();
        let service = WorkloadService::new(
            "http-cleanup",
            "http-cleanup",
            "test",
            Component::new(&engine, wat::parse_str("(component)")?)?,
            Linker::new(&engine),
            vec![],
            LocalResources::default(),
            0,
            Arc::default(),
        );
        let host_handler: Arc<dyn crate::host::http::HostHandler> = handler.clone();
        let resolved = UnresolvedWorkload::new(
            "http-cleanup",
            "http-cleanup",
            "test",
            Some(service),
            [],
            vec![],
        )
        .resolve(
            None,
            &crate::plugin::PluginBindings::default(),
            &crate::host::HostRef::from_handler(&host_handler),
            &crate::observability::Meters::default(),
        )
        .await?;
        let resources = WorkloadResources::default();
        resources.resolved(&resolved);
        if cancel {
            let task = tokio::spawn({
                let resources = resources.clone();
                async move { resources.release("http-cleanup").await }
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                handler.service_http_entered.notified(),
            )
            .await?;
            assert_eq!(handler.workload_unbinds.load(Ordering::SeqCst), 1);
            task.abort();
            assert!(task.await.is_err());
            handler.service_http_gate.add_permits(1);
        } else {
            assert!(resources.release("http-cleanup").await.is_err());
            assert_eq!(handler.messaging_unbinds.load(Ordering::SeqCst), 1);
        }
        resources.release("http-cleanup").await?;
        resources.release("http-cleanup").await?;
        assert_eq!(handler.workload_unbinds.load(Ordering::SeqCst), 1);
        assert_eq!(handler.service_http_unbinds.load(Ordering::SeqCst), 2);
        assert_eq!(handler.messaging_unbinds.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

/// Load a test fixture wasm file at runtime rather than compile time.
/// This avoids requiring fixture wasm files during `cargo build` — they're
/// only needed when tests actually run.
fn load_fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/wasm/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path} not found: {e}"))
}

fn http_counter_wasm() -> Vec<u8> {
    load_fixture("http_counter.wasm")
}

fn messaging_handler_wasm() -> Vec<u8> {
    load_fixture("messaging_handler.wasm")
}

fn service_wasm() -> Vec<u8> {
    load_fixture("cpu_usage_service.wasm")
}
/// Creates a test component using the http_counter fixture.
/// This provides a real component with actual WIT interface imports.
fn create_test_component(id: &str) -> WorkloadComponent {
    let engine = wasmtime::Engine::default();
    let linker = Linker::new(&engine);

    // Use the actual http_counter fixture component
    let wasm = http_counter_wasm();
    let component = Component::new(&engine, &wasm).unwrap();

    let local_resources = LocalResources::default();

    WorkloadComponent::new(
        format!("workload-{id}"),
        format!("test-workload-{id}"),
        "test-namespace".to_string(),
        "test-component".to_string(),
        component,
        linker,
        Vec::new(),
        local_resources,
        Arc::default(),
        InstancePolicy::Ephemeral,
    )
}

/// A component built from WAT, so a test can state an exact import/export
/// shape rather than take whatever a fixture happens to have.
fn component_from_wat(id: &str, wat: &str) -> WorkloadComponent {
    #[cfg(feature = "wasi-tls")]
    crate::init_crypto();

    let engine = wasmtime::Engine::default();
    let linker = Linker::new(&engine);
    let wasm = wat::parse_str(wat).expect("failed to parse WAT");
    let component = Component::new(&engine, &wasm).expect("failed to compile");

    WorkloadComponent::new(
        "workload-local-first".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        id.to_string(),
        component,
        linker,
        Vec::new(),
        LocalResources::default(),
        Arc::default(),
        InstancePolicy::Ephemeral,
    )
}

const MARKER: &str = "test:probe/marker@0.1.0";

/// The linked component is kept once asked for, and forgotten whenever the
/// linker is taken mutably, so a link added afterwards is never missed.
#[test]
fn pre_instantiate_ref_is_kept_until_the_linker_changes() -> anyhow::Result<()> {
    let mut component = component_from_wat(
        "pre",
        r#"(component
                 (import "test:pre/iface" (instance (export "f" (func))))
               )"#,
    );
    assert!(
        component.pre_instantiate_ref().is_err(),
        "the import is not linked yet"
    );
    assert!(!kept_link(&component), "a failure is not kept");

    component
        .metadata
        .linker()?
        .instance("test:pre/iface")
        .unwrap()
        .func_wrap("f", |_, (): ()| Ok(()))
        .unwrap();
    component.pre_instantiate_ref().unwrap();
    assert!(kept_link(&component));
    component.pre_instantiate_ref().unwrap();

    component.metadata.linker()?;
    assert!(!kept_link(&component));
    Ok(())
}

/// Components share the linker they start from until one is written to;
/// the write lands on a copy of that one component's own.
#[test]
fn a_shared_linker_is_copied_only_when_written() -> anyhow::Result<()> {
    let mut a = component_from_wat(
        "a",
        r#"(component
                (import "test:cow/iface" (instance $iface
                    (export "f" (func (result u32)))))
                (alias export $iface "f" (func $f))
                (core func $f (canon lower (func $f)))
                (core module $forward
                    (import "host" "f" (func $f (result i32)))
                    (func (export "call") (result i32) call $f))
                (core instance $forward (instantiate $forward
                    (with "host" (instance (export "f" (func $f))))))
                (func (export "call") (result u32)
                    (canon lift (core func $forward "call")))
            )"#,
    );
    let shared = Arc::clone(linker_of(&a));
    let mut b = a.clone();
    assert!(Arc::ptr_eq(linker_of(&a), linker_of(&b)));
    assert!(a.pre_instantiate_ref().is_err());

    b.metadata
        .linker()?
        .instance("test:cow/iface")
        .unwrap()
        .func_wrap("f", |_, (): ()| Ok((2_u32,)))
        .unwrap();
    assert!(b.pre_instantiate_ref().is_ok());
    assert!(
        a.pre_instantiate_ref().is_err(),
        "binding one component must not satisfy another component's import"
    );
    assert!(
        shared.instantiate_pre(a.metadata.component()).is_err(),
        "the shared base must not inherit a component's binding"
    );
    assert!(!Arc::ptr_eq(linker_of(&a), linker_of(&b)));
    assert!(
        Arc::ptr_eq(linker_of(&a), &shared),
        "the component not written to keeps the shared linker"
    );
    a.metadata
        .linker()?
        .instance("test:cow/iface")
        .unwrap()
        .func_wrap("f", |_, (): ()| Ok((1_u32,)))
        .unwrap();
    assert!(
        !Arc::ptr_eq(linker_of(&a), &shared),
        "taking it mutably while shared copies it"
    );

    let call = |component: &WorkloadComponent| {
        let ctx = crate::engine::ctx::Ctx::builder("cow", component.id()).build();
        let mut store = wasmtime::Store::new(component.metadata.engine(), SharedCtx::new(ctx));
        let instance = component
            .pre_instantiate_ref()
            .unwrap()
            .instantiate(&mut store)
            .unwrap();
        let func = instance
            .get_typed_func::<(), (u32,)>(&mut store, "call")
            .unwrap();
        let (value,) = func.call(&mut store, ()).unwrap();
        value
    };
    assert_eq!(call(&a), 1);
    assert_eq!(call(&b), 2, "the first component's binding must survive");
    assert!(shared.instantiate_pre(a.metadata.component()).is_err());
    Ok(())
}

fn marker_importer(id: &str) -> WorkloadComponent {
    component_from_wat(
        id,
        &format!(r#"(component (import "{MARKER}" (instance)))"#),
    )
}

/// Sealing keeps the linked component, frees the linker copy the binding
/// made, and turns a late write into a refusal; a component that does not
/// link is sealed the same way, keeping only its error.
#[test]
fn sealing_frees_the_linker_copy_and_keeps_the_link() -> anyhow::Result<()> {
    let mut a = component_from_wat(
        "a",
        r#"(component
                (import "test:cow/iface" (instance $iface
                    (export "f" (func (result u32)))))
                (alias export $iface "f" (func $f))
                (core func $f (canon lower (func $f)))
                (core module $forward
                    (import "host" "f" (func $f (result i32)))
                    (func (export "call") (result i32) call $f))
                (core instance $forward (instantiate $forward
                    (with "host" (instance (export "f" (func $f))))))
                (func (export "call") (result u32)
                    (canon lift (core func $forward "call")))
            )"#,
    );
    let call = |component: &WorkloadComponent| {
        let ctx = crate::engine::ctx::Ctx::builder("seal", component.id()).build();
        let mut store = wasmtime::Store::new(component.metadata.engine(), SharedCtx::new(ctx));
        let instance = component
            .pre_instantiate_ref()
            .unwrap()
            .instantiate(&mut store)
            .unwrap();
        let func = instance
            .get_typed_func::<(), (u32,)>(&mut store, "call")
            .unwrap();
        let (value,) = func.call(&mut store, ()).unwrap();
        value
    };

    a.metadata
        .linker()?
        .instance("test:cow/iface")
        .unwrap()
        .func_wrap("f", |_, (): ()| Ok((3_u32,)))
        .unwrap();
    let copy = Arc::downgrade(linker_of(&a));
    assert!(a.metadata.seal_linker());
    assert!(a.metadata.is_sealed());
    assert!(
        matches!(a.metadata.link, LinkState::Linked(_)),
        "the linker is let go"
    );
    assert!(
        copy.upgrade().is_none(),
        "the linker copy is freed once the link is kept"
    );
    assert!(kept_link(&a));
    assert_eq!(call(&a), 3);
    assert_eq!(call(&a), 3, "the kept link survives use");

    let error = a
        .metadata
        .linker()
        .err()
        .expect("sealed linker must reject writes");
    assert!(error.to_string().contains("sealed"));
    assert!(error.to_string().contains(a.id()));
    assert!(error.to_string().contains(a.metadata.workload_id()));
    assert!(kept_link(&a));
    assert_eq!(call(&a), 3, "a rejected write preserves the kept link");

    assert!(a.metadata.seal_linker(), "sealing again is a no-op");

    // A component that does not link is sealed all the same, keeps the
    // error it failed with, and refuses late writes like its siblings.
    // Something wrote to its linker, so there is a copy to let go.
    let mut unlinked = component_from_wat(
        "unlinked",
        r#"(component
                 (import "test:pre/iface" (instance (export "f" (func))))
               )"#,
    );
    unlinked
        .metadata
        .linker()?
        .instance("test:other/iface")
        .unwrap();
    let before = unlinked
        .pre_instantiate_ref()
        .err()
        .expect("missing import must fail before sealing");
    assert!(!unlinked.metadata.seal_linker());
    assert!(unlinked.metadata.is_sealed());
    assert!(matches!(unlinked.metadata.link, LinkState::Unlinked(_)));
    let after = unlinked
        .pre_instantiate_ref()
        .err()
        .expect("missing import must still fail after sealing");
    assert_eq!(
        format!("{after:#}"),
        format!("{before:#}"),
        "wasmtime's own error, as before"
    );
    assert!(unlinked.metadata.linker().is_err());
    assert!(
        !unlinked.metadata.seal_linker(),
        "sealing again still has no link"
    );
    Ok(())
}

/// A component nothing wrote to still shares the engine's linker, so
/// sealing keeps it, makes no link, and frees nothing; the link is made on
/// first use, as before sealing.
#[test]
fn sealing_an_untouched_component_keeps_the_shared_linker_and_links_lazily() {
    let mut untouched = component_from_wat(
        "untouched",
        r#"(component
                (core module $m (func (export "f")))
                (core instance (instantiate $m)))"#,
    );
    let shared = Arc::clone(linker_of(&untouched));
    assert!(!untouched.metadata.seal_linker(), "no copy to let go");
    assert!(untouched.metadata.is_sealed());
    assert!(untouched.metadata.linker().is_err());
    match &untouched.metadata.link {
        LinkState::Shared { linker, pre } => {
            assert!(Arc::ptr_eq(linker, &shared), "the shared linker is kept");
            assert!(pre.get().is_none(), "sealing made no link");
        }
        _ => panic!("an untouched component seals as shared"),
    }
    untouched.pre_instantiate_ref().unwrap();
    assert!(kept_link(&untouched), "the first use links it");
    assert!(untouched.metadata.linker().is_err());
}

/// The linker a component holds, for the tests that watch it being
/// shared, copied and let go.
fn linker_of(component: &WorkloadComponent) -> &Arc<Linker<SharedCtx>> {
    open_linker_of(&component.metadata)
}

fn open_linker_of(metadata: &WorkloadMetadata) -> &Arc<Linker<SharedCtx>> {
    match &metadata.link {
        LinkState::Open { linker, .. } => linker,
        LinkState::Shared { .. } | LinkState::Linked(_) | LinkState::Unlinked(_) => {
            panic!("an unsealed component keeps its linker")
        }
    }
}

/// Whether a link is kept: cached while open, or standing once sealed.
fn kept_link(component: &WorkloadComponent) -> bool {
    match &component.metadata.link {
        LinkState::Open { pre, .. } | LinkState::Shared { pre, .. } => pre.get().is_some(),
        LinkState::Linked(_) => true,
        LinkState::Unlinked(_) => false,
    }
}

#[tokio::test]
async fn sealing_service_linkers_preserves_sibling_imports() -> anyhow::Result<()> {
    for with_cleanup in [false, true] {
        let engine = wasmtime::Engine::default();
        let service = WorkloadService::new(
            "sealed-service",
            "sealed-service",
            "test",
            Component::new(
                &engine,
                wat::parse_str(
                    r#"(component
                            (import "test:service/probe" (instance (export "probe" (func)))))"#,
                )?,
            )?,
            Linker::new(&engine),
            vec![],
            LocalResources::default(),
            1,
            Arc::default(),
        );
        let original_linker = Arc::downgrade(open_linker_of(&service.metadata));
        let callee = WorkloadComponent::new(
            "sealed-service",
            "sealed-service",
            "test",
            "callee",
            Component::new(
                &engine,
                wat::parse_str(
                    r#"(component
                            (core module $m (func (export "probe")))
                            (core instance $m (instantiate $m))
                            (func $probe (canon lift (core func $m "probe")))
                            (instance $api (export "probe" (func $probe)))
                            (export "test:service/probe" (instance $api)))"#,
                )?,
            )?,
            Linker::new(&engine),
            vec![],
            LocalResources::default(),
            Arc::default(),
            InstancePolicy::Ephemeral,
        );
        let callee_id = Arc::clone(&callee.metadata.id);
        // Linking writes the callee's export into the importer's linker, so
        // it holds a copy that sealing lets go; `broken` also imports
        // something nothing provides, so it seals without a link.
        let sibling = |id: &str, extra: &str| -> anyhow::Result<WorkloadComponent> {
            Ok(WorkloadComponent::new(
                "sealed-service",
                "sealed-service",
                "test",
                id,
                Component::new(
                    &engine,
                    wat::parse_str(format!(
                        r#"(component
                                (import "test:service/probe" (instance (export "probe" (func))))
                                {extra})"#
                    ))?,
                )?,
                Linker::new(&engine),
                vec![],
                LocalResources::default(),
                Arc::default(),
                InstancePolicy::Ephemeral,
            ))
        };
        let importer = sibling("importer", "")?;
        let importer_linker = Arc::downgrade(open_linker_of(&importer.metadata));
        let importer_id = Arc::clone(&importer.metadata.id);
        let broken = sibling(
            "broken",
            r#"(import "test:missing/iface" (instance (export "f" (func))))"#,
        )?;
        let broken_id = Arc::clone(&broken.metadata.id);
        let cleanup = with_cleanup.then(WorkloadResources::default);
        let resolved = UnresolvedWorkload::new(
            "sealed-service",
            "sealed-service",
            "test",
            Some(service),
            [callee, importer, broken],
            vec![],
        )
        .resolve_for_start(
            None,
            &Default::default(),
            &crate::host::HostRef::detached(),
            &Default::default(),
            cleanup.as_ref(),
        )
        .await?;
        let service = resolved.service.as_ref().unwrap();
        assert!(service.metadata.is_sealed());
        assert_eq!(
            service.metadata.linked_components,
            HashSet::from([Arc::clone(&callee_id)])
        );
        service.pre_instantiate_raw()?;
        assert!(original_linker.upgrade().is_none());
        {
            let components = resolved.components();
            let components = components.read().await;
            assert!(
                matches!(
                    components[&callee_id].metadata.link,
                    LinkState::Shared { .. }
                ),
                "nothing wrote to the callee's linker"
            );
            assert!(
                matches!(components[&importer_id].metadata.link, LinkState::Linked(_)),
                "the importer's link is kept"
            );
            assert!(
                importer_linker.upgrade().is_none(),
                "and its linker copy freed"
            );
            assert!(
                matches!(components[&broken_id].metadata.link, LinkState::Unlinked(_)),
                "a component that does not link still resolves, keeping its error"
            );
        }
        if let Some(cleanup) = cleanup {
            let state = cleanup.state.lock().unwrap();
            let saved = state.resolved.as_ref().unwrap().service.as_ref().unwrap();
            assert!(
                Arc::ptr_eq(service, saved),
                "cleanup must keep the sealed service"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn sealing_service_linkers_preserves_repeated_starts() -> anyhow::Result<()> {
    #[cfg(feature = "wasi-tls")]
    crate::init_crypto();

    for p3 in [false, true] {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model_async(true);
        let engine = wasmtime::Engine::new(&config)?;
        let (run, lift, version) = if p3 {
            (
                r#"(core func $return (canon task.return (result (result))))
                       (core module $m
                         (import "host" "probe" (func $probe))
                         (import "host" "return" (func $return (param i32)))
                         (func (export "run") (result i32)
                           call $probe i32.const 0 call $return i32.const 0)
                         (func (export "callback") (param i32 i32 i32) (result i32)
                           unreachable))
                       (core instance $m (instantiate $m (with "host" (instance
                         (export "probe" (func $probe)) (export "return" (func $return))))))"#,
                r#"(func $run async (result (result)) (canon lift (core func $m "run")
                         async (callback (core func $m "callback"))))"#,
                "0.3.0",
            )
        } else {
            (
                r#"(core module $m
                         (import "host" "probe" (func $probe))
                         (func (export "run") (result i32) call $probe i32.const 0))
                       (core instance $m (instantiate $m (with "host" (instance
                         (export "probe" (func $probe))))))"#,
                r#"(func $run (result (result)) (canon lift (core func $m "run")))"#,
                "0.2.0",
            )
        };
        let wasm = wat::parse_str(format!(
            r#"(component
                     (import "test:service/probe" (instance $host (export "probe" (func))))
                     (alias export $host "probe" (func $probe))
                     (core func $probe (canon lower (func $probe)))
                     {run}
                     {lift}
                     (instance $cli (export "run" (func $run)))
                     (export "wasi:cli/run@{version}" (instance $cli)))"#
        ))?;
        let mut service = WorkloadService::new(
            "sealed-service",
            "sealed-service",
            "test",
            Component::new(&engine, wasm)?,
            Linker::new(&engine),
            vec![],
            LocalResources::default(),
            1,
            Arc::default(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        service
            .metadata
            .linker()?
            .instance("test:service/probe")?
            .func_wrap("probe", {
                let calls = Arc::clone(&calls);
                move |_, (): ()| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })?;
        // Populate the cache before resolution too; sealing must preserve it.
        service.pre_instantiate_raw()?;
        let copy = Arc::downgrade(open_linker_of(&service.metadata));
        let cleanup = WorkloadResources::default();
        let mut resolved = UnresolvedWorkload::new(
            "sealed-service",
            "sealed-service",
            "test",
            Some(service),
            [],
            vec![],
        )
        .resolve_for_start(
            None,
            &Default::default(),
            &crate::host::HostRef::detached(),
            &Default::default(),
            Some(&cleanup),
        )
        .await?;
        assert!(
            copy.upgrade().is_none(),
            "service and cleanup must release the linker"
        );
        let service = resolved.service_mut().unwrap();
        assert!(service.metadata.linker().is_err());
        for start in 1..=2 {
            let ctx = crate::engine::ctx::Ctx::builder("sealed-service", service.id()).build();
            let mut store = wasmtime::Store::new(&engine, SharedCtx::new(ctx));
            // Both the raw view (used by trigger services) and typed command
            // views must keep working after the linker is gone.
            service
                .pre_instantiate_raw()?
                .instantiate_async(&mut store)
                .await?;
            if p3 {
                let command = service
                    .pre_instantiate_p3()?
                    .instantiate_async(&mut store)
                    .await?;
                let result = store
                    .run_concurrent(async |accessor| {
                        command.wasi_cli_run().call_run(accessor).await
                    })
                    .await??;
                assert_eq!(result, Ok(()));
            } else {
                let command = service
                    .pre_instantiate()?
                    .instantiate_async(&mut store)
                    .await?;
                assert_eq!(command.wasi_cli_run().call_run(&mut store).await?, Ok(()));
            }
            assert_eq!(calls.load(Ordering::SeqCst), start);
        }
    }
    Ok(())
}

fn marker_exporter(id: &str) -> WorkloadComponent {
    component_from_wat(
        id,
        &format!(r#"(component (instance $marker) (export "{MARKER}" (instance $marker)))"#),
    )
}

fn marker_workload(components: Vec<WorkloadComponent>) -> UnresolvedWorkload {
    UnresolvedWorkload::new(
        "local-first".to_string(),
        "local-first".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![WitInterface::from(MARKER)],
    )
}

#[test]
fn engine_hostcall_fuel_reaches_component_and_service_metadata() {
    let engine = crate::engine::Engine::builder()
        .with_pooling_allocator(false)
        .without_max_process_memory()
        .with_hostcall_fuel(std::num::NonZeroUsize::new(4096).unwrap())
        .build()
        .unwrap();
    let bytes = bytes::Bytes::from(wat::parse_str("(component)").unwrap());
    // A service is refused unless it exports an interface with `run`.
    let service = bytes::Bytes::from(
        wat::parse_str(
            r#"(component
                  (core module $m (func (export "run")))
                  (core instance $i (instantiate $m))
                  (func $run (canon lift (core func $i "run")))
                  (instance $cli (export "run" (func $run)))
                  (export "wasi:cli/run@0.2.0" (instance $cli)))"#,
        )
        .unwrap(),
    );
    let unresolved = engine
        .initialize_workload(
            "hostcall-fuel",
            crate::types::Workload {
                namespace: "ns".into(),
                name: "hostcall-fuel".into(),
                annotations: Default::default(),
                service: Some(crate::types::Service {
                    bytes: service,
                    digest: None,
                    local_resources: Default::default(),
                    max_restarts: 0,
                }),
                components: vec![crate::types::Component {
                    name: "component".into(),
                    bytes,
                    ..Default::default()
                }],
                host_interfaces: vec![],
                volumes: vec![],
            },
        )
        .unwrap();
    assert_eq!(unresolved.components.len(), 1);
    for component in unresolved.components.values() {
        assert_eq!(
            component.metadata.store_settings.hostcall_fuel,
            Some(std::num::NonZeroUsize::new(4096).unwrap())
        );
    }
    assert_eq!(
        unresolved
            .service
            .as_ref()
            .unwrap()
            .metadata
            .store_settings
            .hostcall_fuel,
        Some(std::num::NonZeroUsize::new(4096).unwrap())
    );
}

fn marker_plugin() -> HashMap<&'static str, Arc<dyn HostPlugin>> {
    let plugin = Arc::new(MockPlugin::new(
        "marker-plugin",
        vec![],
        vec![WitInterface::from(MARKER)],
    ));
    HashMap::from([(plugin.id(), plugin as Arc<dyn HostPlugin>)])
}

/// Every component id `bind_plugins` reported as bound. Ids are fresh
/// UUIDs, so a test compares against the ones it captured, not the names it
/// gave.
fn bound_component_ids(bound: &[(Arc<dyn HostPlugin + 'static>, Vec<String>)]) -> HashSet<String> {
    bound
        .iter()
        .flat_map(|(_, ids)| ids.iter().cloned())
        .collect()
}

/// A workload serves itself first: an interface one component imports and
/// another exports is linked component-to-component, so the importer is not
/// bound to the plugin that also offers it. Whoever matches installs the
/// shim, and plugins match before components are linked — so without this
/// the plugin would take the import and the sibling's export would go
/// unused, silently.
#[tokio::test]
async fn a_sibling_export_wins_over_a_plugin() {
    let importer = marker_importer("importer");
    let exporter = marker_exporter("exporter");
    let importer_id = importer.id().to_string();
    let exporter_id = exporter.id().to_string();

    let mut workload = marker_workload(vec![importer, exporter]);
    let bound = workload
        .bind_plugins(&marker_plugin(), &crate::plugin::PluginBindings::new())
        .await
        .unwrap();
    let bound_ids = bound_component_ids(&bound);

    assert!(
        !bound_ids.contains(&importer_id),
        "the importer should link to its sibling, not bind the plugin"
    );
    assert!(
        bound_ids.contains(&exporter_id),
        "the exporter still binds, because an export is how a host reaches into a workload"
    );
}

/// The control: with no sibling exporting it, the same import binds the
/// plugin as before. Without this the test above would pass for a workload
/// that simply never matched anything.
#[tokio::test]
async fn a_lone_importer_still_binds_the_plugin() {
    let importer = marker_importer("importer");
    let importer_id = importer.id().to_string();

    let mut workload = marker_workload(vec![importer]);
    let bound = workload
        .bind_plugins(&marker_plugin(), &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert!(
        bound_component_ids(&bound).contains(&importer_id),
        "with nothing in the workload exporting it, the plugin serves the import"
    );
}

/// A plugin that overrides a WASI interface (a filesystem view, say) binds
/// to the components that import that interface, as soon as the
/// workload's `hostInterfaces` names what the plugin serves. A component
/// not using the interface, or a workload without the entry, is untouched.
#[tokio::test]
async fn a_wasi_override_binds_the_components_that_use_the_interface() {
    let fs_user = component_from_wat(
        "fs-user",
        r#"(component (import "wasi:filesystem/types@0.2.12" (instance)))"#,
    );
    let plain = component_from_wat("plain", "(component)");
    let fs_user_id = fs_user.id().to_string();
    let plain_id = plain.id().to_string();
    let plugin = Arc::new(
        MockPlugin::new("view-plugin", vec![WitInterface::from(MARKER)], vec![])
            .overriding("wasi:filesystem/preopens,types@0.2.0"),
    );
    let plugins: HashMap<&'static str, Arc<dyn HostPlugin>> =
        HashMap::from([(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>)]);

    let mut workload = marker_workload(vec![fs_user, plain]);
    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();
    let bound_ids = bound_component_ids(&bound);
    assert!(
        bound_ids.contains(&fs_user_id),
        "imports the overridden interface"
    );
    assert!(
        !bound_ids.contains(&plain_id),
        "imports nothing the plugin changes"
    );
    assert_eq!(plugin.get_call_count("on_workload_item_bind"), 1);

    let fs_user = component_from_wat(
        "fs-user",
        r#"(component (import "wasi:filesystem/types@0.2.12" (instance)))"#,
    );
    let mut workload = UnresolvedWorkload::new(
        "local-first".to_string(),
        "local-first".to_string(),
        "test-namespace".to_string(),
        None,
        vec![fs_user],
        vec![],
    );
    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();
    assert!(bound.is_empty(), "no entry, no binding");
}

#[tokio::test]
async fn a_sealed_linker_failure_rolls_back_the_override_plugin() -> anyhow::Result<()> {
    let mut component = component_from_wat(
        "fs-user",
        r#"(component (import "wasi:filesystem/types@0.2.12" (instance)))"#,
    );
    component.metadata.seal_linker();
    let plugin = Arc::new(
        MockPlugin::new("view-plugin", vec![WitInterface::from(MARKER)], vec![])
            .overriding("wasi:filesystem/preopens,types@0.2.0"),
    );
    let plugins: HashMap<&'static str, Arc<dyn HostPlugin>> =
        HashMap::from([(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>)]);
    let mut workload = marker_workload(vec![component]);

    let error = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .err()
        .context("binding a sealed linker unexpectedly succeeded")?;

    assert!(error.to_string().contains("sealed"));
    let methods = plugin
        .get_call_records()
        .into_iter()
        .map(|record| record.method)
        .collect::<Vec<_>>();
    assert_eq!(methods, ["on_workload_bind", "on_workload_unbind"]);
    Ok(())
}

/// Two plugins redefining one WASI interface cannot both take effect, so
/// the host refuses them before anything binds, naming both.
#[tokio::test]
async fn two_plugins_overriding_one_wasi_interface_are_refused() {
    let a = Arc::new(
        MockPlugin::new("view-a", vec![WitInterface::from(MARKER)], vec![])
            .overriding("wasi:filesystem/types@0.2.0"),
    );
    let b = Arc::new(
        MockPlugin::new(
            "view-b",
            vec![WitInterface::from("other:pkg/iface")],
            vec![],
        )
        .overriding("wasi:filesystem/preopens,types@0.2.0"),
    );
    let plugins: HashMap<&'static str, Arc<dyn HostPlugin>> = HashMap::from([
        (a.id(), a.clone() as Arc<dyn HostPlugin>),
        (b.id(), b.clone() as Arc<dyn HostPlugin>),
    ]);
    let error = crate::plugin::check_wasi_overrides(&plugins)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("view-a") && error.contains("view-b"),
        "{error}"
    );
    assert!(error.contains("wasi:filesystem"), "{error}");

    let mut workload = marker_workload(vec![marker_importer("importer")]);
    let refused = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await;
    assert!(refused.is_err(), "binding refuses the pair too");
    assert_eq!(a.get_call_count("on_workload_item_bind"), 0);
    assert_eq!(b.get_call_count("on_workload_item_bind"), 0);

    // Different versions of one package are different interfaces.
    let c = Arc::new(
        MockPlugin::new(
            "view-c",
            vec![WitInterface::from("other:pkg/iface")],
            vec![],
        )
        .overriding("wasi:filesystem/types@0.3.0"),
    );
    let plugins: HashMap<&'static str, Arc<dyn HostPlugin>> = HashMap::from([
        (a.id(), a.clone() as Arc<dyn HostPlugin>),
        (c.id(), c as Arc<dyn HostPlugin>),
    ]);
    crate::plugin::check_wasi_overrides(&plugins).unwrap();
}

/// Two components exporting the same interface is the ambiguity
/// `build_export_map` declines to resolve, so the import is left to the
/// plugin rather than failing a workload that deploys today.
#[tokio::test]
async fn two_sibling_exporters_leave_the_import_to_the_plugin() {
    let importer = marker_importer("importer");
    let importer_id = importer.id().to_string();

    let mut workload = marker_workload(vec![
        importer,
        marker_exporter("exporter-a"),
        marker_exporter("exporter-b"),
    ]);
    let bound = workload
        .bind_plugins(&marker_plugin(), &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert!(
        bound_component_ids(&bound).contains(&importer_id),
        "an ambiguous sibling export is not a provider, so the plugin keeps the import"
    );
}

/// A workload that both publishes and handles names both interfaces on its
/// one unnamed `wasmcloud:messaging` entry, which is the only shape a
/// manifest can give a package. Each component uses half of it and both
/// bind the messaging plugin, so the publisher gets `consumer` in its
/// linker and the handler's export is subscribed.
///
/// Uses the real plugin, whose world is the split this turns on: `consumer`
/// and `types` served, `handler` called back on the workload.
#[tokio::test]
async fn a_publisher_and_a_handler_share_one_messaging_entry() {
    let publisher = component_from_wat(
        "publisher",
        r#"(component (import "wasmcloud:messaging/consumer@0.2.0" (instance)))"#,
    );
    let handler = component_from_wat(
        "handler",
        r#"(component
                 (instance $handler)
                 (export "wasmcloud:messaging/handler@0.2.0" (instance $handler)))"#,
    );
    let publisher_id = publisher.id().to_string();
    let handler_id = handler.id().to_string();

    let plugin = Arc::new(crate::plugin::wasmcloud_messaging::in_memory::InMemoryMessaging::new());
    let plugins = HashMap::from([(plugin.id(), plugin as Arc<dyn HostPlugin>)]);

    let mut workload = UnresolvedWorkload::new(
        "messaging",
        "messaging",
        "test-namespace",
        None,
        vec![publisher, handler],
        vec![WitInterface::from(
            "wasmcloud:messaging/consumer,handler,types@0.2.0",
        )],
    );
    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .expect("both halves of the entry are served");
    let bound_ids = bound_component_ids(&bound);

    assert!(
        bound_ids.contains(&publisher_id),
        "the publisher needs `consumer` in its linker"
    );
    assert!(
        bound_ids.contains(&handler_id),
        "nothing subscribes the handler's export unless the plugin binds it"
    );
}

/// A component importing a subset of an entry — the ordinary shape of a
/// multi-component workload, since one entry covers them all — binds the
/// plugin serving that entry.
#[tokio::test]
async fn a_component_using_part_of_an_entry_still_binds() {
    const PROBE: &str = "test:probe/alpha,beta@0.1.0";

    let both = component_from_wat(
        "both",
        r#"(component
                 (import "test:probe/alpha@0.1.0" (instance))
                 (import "test:probe/beta@0.1.0" (instance)))"#,
    );
    let alpha_only = component_from_wat(
        "alpha-only",
        r#"(component (import "test:probe/alpha@0.1.0" (instance)))"#,
    );
    let both_id = both.id().to_string();
    let alpha_only_id = alpha_only.id().to_string();

    let plugin = Arc::new(MockPlugin::new(
        "probe-plugin",
        vec![WitInterface::from(PROBE)],
        vec![],
    ));
    let plugins = HashMap::from([(plugin.id(), plugin as Arc<dyn HostPlugin>)]);

    let mut workload = UnresolvedWorkload::new(
        "subset",
        "subset",
        "test-namespace",
        None,
        vec![both, alpha_only],
        vec![WitInterface::from(PROBE)],
    );
    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();
    let bound_ids = bound_component_ids(&bound);

    assert!(bound_ids.contains(&both_id));
    assert!(
        bound_ids.contains(&alpha_only_id),
        "a component importing one interface of the entry still needs the plugin serving it"
    );
}

/// A workload still serves itself first when the entry names more than the
/// importer uses: the interfaces an item imports are the ones a sibling can
/// answer for it, so the link survives and the plugin does not install a
/// shim over it. The sibling that *exports* the interface still binds,
/// because an export is how the host reaches into a workload.
#[tokio::test]
async fn a_sibling_export_still_wins_for_part_of_an_entry() {
    const PROBE: &str = "test:probe/alpha,beta@0.1.0";

    let importer = component_from_wat(
        "importer",
        r#"(component (import "test:probe/alpha@0.1.0" (instance)))"#,
    );
    let exporter = component_from_wat(
        "exporter",
        r#"(component
                 (instance $alpha)
                 (export "test:probe/alpha@0.1.0" (instance $alpha)))"#,
    );
    let importer_id = importer.id().to_string();
    let exporter_id = exporter.id().to_string();

    let plugin = Arc::new(MockPlugin::new(
        "probe-plugin",
        vec![WitInterface::from(PROBE)],
        vec![],
    ));
    let plugins = HashMap::from([(plugin.id(), plugin as Arc<dyn HostPlugin>)]);

    let mut workload = UnresolvedWorkload::new(
        "subset",
        "subset",
        "test-namespace",
        None,
        vec![importer, exporter],
        vec![WitInterface::from(PROBE)],
    );
    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();
    let bound_ids = bound_component_ids(&bound);

    assert!(
        !bound_ids.contains(&importer_id),
        "the importer links to its sibling, so the plugin must not take its import"
    );
    assert!(bound_ids.contains(&exporter_id));
}

/// An item that exports one of the entry's interfaces binds the plugin even
/// when a sibling answers its imports: an export is how the host reaches
/// into a workload, which is a separate question from where imports point.
#[tokio::test]
async fn an_exporter_binds_even_when_a_sibling_answers_its_imports() {
    const PROBE: &str = "test:probe/alpha,beta@0.1.0";

    let both_ways = component_from_wat(
        "both-ways",
        r#"(component
                 (import "test:probe/alpha@0.1.0" (instance))
                 (instance $beta)
                 (export "test:probe/beta@0.1.0" (instance $beta)))"#,
    );
    let exporter = component_from_wat(
        "exporter",
        r#"(component
                 (instance $alpha)
                 (export "test:probe/alpha@0.1.0" (instance $alpha)))"#,
    );
    let both_ways_id = both_ways.id().to_string();

    let plugin = Arc::new(MockPlugin::new(
        "probe-plugin",
        vec![WitInterface::from(PROBE)],
        vec![],
    ));
    let plugins = HashMap::from([(plugin.id(), plugin as Arc<dyn HostPlugin>)]);

    let mut workload = UnresolvedWorkload::new(
        "subset",
        "subset",
        "test-namespace",
        None,
        vec![both_ways, exporter],
        vec![WitInterface::from(PROBE)],
    );
    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert!(
        bound_component_ids(&bound).contains(&both_ways_id),
        "nothing else can reach the export this component serves"
    );
}

/// The provider side keeps asking for the whole entry: a plugin serving
/// part of one must not claim it, or the rest is left bound by nobody —
/// here the interface goes unmatched and the workload is refused, rather
/// than deploying with an import nothing serves.
#[tokio::test]
async fn a_plugin_serving_part_of_an_entry_does_not_claim_it() {
    let importer = component_from_wat(
        "importer",
        r#"(component
                 (import "test:probe/alpha@0.1.0" (instance))
                 (import "test:probe/beta@0.1.0" (instance)))"#,
    );

    let plugin = Arc::new(MockPlugin::new(
        "alpha-only-plugin",
        vec![WitInterface::from("test:probe/alpha@0.1.0")],
        vec![],
    ));
    let plugins = HashMap::from([(plugin.id(), plugin as Arc<dyn HostPlugin>)]);

    let mut workload = UnresolvedWorkload::new(
        "subset",
        "subset",
        "test-namespace",
        None,
        vec![importer],
        vec![WitInterface::from("test:probe/alpha,beta@0.1.0")],
    );
    let err = match workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
    {
        Ok(_) => panic!("a partially served entry has no provider"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("not available on this host"),
        "expected an unmatched-interface refusal, got: {err}"
    );
}

/// Which half of the bind a [`RollbackPlugin`] refuses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FailAt {
    WorkloadBind,
    ItemBind,
}

/// A plugin that counts its own `on_workload_unbind` and can refuse either half of the bind. Whatever
/// a real plugin opened on the way in is released through that callback, so the count is what says whether a failure rolled itself back.
struct RollbackPlugin {
    fail_at: Option<FailAt>,
    unbinds: Arc<AtomicUsize>,
}

impl RollbackPlugin {
    fn new(fail_at: Option<FailAt>) -> Self {
        Self {
            fail_at,
            unbinds: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn unbind_count(&self) -> usize {
        self.unbinds.load(Ordering::SeqCst)
    }

    fn registered(self: &Arc<Self>) -> HashMap<&'static str, Arc<dyn HostPlugin>> {
        HashMap::from([(self.id(), self.clone() as Arc<dyn HostPlugin>)])
    }
}

#[async_trait]
impl HostPlugin for RollbackPlugin {
    fn id(&self) -> &'static str {
        "rollback-plugin"
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::new(),
            exports: HashSet::from([WitInterface::from(MARKER)]),
        }
    }

    async fn on_workload_bind(
        &self,
        _workload: &UnresolvedWorkload,
        _interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        match self.fail_at {
            Some(FailAt::WorkloadBind) => bail!("refusing the workload bind"),
            _ => Ok(()),
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        _item: &mut WorkloadItem<'a>,
        _interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        match self.fail_at {
            Some(FailAt::ItemBind) => bail!("refusing the item bind"),
            _ => Ok(()),
        }
    }

    async fn on_workload_unbind(
        &self,
        _workload_id: &str,
        _interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        self.unbinds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A bind can get partway through before it errors, and the plugin that
/// failed is in no rollback list — it is added only once every one of its
/// items is bound. So the failure path unbinds it itself, or a connection
/// opened on the way in outlives the deploy that never happened.
#[tokio::test]
async fn a_failed_workload_bind_rolls_itself_back() {
    let plugin = Arc::new(RollbackPlugin::new(Some(FailAt::WorkloadBind)));
    let mut workload = marker_workload(vec![marker_importer("importer")]);

    assert!(
        workload
            .bind_plugins(&plugin.registered(), &crate::plugin::PluginBindings::new())
            .await
            .is_err(),
        "the plugin refused its bind"
    );

    assert_eq!(
        plugin.unbind_count(),
        1,
        "the plugin that failed its bind must be unbound exactly once"
    );
}

/// The item bind is worse still: `on_workload_bind` already succeeded, so
/// the plugin is certainly holding state by the time an item is refused.
#[tokio::test]
async fn a_failed_item_bind_rolls_itself_back() {
    let plugin = Arc::new(RollbackPlugin::new(Some(FailAt::ItemBind)));
    let mut workload = marker_workload(vec![marker_importer("importer")]);

    assert!(
        workload
            .bind_plugins(&plugin.registered(), &crate::plugin::PluginBindings::new())
            .await
            .is_err(),
        "the plugin refused its item bind"
    );

    assert_eq!(
        plugin.unbind_count(),
        1,
        "the plugin that failed its item bind must be unbound exactly once"
    );
}

/// Every step of `resolve` after the plugins are bound owes them a
/// rollback, volume-mount canonicalization included: a hostPath that does
/// not exist is a deploy-time failure like any other, and the plugins it
/// leaves behind keep tracking a workload that never runs.
#[tokio::test]
async fn a_bad_volume_mount_unbinds_the_plugins() {
    let mut importer = marker_importer("importer");
    importer.metadata.volume_mounts = vec![(
        PathBuf::from("/definitely/not/a/directory/on/this/host"),
        VolumeMount {
            name: "data".to_string(),
            mount_path: "/data".to_string(),
            read_only: true,
        },
    )];

    let plugin = Arc::new(RollbackPlugin::new(None));
    let workload = marker_workload(vec![importer]);
    // Bound, not a temporary: the resolved workload keeps only a `Weak`.
    let http_handler: Arc<dyn crate::host::http::HostHandler> =
        Arc::new(crate::host::http::NullServer::default());

    workload
        .resolve(
            Some(&plugin.registered()),
            &crate::plugin::PluginBindings::new(),
            &crate::host::HostRef::from_handler(&http_handler),
            &crate::observability::Meters::new(crate::observability::MeterKind::Off),
        )
        .await
        .expect_err("the volume mount cannot be canonicalized");

    assert_eq!(
        plugin.unbind_count(),
        1,
        "a plugin bound before the failing step must be unbound"
    );
}

/// A host-dispatched export (like the `wasi:http` entrypoint) must go to
/// the first exporting component in manifest orderå.
#[test]
fn select_exporter_follows_manifest_order() {
    // Enough components that an unordered scan almost never matches.
    let components: Vec<WorkloadComponent> = (0..8)
        .map(|i| create_test_component(&format!("component{i}")))
        .collect();
    let expected: Vec<Arc<str>> = components.iter().map(|c| Arc::from(c.id())).collect();

    let workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![WitInterface::from("wasi:http/incoming-handler")],
    );

    assert_eq!(
        workload.component_order, expected,
        "component_order must preserve manifest order"
    );
    let order: Vec<Arc<str>> = workload
        .components_in_manifest_order()
        .map(|c| Arc::from(c.id()))
        .collect();
    assert_eq!(order, expected);

    // All eight components export wasi:http; the first must win.
    let selected = workload
        .select_exporter("wasi:http", |c| c.exports_wasi_http())
        .map(|c| Arc::<str>::from(c.id()));
    assert_eq!(selected.as_ref(), expected.first());

    // A predicate nothing matches selects nobody.
    assert!(workload.select_exporter("wasi:none", |_| false).is_none());
}

fn create_test_messaging_component(id: &str) -> WorkloadComponent {
    let engine = wasmtime::Engine::default();
    let linker = Linker::new(&engine);

    let wasm = messaging_handler_wasm();
    let component = Component::new(&engine, &wasm).unwrap();

    let local_resources = LocalResources::default();

    WorkloadComponent::new(
        format!("workload-{id}"),
        format!("test-workload-{id}"),
        "test-namespace".to_string(),
        "test-component".to_string(),
        component,
        linker,
        Vec::new(),
        local_resources,
        Arc::default(),
        InstancePolicy::Ephemeral,
    )
}

fn create_test_service_component(id: &str) -> WorkloadService {
    let engine = wasmtime::Engine::default();
    let linker = Linker::new(&engine);

    let wasm = service_wasm();
    let component = Component::new(&engine, &wasm).unwrap();

    let local_resources = LocalResources::default();

    WorkloadService::new(
        format!("workload-{id}"),
        format!("test-workload-{id}"),
        "test-namespace".to_string(),
        component,
        linker,
        Vec::new(),
        local_resources,
        3,
        Arc::default(),
    )
}

/// Every built-in plugin must actually bind the interface it advertises in its
/// `world()`. The bind hooks pick which interfaces to serve by matching
/// namespace / package / interface-name strings against the requested set, so a
/// wrong literal (a package typo, or a config key passed where an interface name
/// is expected) makes the plugin silently no-op instead of registering its host
/// functions. This guards that whole class of regression: feed each plugin an
/// interface it is designed to serve, then assert the first bind registers host
/// functions. Because a second bind of the same interface on the same linker
/// conflicts, a plugin that no-op'd (never touched the linker) is caught by the
/// second bind wrongly succeeding.
#[cfg(all(
    feature = "wasmcloud-postgres",
    feature = "wasi-blobstore",
    feature = "wasi-keyvalue"
))]
#[tokio::test]
async fn builtin_plugins_bind_their_declared_interfaces() {
    use crate::plugin::wasi_blobstore::{FilesystemBlobstore, InMemoryBlobstore};
    use crate::plugin::wasi_keyvalue::InMemoryKeyValue;
    use crate::plugin::wasmcloud_postgres::WasmcloudPostgres;
    // wasmcloud:postgres carries the target database as interface config, not as
    // an interface name; the plugin reads it after matching on package.
    let mut postgres_iface =
        WitInterface::from("wasmcloud:postgres/types,query,prepared@0.1.1-draft");
    postgres_iface
        .config
        .insert("database".to_string(), "testdb".to_string());
    let blobstore_iface =
        WitInterface::from("wasi:blobstore/blobstore,container,types@0.2.0-draft");
    let keyvalue_iface = WitInterface::from("wasi:keyvalue/store,atomics,batch@0.2.0-draft");
    let cases: Vec<(Arc<dyn HostPlugin>, WitInterface)> = vec![
        (
            Arc::new(
                WasmcloudPostgres::new(
                    &url::Url::parse("postgres://user:pass@localhost:5432/db").unwrap(),
                )
                .unwrap(),
            ) as Arc<dyn HostPlugin>,
            postgres_iface,
        ),
        (
            Arc::new(FilesystemBlobstore::new(std::env::temp_dir())) as Arc<dyn HostPlugin>,
            blobstore_iface.clone(),
        ),
        (
            Arc::new(InMemoryBlobstore::new(None)) as Arc<dyn HostPlugin>,
            blobstore_iface,
        ),
        (
            Arc::new(InMemoryKeyValue::new()) as Arc<dyn HostPlugin>,
            keyvalue_iface,
        ),
    ];
    // A minimal empty component is enough: the bind hooks only mutate the linker.
    let build_component = || {
        let engine = wasmtime::Engine::default();
        let linker = Linker::new(&engine);
        // Minimal empty component: magic "\0asm" + component version + layer.
        let component =
            Component::new(&engine, [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00]).unwrap();
        WorkloadComponent::new(
            "workload-bind".to_string(),
            "test-workload-bind".to_string(),
            "test-namespace".to_string(),
            "test-component".to_string(),
            component,
            linker,
            Vec::new(),
            LocalResources::default(),
            Arc::default(),
            InstancePolicy::Ephemeral,
        )
    };
    for (plugin, iface) in cases {
        let id = plugin.id();
        let set: HashSet<WitInterface> = std::iter::once(iface).collect();
        let mut component = build_component();
        let mut item = WorkloadItem::Component(&mut component);
        plugin
            .on_workload_item_bind(&mut item, WitInterfaces::new(&set))
            .await
            .unwrap_or_else(|e| panic!("plugin `{id}` failed to bind its declared interface: {e}"));
        // The first bind registered the plugin's host functions, so binding the
        // same interface again on the same linker must conflict. If the plugin
        // silently no-op'd (e.g. a matcher typo), it never touched the linker and
        // this second bind wrongly succeeds.
        let rebind = plugin
            .on_workload_item_bind(&mut item, WitInterfaces::new(&set))
            .await;
        assert!(
            rebind.is_err(),
            "plugin `{id}` silently no-op'd on its declared interface instead of \
                     registering host functions"
        );
    }
}

/// Build a one-component workload asking for `interfaces`.
fn workload_requesting(interfaces: Vec<WitInterface>) -> UnresolvedWorkload {
    UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        interfaces,
    )
}

fn kv_iface(name: Option<&str>, config: &[(&str, &str)]) -> WitInterface {
    let mut iface = WitInterface::from("wasi:keyvalue/store");
    iface.name = name.map(str::to_string);
    iface.config = config
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    iface
}

/// The operator's declaration reaches the plugin as the interface's config,
/// on both `on_workload_bind` and `on_workload_item_bind` — a plugin never
/// sees the workload's raw map, so it cannot accidentally read around the
/// policy.
#[tokio::test]
async fn test_operator_bindings_reach_the_plugin_as_interface_config() {
    let plugin = Arc::new(
        MockPlugin::new("kv", vec![], vec![kv_iface(None, &[])])
            .with_named_instance_support()
            .owning(&["url"]),
    );
    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let bindings = crate::plugin::PluginBindings::new().with_plugin(
        crate::plugin::PluginBindingSet::new("kv")
            .with_base([("url".to_string(), "redis://host:6379".to_string())].into())
            .with_binding(
                "cache",
                [("bucket".to_string(), "cache".to_string())].into(),
            ),
    );

    let mut workload = workload_requesting(vec![kv_iface(Some("cache"), &[("timeout-ms", "500")])]);
    workload.bind_plugins(&plugins, &bindings).await.unwrap();

    let seen = plugin.seen_config(Some("cache"));
    assert_eq!(seen["url"], "redis://host:6379");
    assert_eq!(seen["bucket"], "cache");
    assert_eq!(seen["timeout-ms"], "500");
    assert_eq!(
        plugin.get_call_count("on_workload_item_bind"),
        1,
        "the item bind must still run against the resolved interface"
    );
}

/// Answers every key with `secret-for:<key>`.
struct EchoSecrets;

#[async_trait]
impl crate::plugin::HostSecretSource for EchoSecrets {
    async fn resolve(
        &self,
        request: crate::plugin::SecretRequest,
    ) -> Result<
        BTreeMap<crate::plugin::SecretKey, crate::plugin::SecretValue>,
        crate::plugin::SecretSourceError,
    > {
        Ok(request
            .keys
            .into_iter()
            .map(|k| {
                let value = crate::plugin::SecretValue::new(format!("secret-for:{k}"));
                (k, value)
            })
            .collect())
    }
}

/// A kv plugin owning `url`, a secrets plugin, and a `cache` binding whose
/// `url` comes from the secrets plugin.
fn kv_with_plugin_secret() -> (
    Arc<MockPlugin>,
    HashMap<&'static str, Arc<dyn HostPlugin>>,
    crate::plugin::PluginBindings,
) {
    let kv = Arc::new(
        MockPlugin::new("kv", vec![], vec![kv_iface(None, &[])])
            .with_named_instance_support()
            .owning(&["url"]),
    );
    let secrets =
        Arc::new(MockPlugin::new("vault", vec![], vec![]).serving_secrets(Arc::new(EchoSecrets)));
    let plugins = HashMap::from([
        (kv.id(), kv.clone() as Arc<dyn HostPlugin>),
        (secrets.id(), secrets as Arc<dyn HostPlugin>),
    ]);
    let bindings = crate::plugin::PluginBindings::new().with_plugin(
        crate::plugin::PluginBindingSet::new("kv")
            .with_binding("cache", Default::default())
            .with_binding_secrets(
                "cache",
                HashMap::from([(
                    "url".to_string(),
                    crate::plugin::PluginSecretRef {
                        source: "cache-db".into(),
                        plugin: "vault".into(),
                        key: "kv-url".into(),
                        grant: crate::plugin::WorkloadGrant::All,
                    },
                )]),
            ),
    );
    (kv, plugins, bindings)
}

/// A plugin secret reaches the consuming plugin as resolved config.
#[tokio::test]
async fn test_a_plugin_secret_reaches_the_plugin_as_config() {
    let (kv, plugins, bindings) = kv_with_plugin_secret();
    let mut workload = workload_requesting(vec![kv_iface(Some("cache"), &[])]);
    workload.bind_plugins(&plugins, &bindings).await.unwrap();
    assert_eq!(kv.seen_config(Some("cache"))["url"], "secret-for:kv-url");
}

/// A plugin secret is operator config by the time the policy runs, so
/// `deny` refuses a workload that sets the same key.
#[tokio::test]
async fn test_deny_refuses_a_workload_overriding_a_plugin_secret() {
    let (kv, plugins, bindings) = kv_with_plugin_secret();
    let mut workload =
        workload_requesting(vec![kv_iface(Some("cache"), &[("url", "redis://guest")])]);
    let err = match workload.bind_plugins(&plugins, &bindings).await {
        Ok(_) => panic!("binding should have been refused"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("`url`"), "got: {err}");
    assert_eq!(kv.get_call_count("on_workload_bind"), 0);
}

/// Under `deny`, a workload writing a host-owned key fails the deploy — and
/// every plugin bound before it is unbound, the same as any other bind
/// failure.
#[tokio::test]
async fn test_deny_refuses_a_workload_owned_key_and_rolls_back() {
    let logger = Arc::new(MockPlugin::new(
        "aaa-logger",
        vec![],
        vec![WitInterface::from("wasi:logging/logging")],
    ));
    let kv = Arc::new(
        MockPlugin::new("kv", vec![], vec![kv_iface(None, &[])])
            .with_named_instance_support()
            .owning(&["url"]),
    );
    let mut plugins = HashMap::new();
    plugins.insert(logger.id(), logger.clone() as Arc<dyn HostPlugin>);
    plugins.insert(kv.id(), kv.clone() as Arc<dyn HostPlugin>);

    let bindings = crate::plugin::PluginBindings::new().with_plugin(
        crate::plugin::PluginBindingSet::new("kv")
            .with_binding(
                "cache",
                [("bucket".to_string(), "cache".to_string())].into(),
            )
            .with_workload_config(crate::plugin::WorkloadConfigPolicy::Deny),
    );

    let mut workload = workload_requesting(vec![
        WitInterface::from("wasi:logging/logging"),
        kv_iface(Some("cache"), &[("url", "redis://guest:6379")]),
    ]);
    let err = match workload.bind_plugins(&plugins, &bindings).await {
        Ok(_) => panic!("binding should have been refused"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("`url`"), "got: {err}");

    // `aaa-logger` sorts first, so it bound before `kv` refused.
    assert_eq!(logger.get_call_count("on_workload_bind"), 1);
    let unbound = logger
        .get_call_records()
        .iter()
        .filter(|r| r.method == "on_workload_unbind")
        .count();
    assert_eq!(unbound, 1, "the already-bound plugin must be rolled back");
}

/// Under `deny`, a name the operator never declared is refused rather than
/// silently taking the base config.
#[tokio::test]
async fn test_deny_refuses_an_undeclared_binding_name() {
    let plugin = Arc::new(
        MockPlugin::new("kv", vec![], vec![kv_iface(None, &[])]).with_named_instance_support(),
    );
    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let bindings = crate::plugin::PluginBindings::new().with_plugin(
        crate::plugin::PluginBindingSet::new("kv")
            .with_binding("cache", Default::default())
            .with_workload_config(crate::plugin::WorkloadConfigPolicy::Deny),
    );

    let mut workload = workload_requesting(vec![kv_iface(Some("sessions"), &[])]);
    let err = match workload.bind_plugins(&plugins, &bindings).await {
        Ok(_) => panic!("binding should have been refused"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("`sessions`"), "got: {err}");
    assert_eq!(plugin.get_call_count("on_workload_bind"), 0);
}

/// A named binding whose `name` matches a registered plugin id routes
/// exclusively to that plugin — not to a different plugin that also
/// serves the same raw interface, and without either plugin needing
/// `supports_named_instances()`. This routing axis (e.g. a workload
/// picking a specific `wasmcloud:secrets` backend by plugin id) is
/// distinct from the `(implements ..)` multiplexer.
#[tokio::test]
async fn test_named_binding_routes_to_plugin_by_id() {
    let iface = WitInterface::from("wasi:blobstore/container");

    let default_plugin = Arc::new(MockPlugin::new(
        "blobstore-default",
        vec![],
        vec![iface.clone()],
    ));
    let custom_plugin = Arc::new(MockPlugin::new(
        "blobstore-custom",
        vec![],
        vec![iface.clone()],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(
        default_plugin.id(),
        default_plugin.clone() as Arc<dyn HostPlugin>,
    );
    plugins.insert(
        custom_plugin.id(),
        custom_plugin.clone() as Arc<dyn HostPlugin>,
    );

    let mut named_iface = iface.clone();
    named_iface.name = Some("blobstore-custom".to_string());

    let components = vec![create_test_component("component1")];
    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![named_iface],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(
        custom_plugin.get_call_count("on_workload_bind"),
        1,
        "the plugin registered under the requested name must be bound"
    );
    assert_eq!(
        default_plugin.get_call_count("on_workload_bind"),
        0,
        "a different plugin that also serves the raw interface must NOT be bound \
             when the binding names a specific plugin id"
    );
    assert_eq!(bound_plugins.len(), 1);
    assert_eq!(bound_plugins[0].0.id(), "blobstore-custom");
}

/// A named binding whose `name` does NOT match any registered plugin id
/// falls through to the existing world-matching path unchanged (e.g. an
/// `(implements ..)` multiplex label) — the id-routing check is
/// additive, not a replacement.
#[tokio::test]
async fn test_named_binding_falls_through_when_name_is_not_a_plugin_id() {
    let iface = WitInterface::from("wasi:blobstore/container");
    let plugin = Arc::new(
        MockPlugin::new("blobstore-plugin", vec![], vec![iface.clone()])
            .with_named_instance_support(),
    );

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let mut named_iface = iface.clone();
    named_iface.name = Some("team-a".to_string());

    let components = vec![create_test_component("component1")];
    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![named_iface],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(
        plugin.get_call_count("on_workload_bind"),
        1,
        "a name with no matching plugin id must still resolve via world-matching"
    );
    assert_eq!(bound_plugins.len(), 1);
}

/// Two plugins serve one namespace:package at different revisions, the way
/// the sync and async `wasmcloud:messaging` multiplexers do. An entry that
/// pins NO version world-matches both — a versionless entry is compatible
/// with any version — so the one that refuses it via `claims` must leave it
/// for the other rather than claiming it and binding nothing.
///
/// Both are `(implements ..)`-capable, so the named/unnamed deferral does
/// not separate them; `claims` is the only thing that does. The interface is
/// blobstore rather than messaging only because it has to be one the
/// `http_counter.wasm` fixture actually imports.
#[tokio::test]
async fn test_unclaimed_interface_falls_through_to_another_plugin() {
    let iface = WitInterface::from("wasi:blobstore/container");

    // Sorts FIRST, so it is offered every interface before the other plugin
    // — which is exactly the case that used to strand a versionless entry.
    let newer = Arc::new(
        MockPlugin::new("blobstore-a-newer", vec![iface.clone()], vec![])
            .with_named_instance_support()
            .claiming_from("0.3.0"),
    );
    let older = Arc::new(
        MockPlugin::new("blobstore-b-older", vec![iface.clone()], vec![])
            .with_named_instance_support(),
    );

    let mut plugins = HashMap::new();
    plugins.insert(newer.id(), newer.clone() as Arc<dyn HostPlugin>);
    plugins.insert(older.id(), older.clone() as Arc<dyn HostPlugin>);

    let mut versionless = iface.clone();
    versionless.name = Some("team-a".to_string());

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![versionless],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(
        newer.get_call_count("on_workload_bind"),
        0,
        "a plugin that refuses an interface must not claim it"
    );
    assert_eq!(
        older.get_call_count("on_workload_item_bind"),
        1,
        "the refused interface must reach the plugin that will serve it"
    );
    assert_eq!(bound_plugins.len(), 1);
    assert_eq!(bound_plugins[0].0.id(), "blobstore-b-older");
}

/// The other half of the pair: an entry that DOES pin a claimed version goes
/// to the claiming plugin, so refusing versionless entries has not simply
/// disabled it. `0.2.0-draft` is the version the fixture imports; the
/// refusing plugin here claims from below it.
#[tokio::test]
async fn test_claimed_interface_binds_the_claiming_plugin() {
    let iface = WitInterface::from("wasi:blobstore/container@0.2.0-draft");

    let newer = Arc::new(
        MockPlugin::new("blobstore-a-newer", vec![iface.clone()], vec![])
            .with_named_instance_support()
            .claiming_from("0.1.0"),
    );
    let older = Arc::new(
        MockPlugin::new("blobstore-b-older", vec![iface.clone()], vec![])
            .with_named_instance_support(),
    );

    let mut plugins = HashMap::new();
    plugins.insert(newer.id(), newer.clone() as Arc<dyn HostPlugin>);
    plugins.insert(older.id(), older.clone() as Arc<dyn HostPlugin>);

    let mut versioned = iface.clone();
    versioned.name = Some("team-a".to_string());

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![versioned],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(bound_plugins.len(), 1);
    assert_eq!(bound_plugins[0].0.id(), "blobstore-a-newer");
}

/// A plugin that serves labeled *and* plain imports off one backend keeps
/// its plain imports when a single-backend native serves the same
/// interface. A host component plugin is that shape: the operator deployed
/// it to answer `wasi:keyvalue`, and declaring a binding on it says which
/// labels it answers to — not that it has stopped answering the unlabeled
/// import it was already serving.
#[tokio::test]
async fn test_a_dual_mode_plugin_keeps_its_plain_imports() {
    let iface = WitInterface::from("wasi:blobstore/container");

    // Sorts first, so ordering alone cannot be what decides this.
    let dual = Arc::new(
        MockPlugin::new("blobstore-a-component", vec![iface.clone()], vec![])
            .with_named_instance_support()
            .serving_unnamed_too(),
    );
    let native = Arc::new(MockPlugin::new(
        "blobstore-b-native",
        vec![iface.clone()],
        vec![],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(dual.id(), dual.clone() as Arc<dyn HostPlugin>);
    plugins.insert(native.id(), native.clone() as Arc<dyn HostPlugin>);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![iface],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(bound_plugins.len(), 1);
    assert_eq!(
        bound_plugins[0].0.id(),
        "blobstore-a-component",
        "a plugin serving both must not hand its unlabeled import to a native"
    );
}

/// A multiplexer defers a plain import to any plugin that serves plain
/// imports itself — including one that also routes labels. Keying the
/// deferral target on named-instance support alone would have the
/// multiplexer keep the import the moment the other plugin grew a label.
#[tokio::test]
async fn test_a_multiplexer_defers_plain_imports_to_a_dual_mode_plugin() {
    let iface = WitInterface::from("wasi:blobstore/container");

    // Sorts first, so it is offered the import before the dual plugin.
    let mux = Arc::new(
        MockPlugin::new("aaa-multiplexed", vec![iface.clone()], vec![])
            .with_named_instance_support(),
    );
    let dual = Arc::new(
        MockPlugin::new("zzz-component", vec![iface.clone()], vec![])
            .with_named_instance_support()
            .serving_unnamed_too(),
    );

    let mut plugins = HashMap::new();
    plugins.insert(mux.id(), mux.clone() as Arc<dyn HostPlugin>);
    plugins.insert(dual.id(), dual.clone() as Arc<dyn HostPlugin>);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![iface],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(bound_plugins.len(), 1);
    assert_eq!(
        bound_plugins[0].0.id(),
        "zzz-component",
        "the multiplexer must hand a plain import to the plugin serving it plainly"
    );
}

/// An `(implements ..)` label declared under a plugin's `host.plugins`
/// entry routes to that plugin: ahead of id order, and whether or not the
/// plugin reports named-instance support of its own. The declaration also
/// lets it take a named entry beside the unnamed one.
#[tokio::test]
async fn test_a_declared_label_routes_to_the_declaring_plugin() {
    let iface = WitInterface::from("wasi:blobstore/container");

    // Sorts first and supports names, so without the declaration it would
    // claim the label.
    let mux = Arc::new(
        MockPlugin::new("aaa-multiplexed", vec![iface.clone()], vec![])
            .with_named_instance_support(),
    );
    let component = Arc::new(MockPlugin::new(
        "zzz-component",
        vec![iface.clone()],
        vec![],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(mux.id(), mux.clone() as Arc<dyn HostPlugin>);
    plugins.insert(component.id(), component.clone() as Arc<dyn HostPlugin>);

    let mut labeled = iface.clone();
    labeled.name = Some("tenant-a".to_string());
    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![iface, labeled],
    );

    let declared = crate::plugin::PluginBindings::new().with_plugin(
        crate::plugin::PluginBindingSet::new("zzz-component")
            .with_binding("tenant-a", Default::default()),
    );
    let bound_plugins = workload.bind_plugins(&plugins, &declared).await.unwrap();

    assert_eq!(
        bound_plugins.len(),
        1,
        "both entries must land on the declaring plugin, got {:?}",
        bound_plugins
            .iter()
            .map(|(p, _)| p.id())
            .collect::<Vec<_>>()
    );
    assert_eq!(bound_plugins[0].0.id(), "zzz-component");
    assert_eq!(
        mux.get_call_count("on_workload_bind"),
        0,
        "a label the operator declared elsewhere is not the multiplexer's"
    );
}

/// Tests basic plugin binding with one plugin and one component.
/// Verifies that `on_workload_bind` is called before `on_workload_item_bind`.
#[tokio::test]
async fn test_single_plugin_single_component() {
    // An interface the http_counter.wasm fixture actually imports. Declared
    // without a version so the test tracks the fixture across rebuilds; a
    // versionless interface matches any version (see WitInterface::same_package).
    let blobstore_interface = WitInterface::from("wasi:blobstore/container");

    let plugin = Arc::new(MockPlugin::new(
        "blobstore-plugin",
        vec![],
        vec![blobstore_interface.clone()],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    // Create workload with single component
    let components = vec![create_test_component("component1")];

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![blobstore_interface.clone()],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    // Verify plugin was called once for workload binding
    assert_eq!(plugin.get_call_count("on_workload_bind"), 1);

    // Verify plugin was called once for component binding
    assert_eq!(plugin.get_call_count("on_workload_item_bind"), 1);

    // Verify bound_plugins contains our plugin with the component
    assert_eq!(bound_plugins.len(), 1);
    let (_bound_plugin, component_ids) = &bound_plugins[0];
    assert_eq!(component_ids.len(), 1);

    // Verify call order
    let records = plugin.get_call_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].method, "on_workload_bind");
    assert_eq!(records[1].method, "on_workload_item_bind");
    assert_eq!(records[1].component_id.as_ref().unwrap(), &component_ids[0]);
}

/// Tests complex binding scenarios with multiple plugins and components.
/// Verifies that each plugin gets called once for workload binding.
#[tokio::test]
async fn test_multiple_plugins_multiple_components() {
    let http_interface = WitInterface::from("wasi:http/incoming-handler");
    let blobstore_interface = WitInterface::from("wasi:blobstore/blobstore");
    let keyvalue_interface = WitInterface::from("wasi:keyvalue/store");

    let http_plugin = Arc::new(MockPlugin::new(
        "http-plugin",
        vec![],
        vec![http_interface.clone()],
    ));

    let storage_plugin = Arc::new(MockPlugin::new(
        "storage-plugin",
        vec![],
        vec![blobstore_interface.clone(), keyvalue_interface.clone()],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(http_plugin.id(), http_plugin.clone() as Arc<dyn HostPlugin>);
    plugins.insert(
        storage_plugin.id(),
        storage_plugin.clone() as Arc<dyn HostPlugin>,
    );

    // Create components
    let components = vec![
        create_test_component("component1"),
        create_test_component("component2"),
        create_test_component("component3"),
    ];

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![
            http_interface.clone(),
            blobstore_interface.clone(),
            keyvalue_interface.clone(),
        ],
    );

    // Note: Due to the way world() works on real components, we can't easily mock it
    // This test verifies the structure and call patterns are correct
    let _bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    // Each plugin that matches should be in the result
    for (plugin, _component_ids) in &_bound_plugins {
        // Each plugin gets called once for on_workload_bind
        if plugin.id() == "http-plugin" {
            assert_eq!(http_plugin.get_call_count("on_workload_bind"), 1);
        } else if plugin.id() == "storage-plugin" {
            assert_eq!(storage_plugin.get_call_count("on_workload_bind"), 1);
        }
    }
}

/// Tests that when multiple plugins provide the same interface,
/// only one plugin gets bound to avoid duplicate interface handling.
#[tokio::test]
async fn test_no_duplicate_bindings() {
    let http_interface = WitInterface::from("wasi:http/incoming-handler");

    // Two plugins that both provide HTTP
    let plugin1 = Arc::new(MockPlugin::new(
        "http-plugin-1",
        vec![],
        vec![http_interface.clone()],
    ));

    let plugin2 = Arc::new(MockPlugin::new(
        "http-plugin-2",
        vec![],
        vec![http_interface.clone()],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin1.id(), plugin1.clone() as Arc<dyn HostPlugin>);
    plugins.insert(plugin2.id(), plugin2.clone() as Arc<dyn HostPlugin>);

    let components = vec![create_test_component("component1")];

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![http_interface.clone()],
    );

    let _bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    // Only one plugin should be bound per interface
    // Due to HashMap iteration order being unstable, we can't predict which one
    let total_workload_binds =
        plugin1.get_call_count("on_workload_bind") + plugin2.get_call_count("on_workload_bind");

    // Important: Only one plugin should handle the interface
    assert!(
        total_workload_binds <= 1,
        "Only one plugin should bind for a given interface"
    );
}

/// Tests error handling when a workload requests interfaces that no plugin provides.
/// The binding should fail gracefully with a descriptive error message.
#[tokio::test]
async fn test_missing_interface_fails() {
    let http_interface = WitInterface::from("wasi:http/incoming-handler");
    let blobstore_interface = WitInterface::from("wasi:blobstore/blobstore");

    // Plugin only provides HTTP
    let plugin = Arc::new(MockPlugin::new(
        "http-plugin",
        vec![],
        vec![http_interface.clone()],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    // Create a component - it will declare what it actually imports
    let components = vec![create_test_component("component1")];

    // Workload requests both HTTP and Blobstore interfaces
    // But only HTTP is available via plugins
    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![http_interface.clone(), blobstore_interface.clone()],
    );

    // The component imports wasi:blobstore but no plugin provides it, so
    // binding must fail. (The http interface is host-served and never
    // requires a plugin.)
    let err = match workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
    {
        Ok(_) => panic!("binding should fail for an unprovided interface"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("not available on this host"),
        "unexpected error: {err}"
    );
}

fn random_importer(version: &str) -> WorkloadComponent {
    component_from_wat(
        "random-importer",
        &format!(r#"(component (import "wasi:random/random@{version}" (instance)))"#),
    )
}

async fn bind_without_plugins(
    component: WorkloadComponent,
    host_interfaces: Vec<WitInterface>,
) -> anyhow::Result<Vec<(Arc<dyn HostPlugin + 'static>, Vec<String>)>> {
    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![component],
        host_interfaces,
    );
    workload
        .bind_plugins(&HashMap::new(), &crate::plugin::PluginBindings::new())
        .await
}

/// The host links the WASI base itself, so an entry pinning any version
/// compatible with the component's import needs no plugin.
#[tokio::test]
async fn a_wasi_builtin_pinned_at_a_compatible_version_needs_no_plugin() {
    for (imported, pinned) in [
        ("0.2.0", "wasi:random/random@0.2.2"),
        ("0.2.2", "wasi:random/random@0.2.2"),
        ("0.2.12", "wasi:random/random@0.2.2"),
        ("0.2.2", "wasi:random/random@0.2.0"),
        ("0.2.2", "wasi:random/random"),
        ("0.3.0", "wasi:random/random@0.3.0"),
        ("0.3.3", "wasi:random/random@0.3.3"),
    ] {
        let bound = bind_without_plugins(random_importer(imported), vec![pinned.into()])
            .await
            .unwrap_or_else(|e| panic!("import @{imported}, entry {pinned}: {e:#}"));
        assert!(bound.is_empty(), "import @{imported}, entry {pinned}");
    }
}

/// A version or label the host does not link still needs a plugin.
#[tokio::test]
async fn a_wasi_builtin_the_host_cannot_serve_still_needs_a_plugin() {
    let mut labelled = WitInterface::from("wasi:random/random@0.2.2");
    labelled.name = Some("custom".to_string());
    for (imported, entry) in [
        ("0.4.0", WitInterface::from("wasi:random/random@0.4.0")),
        ("0.4.0", WitInterface::from("wasi:random/random")),
        ("0.2.2", labelled),
    ] {
        let err = bind_without_plugins(random_importer(imported), vec![entry])
            .await
            .err()
            .unwrap_or_else(|| panic!("import @{imported} should not bind"));
        assert!(
            err.to_string().contains("not available on this host"),
            "unexpected error: {err}"
        );
    }
}

/// An unversioned entry is judged by the version the component imports, so
/// a WASI base version the host does not link still reaches a plugin.
#[tokio::test]
async fn an_unversioned_wasi_entry_reaches_a_plugin_for_a_version_the_host_lacks() {
    let plugin = Arc::new(MockPlugin::new(
        "random-plugin",
        vec![WitInterface::from("wasi:random/random@0.4.0")],
        vec![],
    ));
    let plugins: HashMap<&'static str, Arc<dyn HostPlugin>> =
        HashMap::from([(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>)]);
    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![random_importer("0.4.0")],
        vec![WitInterface::from("wasi:random/random")],
    );

    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(bound.len(), 1);
    assert_eq!(plugin.get_call_count("on_workload_item_bind"), 1);
}

/// A native plugin serves an entry pinned at any version compatible with
/// both its own and the component's.
#[tokio::test]
async fn a_native_plugin_serves_a_compatible_pinned_version() {
    let plugin = Arc::new(MockPlugin::new(
        "kv-plugin",
        vec![WitInterface::from("acme:kv/store@0.2.0")],
        vec![],
    ));
    let plugins: HashMap<&'static str, Arc<dyn HostPlugin>> =
        HashMap::from([(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>)]);
    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![component_from_wat(
            "kv-importer",
            r#"(component (import "acme:kv/store@0.2.1" (instance)))"#,
        )],
        vec![WitInterface::from("acme:kv/store@0.2.2")],
    );

    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(bound.len(), 1);
    assert_eq!(plugin.get_call_count("on_workload_item_bind"), 1);
}

/// A component exporting the unified WASI P3 `wasi:http/handler` interface
/// must bind with no HTTP plugin present, exactly like a P2
/// `incoming-handler` component: HTTP is served by the host, so the
/// requested interface must not fall through to plugin matching.
#[tokio::test]
async fn test_p3_http_handler_served_by_host() {
    let engine = wasmtime::Engine::default();
    let wasm = wat::parse_str(
        r#"(component
                (component $empty)
                (instance $i (instantiate $empty))
                (export "wasi:http/handler@0.3.0" (instance $i))
            )"#,
    )
    .unwrap();
    let component = Component::new(&engine, &wasm).unwrap();
    let workload_component = WorkloadComponent::new(
        "workload-p3-http".to_string(),
        "test-workload-p3-http".to_string(),
        "test-namespace".to_string(),
        "test-component".to_string(),
        component,
        Linker::new(&engine),
        Vec::new(),
        LocalResources::default(),
        Arc::default(),
        InstancePolicy::Ephemeral,
    );

    let mut http_interface = WitInterface::from("wasi:http/handler");
    http_interface
        .config
        .insert("host".to_string(), "localhost".to_string());
    assert!(http_interface.is_incoming_http_handler());

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![workload_component],
        vec![http_interface],
    );

    let bound_plugins = workload
        .bind_plugins(&HashMap::new(), &crate::plugin::PluginBindings::new())
        .await
        .unwrap();
    assert!(
        bound_plugins.is_empty(),
        "host-served wasi:http/handler must not require a plugin"
    );
}

/// Tests that plugin callbacks are invoked in the correct order:
/// `on_workload_bind` first, then `on_workload_item_bind` for each component.
#[tokio::test]
async fn test_plugin_callback_order() {
    let interface1 = WitInterface::from("test:interface/handler");

    let plugin = Arc::new(MockPlugin::new(
        "test-plugin",
        vec![],
        vec![interface1.clone()],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let components = vec![
        create_test_component("comp1"),
        create_test_component("comp2"),
    ];

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![interface1.clone()],
    );

    let _bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    // Verify callback order
    let records = plugin.get_call_records();

    // First call should always be on_workload_bind
    if !records.is_empty() {
        assert_eq!(
            records[0].method, "on_workload_bind",
            "on_workload_bind should be called before component bindings"
        );

        // All subsequent calls should be on_workload_item_bind
        for record in records.iter().skip(1) {
            assert_eq!(
                record.method, "on_workload_item_bind",
                "All calls after on_workload_bind should be on_workload_item_bind"
            );
        }
    }
}

#[tokio::test]
async fn test_world_includes_bidirectional() {
    let world = WitWorld {
        imports: HashSet::from([WitInterface::from("wasmcloud:messaging/handler")]),
        exports: HashSet::from([WitInterface::from("wasmcloud:messaging/consumer,types")]),
    };

    let interface1 = WitInterface::from("wasmcloud:messaging/handler");
    let interface2 = WitInterface::from("wasmcloud:messaging/consumer,types");
    let interface3 = WitInterface::from("wasmcloud:messaging/handler,consumer,types");
    let interface4 = WitInterface::from("wasmcloud:messaging/producer");

    assert!(world.includes_bidirectional(&interface1));
    assert!(world.includes_bidirectional(&interface2));
    assert!(world.includes_bidirectional(&interface3));
    assert!(!world.includes_bidirectional(&interface4));
    // Show the difference between includes and includes_bidirectional
    assert!(!world.includes(&interface3));
}

/// Tests topological sort with a chain dependency: A -> B -> C
/// Expected order: C, B, A (or any valid topological order)
#[test]
fn build_export_map_registers_unique_exports() {
    let exports = vec![
        (
            ComponentExporter {
                name: Arc::from("comp-a"),
                id: Arc::from("comp-a"),
            },
            vec!["wasi:http/incoming-handler".to_string()],
        ),
        (
            ComponentExporter {
                name: Arc::from("comp-b"),
                id: Arc::from("comp-b"),
            },
            vec!["custom:pkg/iface".to_string()],
        ),
    ];
    let ExportMaps {
        unambiguous: map,
        ambiguous,
        ..
    } = build_export_map(&exports);
    assert_eq!(
        map.get("custom:pkg/iface").map(|c| c.as_ref()),
        Some("comp-b")
    );
    assert!(ambiguous.is_empty());
}

#[test]
fn build_export_map_marks_duplicate_exports_ambiguous() {
    // Two workers exporting the same handler interface: ambiguous, and
    // dropped from the resolvable map (only an importer would error).
    let exports = vec![
        (
            ComponentExporter {
                name: Arc::from("task-leet"),
                id: Arc::from("task-leet"),
            },
            vec!["wasmcloud:messaging/handler@0.2.0".to_string()],
        ),
        (
            ComponentExporter {
                name: Arc::from("task-reverse"),
                id: Arc::from("task-reverse"),
            },
            vec!["wasmcloud:messaging/handler@0.2.0".to_string()],
        ),
    ];
    let ExportMaps {
        unambiguous: map,
        ambiguous,
        ..
    } = build_export_map(&exports);
    assert!(!map.contains_key("wasmcloud:messaging/handler@0.2.0"));
    assert!(ambiguous.contains("wasmcloud:messaging/handler@0.2.0"));
}

#[test]
fn build_export_map_three_exporters_stay_ambiguous() {
    // A third exporter must not accidentally re-register the interface.
    let iface = "wasmcloud:messaging/handler@0.2.0".to_string();
    let exports = vec![
        (
            ComponentExporter {
                name: Arc::from("a"),
                id: Arc::from("a"),
            },
            vec![iface.clone()],
        ),
        (
            ComponentExporter {
                name: Arc::from("b"),
                id: Arc::from("b"),
            },
            vec![iface.clone()],
        ),
        (
            ComponentExporter {
                name: Arc::from("c"),
                id: Arc::from("c"),
            },
            vec![iface.clone()],
        ),
    ];
    let ExportMaps {
        unambiguous: map,
        ambiguous,
        ..
    } = build_export_map(&exports);
    assert!(!map.contains_key(&iface));
    assert!(ambiguous.contains(&iface));
}

#[test]
fn exporters_keeps_every_candidate_an_ambiguous_interface_dropped() {
    // `unambiguous` collapses to nothing once two components export the
    // same interface — which is precisely when a label needs the
    // candidates. They have to survive that collapse.
    let iface = "example:feeds/reader@0.1.0".to_string();
    let exports = vec![
        (
            ComponentExporter {
                name: Arc::from("feed-a"),
                id: Arc::from("id-1"),
            },
            vec![iface.clone()],
        ),
        (
            ComponentExporter {
                name: Arc::from("feed-b"),
                id: Arc::from("id-2"),
            },
            vec![iface.clone()],
        ),
    ];
    let ExportMaps {
        unambiguous: map,
        ambiguous,
        exporters,
    } = build_export_map(&exports);
    assert!(
        !map.contains_key(&iface),
        "ambiguous, so not directly resolvable"
    );
    assert!(ambiguous.contains(&iface));
    let names: Vec<&str> = exporters[&iface].iter().map(|e| e.name.as_ref()).collect();
    assert_eq!(names, vec!["feed-a", "feed-b"]);
    // The ids are the runtime's UUIDs, not the manifest names. Matching a
    // label against an id is what made the first version of this inert.
    let ids: Vec<&str> = exporters[&iface].iter().map(|e| e.id.as_ref()).collect();
    assert_eq!(ids, vec!["id-1", "id-2"]);
}

#[test]
fn resolve_import_unwraps_the_implements_annotation() {
    // A plain import names its interface. A labelled one names the LABEL and
    // carries the interface in `implements`; reading the name alone is what
    // made a labelled import miss the export map entirely.
    assert_eq!(
        resolve_import("example:feeds/reader@0.1.0", None),
        ("example:feeds/reader@0.1.0", None)
    );
    assert_eq!(
        resolve_import("feed-a", Some("example:feeds/reader@0.1.0")),
        ("example:feeds/reader@0.1.0", Some("feed-a"))
    );
}

#[test]
fn a_label_routes_to_the_component_it_names() {
    let iface = "example:feeds/reader@0.1.0".to_string();
    let exports = vec![
        (
            ComponentExporter {
                name: Arc::from("feed-a"),
                id: Arc::from("id-1"),
            },
            vec![iface.clone()],
        ),
        (
            ComponentExporter {
                name: Arc::from("feed-b"),
                id: Arc::from("id-2"),
            },
            vec![iface.clone()],
        ),
    ];
    let ExportMaps { exporters, .. } = build_export_map(&exports);

    // Label in is the MANIFEST name; what comes back is the runtime id the
    // component map is keyed by.
    assert_eq!(
        labelled_exporter(&exporters, &iface, "feed-a")
            .unwrap()
            .map(|(id, export)| (id.as_ref(), export)),
        Some(("id-1", iface.as_str()))
    );
    assert_eq!(
        labelled_exporter(&exporters, &iface, "feed-b")
            .unwrap()
            .map(|(id, export)| (id.as_ref(), export)),
        Some(("id-2", iface.as_str()))
    );
    // A label matching a runtime id must NOT resolve: ids are UUIDs a user
    // never sees and never writes in a manifest.
    assert!(
        labelled_exporter(&exporters, &iface, "id-1")
            .unwrap()
            .is_none()
    );
    // A label naming no component in the workload resolves to nothing, so
    // the caller falls through to the existing paths rather than guessing.
    assert!(
        labelled_exporter(&exporters, &iface, "feed-c")
            .unwrap()
            .is_none()
    );
    // A component that exists but does not export this interface is not a
    // candidate for it.
    let other = "wasi:keyvalue/store@0.2.0".to_string();
    assert!(
        labelled_exporter(&exporters, &other, "feed-a")
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_label_resolves_a_compatible_export_version() {
    let imported = "example:feeds/reader@0.1.0";
    let exported = "example:feeds/reader@0.1.1";
    let exports = build_export_map(&[(
        ComponentExporter {
            name: Arc::from("feed-a"),
            id: Arc::from("id-1"),
        },
        vec![exported.to_string()],
    )]);

    assert_eq!(
        labelled_exporter(&exports.exporters, imported, "feed-a")
            .unwrap()
            .map(|(id, export)| (id.as_ref(), export)),
        Some(("id-1", exported))
    );
    assert!(
        labelled_exporter(&exports.exporters, "example:feeds/reader@0.2.0", "feed-a")
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_exact_version_takes_precedence_over_compatible_exports() {
    let imported = "example:feeds/reader@0.1.0";
    let exports = build_export_map(&[
        (
            ComponentExporter {
                name: Arc::from("feed-a"),
                id: Arc::from("compatible"),
            },
            vec!["example:feeds/reader@0.1.1".to_string()],
        ),
        (
            ComponentExporter {
                name: Arc::from("feed-a"),
                id: Arc::from("exact"),
            },
            vec![imported.to_string()],
        ),
    ]);

    assert_eq!(
        labelled_exporter(&exports.exporters, imported, "feed-a")
            .unwrap()
            .map(|(id, export)| (id.as_ref(), export)),
        Some(("exact", imported))
    );
}

#[test]
fn a_duplicated_component_name_is_refused_rather_than_guessed() {
    // Two components sharing a manifest name, both exporting the interface:
    // the label names both. Picking one silently mis-routes half the calls,
    // and the winner would be whichever UUID sorted first.
    let iface = "example:feeds/reader@0.1.0".to_string();
    let exports = vec![
        (
            ComponentExporter {
                name: Arc::from("feed-a"),
                id: Arc::from("id-1"),
            },
            vec![iface.clone()],
        ),
        (
            ComponentExporter {
                name: Arc::from("feed-a"),
                id: Arc::from("id-2"),
            },
            vec![iface.clone()],
        ),
    ];
    let ExportMaps { exporters, .. } = build_export_map(&exports);
    let err = labelled_exporter(&exporters, &iface, "feed-a")
        .expect_err("a duplicated name must not resolve");
    assert!(err.to_string().contains("ambiguous"), "{err}");
    // A name that is still unique resolves normally.
    assert!(
        labelled_exporter(&exporters, &iface, "feed-b")
            .expect("unambiguous")
            .is_none()
    );
}

#[test]
fn expand_link_closure_adds_nested_links_only() {
    let a: Arc<str> = Arc::from("component-a");
    let b: Arc<str> = Arc::from("component-b");
    let c: Arc<str> = Arc::from("component-c");
    let unrelated: Arc<str> = Arc::from("unrelated");

    let direct_links = HashSet::from([a.clone()]);
    let resolved_links = HashMap::from([
        (a.clone(), HashSet::from([b.clone(), c.clone()])),
        (unrelated.clone(), HashSet::from([Arc::from("ignored")])),
    ]);

    let closure = expand_link_closure(&direct_links, &resolved_links);

    assert_eq!(closure.len(), 3);
    assert!(closure.contains(&a));
    assert!(closure.contains(&b));
    assert!(closure.contains(&c));
    assert!(!closure.contains(&unrelated));
}

#[test]
fn test_topological_sort_chain() {
    let a: Arc<str> = Arc::from("component-a");
    let b: Arc<str> = Arc::from("component-b");
    let c: Arc<str> = Arc::from("component-c");

    // A depends on B, B depends on C
    let mut dependencies: HashMap<Arc<str>, HashSet<Arc<str>>> = HashMap::new();
    dependencies.insert(a.clone(), HashSet::from([b.clone()]));
    dependencies.insert(b.clone(), HashSet::from([c.clone()]));
    dependencies.insert(c.clone(), HashSet::new());

    let result = topological_sort_components(&dependencies).unwrap();

    // C should come before B, and B should come before A
    let c_pos = result.iter().position(|x| x == &c).unwrap();
    let b_pos = result.iter().position(|x| x == &b).unwrap();
    let a_pos = result.iter().position(|x| x == &a).unwrap();

    assert!(
        c_pos < b_pos,
        "C should be processed before B: C at {c_pos}, B at {b_pos}"
    );
    assert!(
        b_pos < a_pos,
        "B should be processed before A: B at {b_pos}, A at {a_pos}"
    );
}

/// Tests topological sort with no dependencies
#[test]
fn test_topological_sort_no_dependencies() {
    let a: Arc<str> = Arc::from("component-a");
    let b: Arc<str> = Arc::from("component-b");
    let c: Arc<str> = Arc::from("component-c");

    let mut dependencies: HashMap<Arc<str>, HashSet<Arc<str>>> = HashMap::new();
    dependencies.insert(a.clone(), HashSet::new());
    dependencies.insert(b.clone(), HashSet::new());
    dependencies.insert(c.clone(), HashSet::new());

    let result = topological_sort_components(&dependencies).unwrap();

    // All components should be present
    assert_eq!(result.len(), 3);
    assert!(result.contains(&a));
    assert!(result.contains(&b));
    assert!(result.contains(&c));
}

/// Tests topological sort with diamond dependency: A -> B, A -> C, B -> D, C -> D
#[test]
fn test_topological_sort_diamond() {
    let a: Arc<str> = Arc::from("component-a");
    let b: Arc<str> = Arc::from("component-b");
    let c: Arc<str> = Arc::from("component-c");
    let d: Arc<str> = Arc::from("component-d");

    // A depends on B and C, both B and C depend on D
    let mut dependencies: HashMap<Arc<str>, HashSet<Arc<str>>> = HashMap::new();
    dependencies.insert(a.clone(), HashSet::from([b.clone(), c.clone()]));
    dependencies.insert(b.clone(), HashSet::from([d.clone()]));
    dependencies.insert(c.clone(), HashSet::from([d.clone()]));
    dependencies.insert(d.clone(), HashSet::new());

    let result = topological_sort_components(&dependencies).unwrap();

    let a_pos = result.iter().position(|x| x == &a).unwrap();
    let b_pos = result.iter().position(|x| x == &b).unwrap();
    let c_pos = result.iter().position(|x| x == &c).unwrap();
    let d_pos = result.iter().position(|x| x == &d).unwrap();

    // D should come before B and C
    assert!(d_pos < b_pos, "D should be processed before B");
    assert!(d_pos < c_pos, "D should be processed before C");
    // B and C should come before A
    assert!(b_pos < a_pos, "B should be processed before A");
    assert!(c_pos < a_pos, "C should be processed before A");
}

/// Tests topological sort with circular dependency detection
#[test]
fn test_topological_sort_circular_dependency() {
    let a: Arc<str> = Arc::from("component-a");
    let b: Arc<str> = Arc::from("component-b");
    let c: Arc<str> = Arc::from("component-c");

    // Circular: A -> B -> C -> A
    let mut dependencies: HashMap<Arc<str>, HashSet<Arc<str>>> = HashMap::new();
    dependencies.insert(a.clone(), HashSet::from([b.clone()]));
    dependencies.insert(b.clone(), HashSet::from([c.clone()]));
    dependencies.insert(c.clone(), HashSet::from([a.clone()]));

    let result = topological_sort_components(&dependencies);
    assert!(
        result.is_err(),
        "Should detect circular dependency: {result:?}"
    );
}

/// A component exporting the handler half of a `consumer,handler` entry
/// binds the messaging plugin, so its handler is subscribed. That entry is
/// how a workload which both publishes and handles is written, often with
/// the two halves in different components.
#[tokio::test]
async fn a_component_exporting_half_an_entry_binds_the_plugin() {
    let messaging_handler = WitInterface {
        namespace: "wasmcloud".to_string(),
        package: "messaging".to_string(),
        interfaces: ["handler".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.2.0").unwrap()),
        config: Default::default(),
        name: None,
    };

    let messaging_consumer = WitInterface {
        namespace: "wasmcloud".to_string(),
        package: "messaging".to_string(),
        interfaces: ["consumer".to_string(), "types".to_string()]
            .into_iter()
            .collect(),
        version: Some(semver::Version::parse("0.2.0").unwrap()),
        config: Default::default(),
        name: None,
    };

    let logging = WitInterface {
        namespace: "wasi".to_string(),
        package: "logging".to_string(),
        interfaces: ["logging".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.1.0-draft").unwrap()),
        config: Default::default(),
        name: None,
    };

    let messaging_plugin = Arc::new(MockPlugin::new(
        "messaging-plugin",
        vec![messaging_consumer],
        vec![messaging_handler],
    ));

    let logging_plugin = Arc::new(MockPlugin::new(
        "logging-plugin",
        vec![logging.clone()],
        vec![],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(
        logging_plugin.id(),
        logging_plugin.clone() as Arc<dyn HostPlugin>,
    );
    plugins.insert(
        messaging_plugin.id(),
        messaging_plugin.clone() as Arc<dyn HostPlugin>,
    );

    // Create workload with single component
    let components = vec![create_test_messaging_component("component")];

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        vec![
            WitInterface {
                namespace: "wasmcloud".to_string(),
                package: "messaging".to_string(),
                interfaces: ["consumer".to_string(), "handler".to_string()]
                    .into_iter()
                    .collect(),
                version: Some(semver::Version::parse("0.2.0").unwrap()),
                config: Default::default(),
                name: None,
            },
            logging,
        ],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    // The component imports `wasi:logging`, so the logging plugin binds it.
    assert_eq!(logging_plugin.get_call_count("on_workload_bind"), 1);
    assert_eq!(logging_plugin.get_call_count("on_workload_item_bind"), 1);

    // It exports `wasmcloud:messaging/handler`, one of the two interfaces
    // the messaging entry names, so the messaging plugin binds it too —
    // without that bind nothing subscribes and the handler never runs.
    assert_eq!(messaging_plugin.get_call_count("on_workload_bind"), 1);
    assert_eq!(messaging_plugin.get_call_count("on_workload_item_bind"), 1);

    assert_eq!(bound_plugins.len(), 2);
}

#[tokio::test]
async fn test_single_plugin_single_service() {
    let logging_interface = WitInterface {
        namespace: "wasi".to_string(),
        package: "logging".to_string(),
        interfaces: ["logging".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.1.0-draft").unwrap()),
        config: Default::default(),
        name: None,
    };

    let plugin = Arc::new(MockPlugin::new(
        "logging-plugin",
        vec![logging_interface.clone()],
        vec![],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    // Create workload with single component
    let service = create_test_service_component("service");

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        Some(service),
        vec![],
        vec![logging_interface.clone()],
    );

    let bound_plugins = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    // Verify plugin was called once for workload binding
    assert_eq!(plugin.get_call_count("on_workload_bind"), 1);

    // Verify plugin was called once for service binding
    assert_eq!(plugin.get_call_count("on_workload_item_bind"), 1);

    // Verify bound_plugins contains our plugin with the component
    assert_eq!(bound_plugins.len(), 1);
    let (_bound_plugin, component_ids) = &bound_plugins[0];
    assert_eq!(component_ids.len(), 1);
}

fn keyvalue_interface(name: Option<&str>) -> WitInterface {
    WitInterface {
        namespace: "wasi".to_string(),
        package: "keyvalue".to_string(),
        interfaces: ["store".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.2.0-draft").unwrap()),
        config: Default::default(),
        name: name.map(String::from),
    }
}

/// Two named `wasi:keyvalue` entries, plugin doesn't support naming -> error
#[tokio::test]
async fn test_named_interfaces_fail_without_plugin_support() {
    let plugin = Arc::new(MockPlugin::new(
        "keyvalue-plugin",
        vec![],
        vec![keyvalue_interface(None)],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![
            keyvalue_interface(Some("cache")),
            keyvalue_interface(Some("sessions")),
        ],
    );

    let result = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await;
    match result {
        Ok(_) => panic!("Expected error for unsupported named instances"),
        Err(e) => {
            let err_msg = format!("{e}");
            assert!(
                err_msg.contains("does not support named instances"),
                "Expected 'does not support named instances' error, got: {err_msg}"
            );
        }
    }
}

/// A named entry beside an unnamed one is two bindings for one package, so
/// a plugin serving a single instance has nowhere to route the second.
#[tokio::test]
async fn test_a_named_entry_beside_an_unnamed_one_is_refused() {
    let plugin = Arc::new(MockPlugin::new(
        "keyvalue-plugin",
        vec![],
        vec![keyvalue_interface(None)],
    ));
    let plugins = HashMap::from([(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>)]);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![keyvalue_interface(None), keyvalue_interface(Some("cache"))],
    );

    let err = match workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
    {
        Ok(_) => panic!("two bindings cannot share one instance"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err.contains("does not support named instances") && err.contains("cache"),
        "the refusal must name the binding that has nowhere to go, got: {err}"
    );
}

/// Plugins bind in id order, so a refusal partway through leaves earlier
/// ones holding state — a connection, a subscription — for a workload that
/// never deploys.
#[tokio::test]
async fn test_a_refused_binding_rolls_back_the_plugins_already_bound() {
    let earlier = Arc::new(RollbackPlugin::new(None));
    let later = Arc::new(MockPlugin::new(
        "zz-keyvalue-plugin",
        vec![],
        vec![keyvalue_interface(None)],
    ));
    let mut plugins = earlier.registered();
    plugins.insert(later.id(), later.clone() as Arc<dyn HostPlugin>);

    let importer = component_from_wat(
        "importer",
        &format!(
            r#"(component
                     (import "{MARKER}" (instance))
                     (import "wasi:keyvalue/store@0.2.0-draft" (instance)))"#
        ),
    );
    let mut workload = UnresolvedWorkload::new(
        "rollback",
        "rollback",
        "test-namespace",
        None,
        vec![importer],
        vec![
            WitInterface::from(MARKER),
            keyvalue_interface(None),
            keyvalue_interface(Some("cache")),
        ],
    );

    assert!(
        workload
            .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
            .await
            .is_err(),
        "two bindings cannot share one instance"
    );
    assert_eq!(
        earlier.unbind_count(),
        1,
        "the plugin bound before the refusal must be unbound"
    );
}

/// A lone name is one binding: it selects the operator's declaration for
/// this plugin and is delivered to it, which a single instance can serve.
#[tokio::test]
async fn test_a_lone_named_entry_binds_a_single_instance_plugin() {
    let plugin = Arc::new(MockPlugin::new(
        "keyvalue-plugin",
        vec![],
        vec![keyvalue_interface(None)],
    ));
    let plugins = HashMap::from([(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>)]);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![keyvalue_interface(Some("cache"))],
    );

    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .expect("one binding needs no routing");
    assert_eq!(bound.len(), 1);
}

/// Same setup but plugin returns `supports_named_instances() == true` -> succeeds
#[tokio::test]
async fn test_named_interfaces_succeed_with_plugin_support() {
    let plugin = Arc::new(
        MockPlugin::new("keyvalue-plugin", vec![], vec![keyvalue_interface(None)])
            .with_named_instance_support(),
    );

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![
            keyvalue_interface(Some("cache")),
            keyvalue_interface(Some("sessions")),
        ],
    );

    let result = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await;
    if let Err(e) = result {
        panic!("Expected success but got error: {e}");
    }
}

/// Only one named entry -> no multiplexing needed, passes even without plugin support
#[tokio::test]
async fn test_single_named_interface_no_validation() {
    let plugin = Arc::new(MockPlugin::new(
        "keyvalue-plugin",
        vec![],
        vec![keyvalue_interface(None)],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![keyvalue_interface(Some("cache"))],
    );

    let result = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await;
    if let Err(e) = result {
        panic!("Single named entry should not require named instance support: {e}");
    }
}

/// Coexistence: a regular (standalone) plugin and a named-capable
/// (multiplexed) plugin both matching the same package bind together, with
/// the unnamed interface routed to the regular plugin and the `(implements
/// ..)` named interfaces routed to the named-capable one. Without the
/// name-aware dispatch this errors (the regular plugin would claim the two
/// named entries it cannot serve).
#[tokio::test]
async fn test_named_and_regular_plugins_coexist() {
    let regular = Arc::new(MockPlugin::new(
        "kv-standalone",
        vec![],
        vec![keyvalue_interface(None)],
    ));
    let multiplexed = Arc::new(
        MockPlugin::new("kv-multiplexed", vec![], vec![keyvalue_interface(None)])
            .with_named_instance_support(),
    );

    let mut plugins = HashMap::new();
    plugins.insert(regular.id(), regular.clone() as Arc<dyn HostPlugin>);
    plugins.insert(multiplexed.id(), multiplexed.clone() as Arc<dyn HostPlugin>);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![
            keyvalue_interface(None),
            keyvalue_interface(Some("cache")),
            keyvalue_interface(Some("sessions")),
        ],
    );

    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .expect("standalone + multiplexed plugins should bind together");
    let bound_ids: std::collections::HashSet<&str> = bound.iter().map(|(p, _)| p.id()).collect();
    assert!(
        bound_ids.contains("kv-standalone"),
        "regular plugin must bind the unnamed interface"
    );
    assert!(
        bound_ids.contains("kv-multiplexed"),
        "named-capable plugin must bind the named interfaces"
    );
}

/// Existing unnamed entries -> no change in behavior
#[tokio::test]
async fn test_unnamed_interfaces_backwards_compatible() {
    let iface = WitInterface {
        namespace: "wasi".to_string(),
        package: "blobstore".to_string(),
        interfaces: ["container".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.2.0-draft").unwrap()),
        config: Default::default(),
        name: None,
    };

    let plugin = Arc::new(MockPlugin::new(
        "blobstore-plugin",
        vec![],
        vec![iface.clone()],
    ));

    let mut plugins = HashMap::new();
    plugins.insert(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>);

    let mut workload = UnresolvedWorkload::new(
        "test-workload-id".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        vec![create_test_component("component1")],
        vec![iface],
    );

    let result = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await;
    if let Err(e) = result {
        panic!("Unnamed interfaces should work as before: {e}");
    }
}

#[test]
fn stable_workload_name_filtering_and_fallback() {
    // 1. Valid deployment annotation with surrounding whitespace gets trimmed
    let mut annotations = HashMap::new();
    annotations.insert(
        WORKLOAD_DEPLOYMENT_ANNOTATION.to_string(),
        "  my-deployment  ".to_string(),
    );
    let comp = create_test_component("c").with_annotations(annotations);
    assert_eq!(comp.stable_workload_name(), "my-deployment");

    // 2. Blank deployment annotation falls back to daemonset
    let mut annotations = HashMap::new();
    annotations.insert(
        WORKLOAD_DEPLOYMENT_ANNOTATION.to_string(),
        "   ".to_string(),
    );
    annotations.insert(
        WORKLOAD_DAEMONSET_ANNOTATION.to_string(),
        "my-daemonset".to_string(),
    );
    annotations.insert(
        WORKLOAD_REPLICASET_ANNOTATION.to_string(),
        "my-replicaset".to_string(),
    );
    let comp = create_test_component("c").with_annotations(annotations);
    assert_eq!(comp.stable_workload_name(), "my-daemonset");

    // Without a daemonset, a blank deployment annotation falls back to replicaset
    let mut annotations = HashMap::new();
    annotations.insert(
        WORKLOAD_DEPLOYMENT_ANNOTATION.to_string(),
        "   ".to_string(),
    );
    annotations.insert(
        WORKLOAD_REPLICASET_ANNOTATION.to_string(),
        "my-replicaset".to_string(),
    );
    let comp = create_test_component("c").with_annotations(annotations);
    assert_eq!(comp.stable_workload_name(), "my-replicaset");

    // 3. Blank deployment and blank replicaset falls back to legacy replicaset
    let mut annotations = HashMap::new();
    annotations.insert(WORKLOAD_DEPLOYMENT_ANNOTATION.to_string(), "".to_string());
    annotations.insert(
        WORKLOAD_REPLICASET_ANNOTATION.to_string(),
        "   ".to_string(),
    );
    annotations.insert(
        WORKLOAD_REPLICASET_LEGACY_ANNOTATION.to_string(),
        "my-legacy-rs".to_string(),
    );
    let comp = create_test_component("c").with_annotations(annotations);
    assert_eq!(comp.stable_workload_name(), "my-legacy-rs");

    // 4. Blank replicasets fall back to k8s app name
    let mut annotations = HashMap::new();
    annotations.insert(WORKLOAD_REPLICASET_ANNOTATION.to_string(), "".to_string());
    annotations.insert(
        WORKLOAD_REPLICASET_LEGACY_ANNOTATION.to_string(),
        "".to_string(),
    );
    annotations.insert(K8S_APP_NAME_ANNOTATION.to_string(), "my-app".to_string());
    let comp = create_test_component("c").with_annotations(annotations);
    assert_eq!(comp.stable_workload_name(), "my-app");

    // 5. All blank annotations fall back to raw workload_name
    let mut annotations = HashMap::new();
    annotations.insert(WORKLOAD_DEPLOYMENT_ANNOTATION.to_string(), "".to_string());
    annotations.insert(
        WORKLOAD_REPLICASET_ANNOTATION.to_string(),
        "   ".to_string(),
    );
    annotations.insert(
        WORKLOAD_REPLICASET_LEGACY_ANNOTATION.to_string(),
        "".to_string(),
    );
    annotations.insert(K8S_APP_NAME_ANNOTATION.to_string(), "   ".to_string());
    let comp = create_test_component("c").with_annotations(annotations);
    assert_eq!(comp.stable_workload_name(), "test-workload-c");
}

/// A grant matches the orchestrator's replica group, never the
/// application name annotation, which anything may carry.
#[test]
fn a_grant_name_is_the_replica_group_or_the_workload_name() {
    let workload = |annotations: &[(&str, &str)]| {
        let annotations = annotations
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        UnresolvedWorkload::new(
            "id".to_string(),
            "billing-api".to_string(),
            "billing".to_string(),
            None,
            vec![],
            vec![],
        )
        .with_annotations(&annotations)
    };
    for (annotations, expected) in [
        (&[][..], "billing-api"),
        (&[(K8S_APP_NAME_ANNOTATION, "billing")][..], "billing-api"),
        (
            &[(WORKLOAD_REPLICASET_ANNOTATION, "billing-rs")][..],
            "billing-rs",
        ),
        (
            &[
                (WORKLOAD_REPLICASET_ANNOTATION, "billing-rs"),
                (WORKLOAD_DEPLOYMENT_ANNOTATION, "billing"),
            ][..],
            "billing",
        ),
    ] {
        assert_eq!(
            workload(annotations).grant_name(),
            expected,
            "{annotations:?}"
        );
    }
}

/// Like [`component_from_wat`], but on an engine that parses
/// `(implements ..)`. The default engine does not.
fn implements_component_from_wat(id: &str, wat: &str) -> WorkloadComponent {
    let mut cfg = wasmtime::Config::new();
    cfg.wasm_component_model_implements(true);
    let engine = wasmtime::Engine::new(&cfg).expect("failed to build engine");
    let linker = Linker::new(&engine);
    let wasm = wat::parse_str(wat).expect("failed to parse WAT");
    let component = Component::new(&engine, &wasm).expect("failed to compile");
    WorkloadComponent::new(
        "workload-served-within".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        id.to_string(),
        component,
        linker,
        Vec::new(),
        LocalResources::default(),
        Arc::default(),
        InstancePolicy::Ephemeral,
    )
}

fn implements_marker_exporter(name: &str) -> WorkloadComponent {
    implements_component_from_wat(
        name,
        &format!(r#"(component (instance $marker) (export "{MARKER}" (instance $marker)))"#),
    )
}

/// An importer that imports MARKER once per label in `labels`.
fn labelled_marker_importer(name: &str, labels: &[&str]) -> WorkloadComponent {
    let imports = labels
        .iter()
        .map(|l| format!(r#"(import "{l}" (implements "{MARKER}") (instance))"#))
        .collect::<Vec<_>>()
        .join(" ");
    implements_component_from_wat(name, &format!("(component {imports})"))
}

/// The host defines only the plain WASI instance, so an import labelled
/// with a plugin's id still binds that plugin under an unnamed entry.
#[tokio::test]
async fn a_labelled_wasi_import_still_binds_its_plugin() {
    let plugin = Arc::new(MockPlugin::new(
        "random-plugin",
        vec![WitInterface::from("wasi:random/random@0.2.2")],
        vec![],
    ));
    let plugins: HashMap<&'static str, Arc<dyn HostPlugin>> =
        HashMap::from([(plugin.id(), plugin.clone() as Arc<dyn HostPlugin>)]);
    let mut workload = served_within_workload_fixture(
        vec![implements_component_from_wat(
            "random-importer",
            r#"(component (import "random-plugin" (implements "wasi:random/random@0.2.2") (instance)))"#,
        )],
        vec![WitInterface::from("wasi:random/random")],
    );

    let bound = workload
        .bind_plugins(&plugins, &crate::plugin::PluginBindings::new())
        .await
        .unwrap();

    assert_eq!(bound.len(), 1);
    assert_eq!(plugin.get_call_count("on_workload_item_bind"), 1);
}

fn served_within_workload_fixture(
    components: Vec<WorkloadComponent>,
    entries: Vec<WitInterface>,
) -> UnresolvedWorkload {
    UnresolvedWorkload::new(
        "workload-served-within".to_string(),
        "test-workload".to_string(),
        "test-namespace".to_string(),
        None,
        components,
        entries,
    )
}

fn marker_entry(name: Option<&str>) -> WitInterface {
    let mut i = WitInterface::from(MARKER);
    i.name = name.map(str::to_string);
    i
}

fn named_instance_marker_plugin() -> HashMap<&'static str, Arc<dyn HostPlugin>> {
    let plugin = Arc::new(
        MockPlugin::new("marker-plugin", vec![], vec![WitInterface::from(MARKER)])
            .with_named_instance_support()
            .serving_unnamed_too(),
    );
    HashMap::from([(plugin.id(), plugin as Arc<dyn HostPlugin>)])
}

async fn plugin_bound_ids(workload: &mut UnresolvedWorkload) -> HashSet<String> {
    let bound = workload
        .bind_plugins(
            &named_instance_marker_plugin(),
            &crate::plugin::PluginBindings::new(),
        )
        .await
        .expect("bind_plugins should succeed");
    bound_component_ids(&bound)
}

/// Two exporters is the ambiguity `build_export_map` declines to resolve. A
/// label naming one of them settles it, so the import stays in the workload
/// and must NOT be handed to a plugin.
#[tokio::test]
async fn a_label_settles_the_ambiguity_for_plugin_matching() {
    let importer = labelled_marker_importer("importer", &["exporter-a"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        vec![
            importer,
            implements_marker_exporter("exporter-a"),
            implements_marker_exporter("exporter-b"),
        ],
        vec![WitInterface::from(MARKER)],
    );
    assert!(
        !plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "a label naming a sibling exporter should keep the import in-workload"
    );
}

/// The control for the test above: a label naming no component is one the
/// operator meant for a host interface, so it must still reach the plugin.
#[tokio::test]
async fn a_label_naming_no_component_falls_to_the_plugin() {
    let importer = labelled_marker_importer("importer", &["nobody"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        vec![
            importer,
            implements_marker_exporter("exporter-a"),
            implements_marker_exporter("exporter-b"),
        ],
        vec![WitInterface::from(MARKER)],
    );
    assert!(
        plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "a label naming no component must be left to the plugin"
    );
}

/// One component importing the same interface twice, once under a label
/// naming a sibling and once under a label meant for a host plugin. The
/// verdict is per-entry, so answering for only one of the two imports would
/// drop the entry and leave the plugin label with no provider at all.
#[tokio::test]
async fn a_sibling_label_does_not_answer_for_a_plugin_label() {
    let importer = labelled_marker_importer("importer", &["exporter-a", "plug"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        vec![
            importer,
            implements_marker_exporter("exporter-a"),
            implements_marker_exporter("exporter-b"),
        ],
        vec![marker_entry(Some("plug"))],
    );
    assert!(
        plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "the `plug` label still needs the plugin, even though a sibling \
             label on the same interface is served in-workload"
    );
}

/// The control for the test above: the same importer carrying ONLY the
/// plugin label binds, so a failure there is attributable to the sibling.
#[tokio::test]
async fn a_plugin_label_alone_binds_the_plugin() {
    let importer = labelled_marker_importer("importer", &["plug"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        vec![
            importer,
            implements_marker_exporter("exporter-a"),
            implements_marker_exporter("exporter-b"),
        ],
        vec![marker_entry(Some("plug"))],
    );
    assert!(
        plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "control: a lone plugin label must bind the plugin"
    );
}

/// A single sibling exporter must not swallow a plugin label either. The
/// unlabelled single-exporter route is for unlabelled imports; a label
/// naming nobody belongs to the host.
#[tokio::test]
async fn a_plugin_label_survives_a_single_sibling_exporter() {
    let importer = labelled_marker_importer("importer", &["plug"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        vec![importer, implements_marker_exporter("exporter-a")],
        vec![marker_entry(Some("plug"))],
    );
    assert!(
        plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "one sibling exporter must not swallow a label meant for the host"
    );
}

/// Direction 1: the label IS declared under `hostInterfaces`. A component
/// named the same as that label, exporting the very interface the host
/// serves, must NOT capture it. Before the precedence rule this silently
/// routed guest-to-guest while the manifest still asked for the host.
#[tokio::test]
async fn a_declared_host_label_beats_a_component_of_the_same_name() {
    let importer = labelled_marker_importer("importer", &["plug"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        // The squatter: named `plug`, exports what the host plugin serves.
        vec![importer, implements_marker_exporter("plug")],
        vec![marker_entry(Some("plug"))],
    );
    assert!(
        plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "a declared `hostInterfaces` label must beat a component sharing its \
             name, or the component silently captures host traffic"
    );
}

/// Direction 2: the label is NOT declared under `hostInterfaces`, so there
/// is no host claim on it and the sibling it names still wins. The
/// precedence rule must not swallow ordinary guest-to-guest routing.
#[tokio::test]
async fn an_undeclared_label_still_routes_to_its_component() {
    let importer = labelled_marker_importer("importer", &["exporter-a"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        vec![
            importer,
            implements_marker_exporter("exporter-a"),
            implements_marker_exporter("exporter-b"),
        ],
        // Declared under a DIFFERENT name, so `exporter-a` is unclaimed.
        vec![marker_entry(Some("plug"))],
    );
    assert!(
        !plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "a label the host never declared must still route to the sibling \
             component it names"
    );
}

/// The precedence rule keys on the declared label, not merely on a label
/// being present: an unnamed `hostInterfaces` entry claims nothing, so a
/// labelled import still resolves in-workload.
#[tokio::test]
async fn an_unnamed_host_entry_claims_no_label() {
    let importer = labelled_marker_importer("importer", &["exporter-a"]);
    let importer_id = importer.id().to_string();
    let mut workload = served_within_workload_fixture(
        vec![
            importer,
            implements_marker_exporter("exporter-a"),
            implements_marker_exporter("exporter-b"),
        ],
        vec![marker_entry(None)],
    );
    assert!(
        !plugin_bound_ids(&mut workload).await.contains(&importer_id),
        "an unnamed host entry must not claim a label"
    );
}

/// The two predicates that gate the shadow warning. The last assertion is
/// the one that matters: an exporter imports nothing, so a workload that
/// merely contains a namesake exporter and no importer must not warn.
/// Exporting a host-served interface is ordinary, and is how the host
/// reaches into a workload at all.
#[test]
fn the_shadow_warning_gates_on_an_actual_capture() {
    let entry = marker_entry(Some("plug"));

    // A namesake that exports what the entry names is a candidate shadow.
    assert!(exports_any_of(
        &entry,
        &implements_marker_exporter("plug").world()
    ));
    // One that only imports it is not.
    assert!(!exports_any_of(
        &entry,
        &labelled_marker_importer("importer", &["plug"]).world()
    ));

    // The label has to be the one actually imported.
    assert!(imports_under_label(
        &entry,
        &labelled_marker_importer("importer", &["plug"]).world(),
        "plug"
    ));
    assert!(!imports_under_label(
        &entry,
        &labelled_marker_importer("importer", &["other"]).world(),
        "plug"
    ));
    // Nothing imports it here, so there is no capture to report.
    assert!(!imports_under_label(
        &entry,
        &implements_marker_exporter("plug").world(),
        "plug"
    ));
}

/// A component linked to a sibling's async export registers a closure in
/// its linker that reaches back to the component map, which holds that
/// linker. The map must still be freed once the workload drops.
#[tokio::test]
async fn a_linked_async_export_does_not_keep_the_components_alive() -> anyhow::Result<()> {
    let engine = crate::engine::Engine::builder().build()?;
    let engine = engine.inner();
    let component = |id: &str, wat: &str| -> anyhow::Result<WorkloadComponent> {
        Ok(WorkloadComponent::new(
            "linked-cycle",
            "linked-cycle",
            "test",
            id,
            Component::new(engine, wat::parse_str(wat)?)?,
            Linker::new(engine),
            Vec::new(),
            LocalResources::default(),
            Arc::default(),
            InstancePolicy::Ephemeral,
        ))
    };
    let exporter = component(
        "exporter",
        r#"(component
                (core module $m
                    (func (export "f") (result i32) (i32.const 0))
                    (func (export "cb") (param i32 i32 i32) (result i32) (i32.const 0)))
                (core instance $i (instantiate $m))
                (type $f (func async))
                (func $f (type $f)
                    (canon lift (core func $i "f") async (callback (core func $i "cb"))))
                (instance $inst (export "f" (func $f)))
                (export "test:probe/linked@0.1.0" (instance $inst))
            )"#,
    )?;
    let importer = component(
        "importer",
        r#"(component
                (import "test:probe/linked@0.1.0" (instance (export "f" (func async))))
            )"#,
    )?;

    let resolved = UnresolvedWorkload::new(
        "linked-cycle",
        "linked-cycle",
        "test",
        None,
        [exporter, importer],
        vec![],
    )
    .resolve(
        None,
        &crate::plugin::PluginBindings::default(),
        &crate::host::HostRef::detached(),
        &crate::observability::Meters::default(),
    )
    .await?;
    let importer_links = resolved
        .components
        .read()
        .await
        .values()
        .find(|c| c.name() == "importer")
        .map(|c| c.linked_components.len());
    assert_eq!(importer_links, Some(1), "the importer should be linked");

    let components = Arc::downgrade(&resolved.components);
    drop(resolved);
    assert!(
        components.upgrade().is_none(),
        "the component map outlived its workload"
    );
    Ok(())
}

/// A p3 service that cannot be pre-instantiated fails the start rather than
/// reading as a workload without a service.
#[tokio::test]
async fn a_p3_service_that_cannot_pre_instantiate_fails_to_execute() -> anyhow::Result<()> {
    let engine = wasmtime::Engine::default();
    let service = WorkloadService::new(
        "p3-service-error",
        "p3-service-error",
        "test",
        Component::new(
            &engine,
            wat::parse_str(
                r#"(component
                        (import "wasi:probe/missing@0.3.0" (instance (export "f" (func))))
                    )"#,
            )?,
        )?,
        Linker::new(&engine),
        vec![],
        LocalResources::default(),
        0,
        Arc::default(),
    );
    assert!(service.metadata.targets_p3());
    let mut resolved = UnresolvedWorkload::new(
        "p3-service-error",
        "p3-service-error",
        "test",
        Some(service),
        [],
        vec![],
    )
    .resolve(
        None,
        &crate::plugin::PluginBindings::default(),
        &crate::host::HostRef::detached(),
        &crate::observability::Meters::default(),
    )
    .await?;

    let err = resolved
        .execute_service()
        .await
        .expect_err("a service that cannot pre-instantiate must fail");
    assert!(
        format!("{err:#}").contains("P3 service of workload 'test/p3-service-error'"),
        "the error should name the service, got: {err:#}"
    );
    Ok(())
}
