#![cfg(feature = "wasm_component_model_implements")]
//! End-to-end `(implements ..)` routing of the async, native-stream
//! `wasmcloud:blobstore@0.1.0` through a **real P3 guest**, backed by NATS.
//!
//! The `blobstore-implements-p3` fixture imports the async blobstore under the
//! labels `store-a` (the `blobstore` interface) and `objects-a` (the `container`
//! interface). On each request it creates a container, streams an object body in
//! via `write-data(name, stream<u8>)`, reads it back out via
//! `get-data(..) -> stream<u8>`, and returns the bytes.
//!
//! The host binds a single [`MultiplexedAsyncBlobstore`] with a NATS provider and
//! declares the two named interfaces routed to the Dockerized NATS object store.
//! A 200 whose body echoes the written bytes proves the concurrent/stream host
//! ABI works end to end through a guest — the producer (`get-data`), the consumer
//! (`write-data`), and the implements id threading — against a real backend.
//!
//! Requires Docker (NATS JetStream); marked `#[ignore]`, so it runs only under
//! `cargo test --include-ignored`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio::time::timeout;

use wash_runtime::{
    engine::Engine,
    host::{
        HostApi, HostBuilder,
        http::{DevRouter, Ingress},
    },
    plugin::{
        PluginBindingSet, PluginBindings,
        wasi_blobstore::{MultiplexedAsyncBlobstore, NatsBlobProvider},
    },
    types::{Component, LocalResources, Workload, WorkloadStartRequest, WorkloadState},
    wit::WitInterface,
};

mod common;
use common::component_workload_request;
use common::get_ok_body;
use common::http_incoming_handler_interface;
use common::start_dev_router_host;
use common::start_running_workload;

const BLOBSTORE_IMPLEMENTS_P3_WASM: &[u8] = include_bytes!("wasm/blobstore_implements_p3.wasm");

/// A named async `wasmcloud:blobstore` interface routed to the NATS backend.
/// `name` is the `(implements ..)` label the guest imports under; `iface` is the
/// blobstore sub-interface (`blobstore` or `container`). The server's `url` is
/// the operator's, declared on the host.
fn nats_blob_iface(name: &str, iface: &str) -> WitInterface {
    WitInterface {
        namespace: "wasmcloud".to_string(),
        package: "blobstore".to_string(),
        interfaces: [iface.to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.1.0").unwrap()),
        config: wash_runtime::wit::Config::from_iter([("backend".to_string(), "nats".to_string())]),
        name: Some(name.to_string()),
    }
}

#[tokio::test]
#[ignore = "requires Docker (NATS JetStream); run with `cargo test --include-ignored`"]
async fn p3_guest_streams_blobstore_through_nats() -> Result<()> {
    // --- NATS container (JetStream enabled for the object store) ---
    let nats = GenericImage::new("nats", "2.12.8-alpine")
        .with_exposed_port(4222.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js"])
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("failed to start nats: {e}"))?;
    let nats_port = nats.get_host_port_ipv4(4222).await?;
    let nats_url = format!("nats://127.0.0.1:{nats_port}");

    // --- host with the async multiplexed blobstore plugin + NATS provider ---
    let (addr, host) = start_dev_router_host(|builder| {
        Ok(builder
            .with_plugin(Arc::new(
                MultiplexedAsyncBlobstore::new()
                    .with_provider(Arc::new(NatsBlobProvider::default())),
            ))?
            .with_plugin_bindings(PluginBindings::new().with_plugin(
                PluginBindingSet::new("wasmcloud-blobstore-multiplexed").with_base(
                    wash_runtime::wit::Config::from_iter([("url".to_string(), nats_url)]),
                ),
            )))
    })
    .await?;

    // The guest's `store-a` (blobstore) and `objects-a` (container) implements
    // imports both route to the same NATS server (pooled by url).
    let host_interfaces = vec![
        http_incoming_handler_interface("blob-impl", None),
        nats_blob_iface("store-a", "blobstore"),
        nats_blob_iface("objects-a", "container"),
    ];

    let req = component_workload_request(
        "blobstore-implements-p3.wasm",
        "blobstore-implements-p3",
        BLOBSTORE_IMPLEMENTS_P3_WASM,
        LocalResources::default(),
        host_interfaces,
    );

    // Binding runs the named-imports `add_to_linker` for both `store-a` and
    // `objects-a`. `workload_start` returns Ok even on resolution failure (it
    // encodes the error in the status), so assert it actually reached Running.
    start_running_workload(&host, req)
        .await
        .context("workload should resolve")?;

    // Drive the guest: it creates a container, streams an object in, reads it
    // back, and echoes the bytes. A matching body proves the stream ABI flowed
    // both directions through the real NATS object store.
    let body = get_ok_body(addr, "blob-impl").await?;
    assert_eq!(
        body, "meow from a real p3 guest",
        "the guest must round-trip the object body through the NATS-backed stream ABI"
    );

    Ok(())
}
