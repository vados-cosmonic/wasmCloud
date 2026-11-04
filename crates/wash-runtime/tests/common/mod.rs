//! Shared helpers for wash-runtime integration tests.

#![allow(dead_code)]
// Shared builder helpers unwrap/expect on constant fixtures in non-`#[test]`
// fns, which the clippy.toml in-tests allows don't cover. This module is
// included by many test crates, so keep the allow self-contained here rather
// than relying on every consumer to carry one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "wasi-tls")]
pub mod tls;

#[cfg(feature = "wasmcloud-postgres")]
pub mod postgres;

pub mod streaming;

use anyhow::{Context, Result};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::time::timeout;
use wash_runtime::{
    engine::Engine,
    host::{
        Host, HostApi, HostBuilder,
        http::{DevRouter, HostHandler, Ingress, Router},
    },
    types::{
        Component, LocalResources, Workload, WorkloadStartRequest, WorkloadStartResponse,
        WorkloadState,
    },
    wit::WitInterface,
};

#[cfg(all(
    feature = "host-component-plugins",
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
use wash_runtime::plugin::component_host::ComponentHostPlugin;
/// The types only the standard-plugin helpers use, gated once on the features those plugins need.
#[cfg(all(
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
use {
    std::path::Path,
    wash_runtime::host::http::{DynamicRouter, TlsConfig},
    wash_runtime::plugin::{
        wasi_blobstore::InMemoryBlobstore, wasi_config::DynamicConfig,
        wasi_keyvalue::InMemoryKeyValue, wasi_logging::TracingLogger,
        wasmcloud_secrets::WasmcloudSecrets,
    },
};

/// HTTP incoming-handler (0.2.2) with `host` config, plus optional
/// comma-separated `host-aliases`. (example :  "admin,user,customer")
pub fn http_incoming_handler_interface(host_header: &str, aliases: Option<&str>) -> WitInterface {
    let mut config = wash_runtime::wit::Config::new();
    config.insert("host".to_string(), host_header.to_string());
    if let Some(aliases) = aliases {
        config.insert("host-aliases".to_string(), aliases.to_string());
    }
    WitInterface {
        config,
        ..WitInterface::from("wasi:http/incoming-handler@0.2.2")
    }
}

fn wasi_blobstore_interface() -> WitInterface {
    WitInterface::from("wasi:blobstore/blobstore,container,types@0.2.0-draft")
}

fn wasi_keyvalue_interface() -> WitInterface {
    WitInterface::from("wasi:keyvalue/store,atomics@0.2.0-draft")
}

fn wasi_logging_interface() -> WitInterface {
    WitInterface::from("wasi:logging/logging@0.1.0-draft")
}

fn wasi_config_interface() -> WitInterface {
    WitInterface::from("wasi:config/store@0.2.0-rc.1")
}

/// Interfaces used by the `http-counter` component: HTTP, blobstore,
/// keyvalue, logging, config.
pub fn http_counter_host_interfaces(host_header: &str) -> Vec<WitInterface> {
    http_counter_host_interfaces_with_aliases(host_header, None)
}

/// Same as `http_counter_host_interfaces` but with optional host aliases are
/// passed through to the HTTP interface's `host-aliases` config entry.
pub fn http_counter_host_interfaces_with_aliases(
    host_header: &str,
    aliases: Option<&str>,
) -> Vec<WitInterface> {
    vec![
        http_incoming_handler_interface(host_header, aliases),
        wasi_blobstore_interface(),
        wasi_keyvalue_interface(),
        wasi_logging_interface(),
        wasi_config_interface(),
    ]
}

/// Interfaces for HTTP-only components (e.g. `http-handler-p2`,
/// `http-handler-p3`): just the HTTP incoming-handler interface.
pub fn http_only_host_interfaces(host_header: &str) -> Vec<WitInterface> {
    vec![http_incoming_handler_interface(host_header, None)]
}

/// The bespoke `acme:kv/store@0.1.0` capability, provided by the `kv-plugin`
/// host component plugin.
#[cfg(feature = "host-component-plugins")]
pub fn acme_kv_interface() -> WitInterface {
    WitInterface::from("acme:kv/store@0.1.0")
}

/// Interfaces for the `kv-plugin-caller` workload: HTTP ingress plus the imported
/// `acme:kv/store` capability the host component plugin satisfies.
#[cfg(feature = "host-component-plugins")]
pub fn kv_plugin_caller_host_interfaces(host_header: &str) -> Vec<WitInterface> {
    kv_plugin_caller_host_interfaces_with_config(host_header, HashMap::new())
}

/// Like [`kv_plugin_caller_host_interfaces`] but carrying interface-level
/// config on the `acme:kv` entry — delivered to a lifecycle-exporting plugin's
/// `on-workload-bind`.
#[cfg(feature = "host-component-plugins")]
pub fn kv_plugin_caller_host_interfaces_with_config(
    host_header: &str,
    config: HashMap<String, String>,
) -> Vec<WitInterface> {
    vec![
        http_incoming_handler_interface(host_header, None),
        WitInterface {
            config: config.into(),
            ..acme_kv_interface()
        },
    ]
}

/// Interfaces for the `kv-plugin-implements-caller` workload: HTTP ingress plus
/// three `acme:kv` entries — the plain one and one per `(implements ..)` label
/// the component imports. One entry per binding, which is what the operator's
/// declaration is matched against.
#[cfg(feature = "host-component-plugins")]
pub fn kv_plugin_implements_caller_host_interfaces(host_header: &str) -> Vec<WitInterface> {
    vec![
        http_incoming_handler_interface(host_header, None),
        acme_kv_interface(),
        WitInterface {
            name: Some("tenant-a".to_string()),
            ..acme_kv_interface()
        },
        WitInterface {
            name: Some("tenant-b".to_string()),
            ..acme_kv_interface()
        },
    ]
}

/// The `wasmcloud:secrets` capability (store + reveal), served by the native
/// `wasmcloud-secrets` plugin.
pub fn secrets_interface(config: HashMap<String, String>) -> WitInterface {
    WitInterface {
        config: config.into(),
        ..WitInterface::from("wasmcloud:secrets/store,reveal@2.1.0")
    }
}

/// Interfaces for the `secrets-caller` workload: HTTP ingress plus the imported
/// `wasmcloud:secrets` capability, carrying the credentials as interface config —
/// which the secrets plugin captures in its `on-workload-bind`.
pub fn secrets_caller_host_interfaces_with_config(
    host_header: &str,
    config: HashMap<String, String>,
) -> Vec<WitInterface> {
    vec![
        http_incoming_handler_interface(host_header, None),
        secrets_interface(config),
    ]
}

/// Interfaces for P3 HTTP + blobstore components.
pub fn http_blobstore_host_interfaces(host_header: &str) -> Vec<WitInterface> {
    vec![
        http_incoming_handler_interface(host_header, None),
        wasi_blobstore_interface(),
    ]
}

pub fn component_workload_request(
    component_name: &str,
    workload_name: &str,
    wasm: &'static [u8],
    local_resources: LocalResources,
    host_interfaces: Vec<WitInterface>,
) -> WorkloadStartRequest {
    WorkloadStartRequest {
        workload_id: uuid::Uuid::new_v4().to_string(),
        workload: Workload {
            namespace: "test".to_string(),
            name: workload_name.to_string(),
            annotations: HashMap::new(),
            service: None,
            components: vec![Component {
                name: component_name.to_string(),
                digest: None,
                bytes: bytes::Bytes::from_static(wasm),
                local_resources,
                pool_size: 1,
                max_invocations: 100,
                max_concurrency: 1,
                ..Default::default()
            }],
            host_interfaces,
            volumes: vec![],
        },
    }
}

pub fn default_counter_resources() -> LocalResources {
    // http-counter calls example.com — encode that exact host in the
    // policy so the deny-all default doesn't block it AND the test
    // documents which upstream the fixture talks to.
    LocalResources {
        memory_limit_mb: 256,
        cpu_limit: 1,
        config: HashMap::new(),
        environment: HashMap::new(),
        volume_mounts: vec![],
        allowed_hosts: vec!["example.com".parse().unwrap()].into(),
        allowed_ip_name_lookups: Default::default(),
        allowed_host_loopback_ports: Default::default(),
    }
}

/// Attach the standard suite of plugins used by http-counter tests:
/// in-memory blobstore + keyvalue, tracing logger, dynamic config.
#[cfg(all(
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
fn with_standard_plugins(
    builder: wash_runtime::host::HostBuilder,
) -> Result<wash_runtime::host::HostBuilder> {
    builder
        .with_plugin(Arc::new(InMemoryBlobstore::new(None)))?
        .with_plugin(Arc::new(InMemoryKeyValue::new()))?
        .with_plugin(Arc::new(TracingLogger::default()))?
        .with_plugin(Arc::new(DynamicConfig::default()))?
        .with_plugin(Arc::new(WasmcloudSecrets::new()))
}

/// Start a host whose HTTP traffic goes through `http`,
/// with whatever plugins `plugins` attaches to the builder.
///
/// Every `start_host*` helper in this module is this function
/// with a particular ingress and plugin set.
/// A test that needs its own plugin set calls it,
/// or [`start_dev_router_host`], directly instead of building a host by hand.
pub async fn start_host(
    http: Arc<dyn HostHandler>,
    plugins: impl FnOnce(HostBuilder) -> Result<HostBuilder>,
) -> Result<Arc<Host>> {
    let builder = HostBuilder::new()
        .with_engine(Engine::builder().build()?)
        .with_http_handler(http);
    plugins(builder)?
        .build()?
        .start()
        .await
        .context("Failed to start host")
}

/// [`start_host`] serving HTTP through `router` on `addr`.
/// Returns the bound address alongside the started host.
pub async fn start_host_on(
    router: impl Router,
    addr: &str,
    plugins: impl FnOnce(HostBuilder) -> Result<HostBuilder>,
) -> Result<(SocketAddr, Arc<Host>)> {
    let ingress = Ingress::new(router, addr.parse()?).await?;
    let bound_addr = ingress.addr();
    Ok((bound_addr, start_host(Arc::new(ingress), plugins).await?))
}

/// [`start_host_on`] with a `DevRouter` on an ephemeral localhost port:
/// the shape most single-workload tests want.
pub async fn start_dev_router_host(
    plugins: impl FnOnce(HostBuilder) -> Result<HostBuilder>,
) -> Result<(SocketAddr, Arc<Host>)> {
    start_host_on(DevRouter::default(), "127.0.0.1:0", plugins).await
}

/// Start a host with a "DevRouter" backed HTTP server and the standard plugin
/// set. Returns the bound address and a started `HostApi` ref.
#[cfg(all(
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_dev_router(addr: &str) -> Result<(SocketAddr, impl HostApi)> {
    start_host_on(DevRouter::default(), addr, with_standard_plugins).await
}

/// Start a host with a "DynamicRouter" backed HTTP server and the standard
/// plugin set.
#[cfg(all(
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_dynamic_router(addr: &str) -> Result<(SocketAddr, impl HostApi)> {
    start_host_on(DynamicRouter::default(), addr, with_standard_plugins).await
}

/// Start a host with a TLS-enabled `DevRouter`-backed HTTP server and the
/// standard plugin set. Certificate and key are read from disk at the given
/// paths.
#[cfg(all(
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_tls(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(SocketAddr, impl HostApi)> {
    let ingress = Ingress::new_with_tls(
        DevRouter::default(),
        "127.0.0.1:0".parse()?,
        TlsConfig::new(cert_path, key_path),
    )
    .await?;
    let bound_addr = ingress.addr();
    Ok((
        bound_addr,
        start_host(Arc::new(ingress), with_standard_plugins).await?,
    ))
}

/// The same host as [`start_host_with_dev_router`].
/// The engine runs p2 and p3 components alike,
/// so p3 tests need nothing extra; the name is kept for those callers.
#[cfg(all(
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_p3_http_handler(addr: &str) -> Result<(SocketAddr, impl HostApi)> {
    start_host_with_dev_router(addr).await
}

/// Start a p3 host with the standard plugin set plus a [`ComponentHostPlugin`]
/// built from `plugin_wasm`, routed by `router`, with `max_restarts` overriding
/// the plugin's supervision budget when given. The named wrappers below cover
/// the common shapes.
#[cfg(all(
    feature = "host-component-plugins",
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
async fn start_host_with_component_plugin_router(
    addr: &str,
    router: impl wash_runtime::host::http::Router,
    plugin_id: &'static str,
    plugin_wasm: &'static [u8],
    max_restarts: Option<u32>,
    plugin_bindings: wash_runtime::plugin::PluginBindings,
) -> Result<(SocketAddr, impl HostApi)> {
    let engine = Engine::builder().build()?;
    let ingress = Ingress::new(router, addr.parse()?).await?;
    let bound_addr = ingress.addr();
    let builder = with_standard_plugins(
        HostBuilder::new()
            .with_engine(engine.clone())
            .with_http_handler(Arc::new(ingress)),
    )?;
    let native_plugins = builder.native_plugins();
    let host_ref = builder.host_ref();
    let mut plugin = ComponentHostPlugin::builder()
        .id(plugin_id)
        .wasm(plugin_wasm)
        .engine(engine.clone())
        .native_plugins(native_plugins)
        .maybe_host_ref(Some(host_ref))
        .build()
        .await
        .context("failed to build host component plugin")?;
    if let Some(max_restarts) = max_restarts {
        plugin = plugin.with_max_restarts(max_restarts);
    }
    let host = builder
        .with_plugin(Arc::new(plugin))?
        .with_plugin_bindings(plugin_bindings)
        .build()?;
    let host = host.start().await.context("Failed to start host")?;
    Ok((bound_addr, host))
}

/// Start a p3 host with a component plugin whose `host.plugins` entry declares
/// `bindings`, the way an operator turns label routing on for one.
#[cfg(feature = "host-component-plugins")]
pub async fn start_host_with_component_plugin_bindings(
    addr: &str,
    plugin_id: &'static str,
    plugin_wasm: &'static [u8],
    plugin_bindings: wash_runtime::plugin::PluginBindings,
) -> Result<(SocketAddr, impl HostApi)> {
    start_host_with_component_plugin_router(
        addr,
        DevRouter::default(),
        plugin_id,
        plugin_wasm,
        None,
        plugin_bindings,
    )
    .await
}

/// Start a p3 host with several component plugins, `(id, wasm)` each, and the
/// operator's `plugin_bindings` for them.
#[cfg(all(
    feature = "host-component-plugins",
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_component_plugins(
    addr: &str,
    plugins: &[(&'static str, &'static [u8])],
    plugin_bindings: wash_runtime::plugin::PluginBindings,
) -> Result<(SocketAddr, impl HostApi)> {
    let engine = Engine::builder().build()?;
    let ingress = Ingress::new(DevRouter::default(), addr.parse()?).await?;
    let bound_addr = ingress.addr();
    let mut builder = with_standard_plugins(
        HostBuilder::new()
            .with_engine(engine.clone())
            .with_http_handler(Arc::new(ingress)),
    )?;
    let native_plugins = builder.native_plugins();
    let host_ref = builder.host_ref();
    for (id, wasm) in plugins {
        let plugin = ComponentHostPlugin::builder()
            .id(id)
            .wasm(wasm)
            .engine(engine.clone())
            .native_plugins(native_plugins.clone())
            .maybe_host_ref(Some(host_ref.clone()))
            .build()
            .await
            .with_context(|| format!("failed to build host component plugin '{id}'"))?;
        builder = builder.with_plugin(Arc::new(plugin))?;
    }
    let host = builder.with_plugin_bindings(plugin_bindings).build()?;
    let host = host.start().await.context("Failed to start host")?;
    Ok((bound_addr, host))
}

/// Start a p3 host with the standard plugin set plus a [`ComponentHostPlugin`]
/// built from `plugin_wasm` (a host component plugin exporting a capability).
/// Used to test workloads that import a component-provided host capability.
#[cfg(all(
    feature = "host-component-plugins",
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_component_plugin(
    addr: &str,
    plugin_id: &'static str,
    plugin_wasm: &'static [u8],
) -> Result<(SocketAddr, impl HostApi)> {
    start_host_with_component_plugin_router(
        addr,
        DevRouter::default(),
        plugin_id,
        plugin_wasm,
        None,
        wash_runtime::plugin::PluginBindings::new(),
    )
    .await
}

/// Like [`start_host_with_component_plugin`] but with a `DynamicRouter` that
/// routes by `Host` header — so distinct workloads are reachable individually
/// (the `DevRouter` sends every request to the last-resolved workload). Needed
/// to test per-caller behavior across genuinely separate workloads.
#[cfg(all(
    feature = "host-component-plugins",
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_component_plugin_by_host(
    addr: &str,
    plugin_id: &'static str,
    plugin_wasm: &'static [u8],
) -> Result<(SocketAddr, impl HostApi)> {
    start_host_with_component_plugin_router(
        addr,
        DynamicRouter::default(),
        plugin_id,
        plugin_wasm,
        None,
        wash_runtime::plugin::PluginBindings::new(),
    )
    .await
}

/// Like [`start_host_with_component_plugin`] but overriding the plugin's
/// supervision restart budget — for tests that exhaust it.
#[cfg(all(
    feature = "host-component-plugins",
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_component_plugin_max_restarts(
    addr: &str,
    plugin_id: &'static str,
    plugin_wasm: &'static [u8],
    max_restarts: u32,
) -> Result<(SocketAddr, impl HostApi)> {
    start_host_with_component_plugin_router(
        addr,
        DevRouter::default(),
        plugin_id,
        plugin_wasm,
        Some(max_restarts),
        wash_runtime::plugin::PluginBindings::new(),
    )
    .await
}

/// Like [`start_host_with_p3_http_handler`] but also returns the [`Ingress`], so a test can
/// drive host-side ingress hooks directly (e.g. deliver a message to a trigger service's
/// messaging handler via `deliver_trigger_service_message`).
#[cfg(all(
    feature = "wasi-blobstore",
    feature = "wasi-config",
    feature = "wasi-keyvalue",
    feature = "wasi-logging"
))]
pub async fn start_host_with_p3_handler(
    addr: &str,
) -> Result<(SocketAddr, impl HostApi, Arc<Ingress<DevRouter>>)> {
    let ingress = Arc::new(Ingress::new(DevRouter::default(), addr.parse()?).await?);
    let host = start_host(ingress.clone(), with_standard_plugins).await?;
    Ok((ingress.addr(), host, ingress))
}

/// Extract the numeric value of `"name":N` from a flat JSON body without
/// pulling in a JSON dependency. Panics (failing the test) if the field is
/// missing or non-numeric.
pub fn json_u64_field(body: &str, name: &str) -> u64 {
    let key = format!("\"{name}\":");
    let start = body.find(&key).expect("field present in body") + key.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("numeric field")
}

/// GET `http://{addr}{path}` with the given `HOST` header and a 15s timeout.
/// The one request path every `get_*` helper below goes through.
async fn get(
    client: &reqwest::Client,
    addr: SocketAddr,
    host_header: &str,
    path: &str,
) -> Result<reqwest::Response> {
    timeout(
        Duration::from_secs(15),
        client
            .get(format!("http://{addr}{path}"))
            .header("HOST", host_header)
            .send(),
    )
    .await
    .with_context(|| format!("request to {host_header}{path} timed out"))?
    .with_context(|| format!("request to {host_header}{path} failed"))
}

/// GET `http://{addr}/` with the given `HOST` header; just the status.
pub async fn get_status(
    client: &reqwest::Client,
    addr: SocketAddr,
    host_header: &str,
) -> Result<reqwest::StatusCode> {
    Ok(get(client, addr, host_header, "/").await?.status())
}

/// Like `get_status` but also returns the response body text.
pub async fn get_status_and_body(
    client: &reqwest::Client,
    addr: SocketAddr,
    host_header: &str,
) -> Result<(reqwest::StatusCode, String)> {
    req(client, &addr, host_header, "/").await
}

/// Like `get_status_and_body`, but against an arbitrary `path` instead of
/// always `/` — for tests whose routing is on the path (e.g. `/get?key=…`),
/// not just the `HOST` header.
pub async fn req(
    client: &reqwest::Client,
    addr: &SocketAddr,
    host: &str,
    path: &str,
) -> Result<(reqwest::StatusCode, String)> {
    let response = get(client, *addr, host, path).await?;
    let status = response.status();
    let body = response.text().await?;
    Ok((status, body))
}

/// GET `/` on `host_header`, fail unless the response is a success,
/// and return its body — the check most HTTP-driven tests end with.
pub async fn get_ok_body(addr: SocketAddr, host_header: &str) -> Result<String> {
    let (status, body) = get_status_and_body(&reqwest::Client::new(), addr, host_header).await?;
    anyhow::ensure!(status.is_success(), "expected 200, got {status}: {body}");
    Ok(body)
}

/// Start `request` on `host` and fail unless the workload reached `Running`.
///
/// `workload_start` returns `Ok` even when resolution fails
/// (the error is in the returned status),
/// so a bare `?` on it does not prove the workload started.
pub async fn start_running_workload(
    host: &impl HostApi,
    request: WorkloadStartRequest,
) -> Result<WorkloadStartResponse> {
    let response = host
        .workload_start(request)
        .await
        .context("workload_start call failed")?;
    anyhow::ensure!(
        response.workload_status.workload_state == WorkloadState::Running,
        "workload did not reach Running ({:?}): {}",
        response.workload_status.workload_state,
        response.workload_status.message
    );
    Ok(response)
}
