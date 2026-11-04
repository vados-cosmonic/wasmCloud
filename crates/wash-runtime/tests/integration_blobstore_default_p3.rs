#![cfg(feature = "wasm_component_model_implements")]
//! A **plain** (unlabeled) async `wasmcloud:blobstore@0.1.0` import, served by a
//! default backend through a **real P3 guest** — no `(implements ..)` label.
//!
//! The `blobstore-default-p3` fixture imports `wasmcloud:blobstore/blobstore`
//! plainly. On each request it creates a container, streams an object body in via
//! `write-data(name, stream<u8>)`, reads it back via `get-data(..) -> stream<u8>`,
//! and returns the bytes.
//!
//! The host binds a single [`MultiplexedAsyncBlobstore`] with the in-memory
//! provider and declares an **unnamed** blobstore interface (the default route).
//! A 200 whose body echoes the written bytes proves that a component gets a
//! working backend from a plain import — no label needed — exercising the
//! standard (non-`(implements)`) binding to the workload's default backend.
//!
//! Unlike the NATS-backed blobstore integration tests, this uses the in-memory
//! backend, so it needs no Docker and is not `#[ignore]`d.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use anyhow::Result;

use wash_runtime::{
    plugin::wasi_blobstore::{InMemoryProvider, MultiplexedAsyncBlobstore},
    types::LocalResources,
    wit::WitInterface,
};

mod common;
use common::{
    component_workload_request, get_ok_body, http_incoming_handler_interface,
    start_dev_router_host, start_running_workload,
};

const BLOBSTORE_DEFAULT_P3_WASM: &[u8] = include_bytes!("wasm/blobstore_default_p3.wasm");

#[tokio::test]
async fn p3_guest_plain_blobstore_uses_default_backend() -> Result<()> {
    // --- host with the async multiplexed blobstore plugin + in-memory provider ---
    let (addr, host) = start_dev_router_host(|builder| {
        builder.with_plugin(Arc::new(
            MultiplexedAsyncBlobstore::new().with_provider(Arc::new(InMemoryProvider)),
        ))
    })
    .await?;

    // The guest imports blobstore *plainly* (no label). An UNNAMED interface,
    // with no backend override, is the workload's default route: the plugin
    // binds it via the standard host interface, falling back to in-memory.
    let host_interfaces = vec![
        http_incoming_handler_interface("blob-default", None),
        WitInterface::from("wasmcloud:blobstore/blobstore,container,types@0.1.0"),
    ];

    // Binding runs the *standard* `blobstore::add_to_linker` (not the named one)
    // for the plain import; reaching Running proves it resolved to a default backend.
    start_running_workload(
        &host,
        component_workload_request(
            "blobstore-default-p3.wasm",
            "blobstore-default-p3",
            BLOBSTORE_DEFAULT_P3_WASM,
            LocalResources::default(),
            host_interfaces,
        ),
    )
    .await?;

    // Drive the guest: create a container, stream an object in, read it back, and
    // echo the bytes. A matching body proves the plain import reached a working
    // default backend through the stream ABI — with no label.
    let body = get_ok_body(addr, "blob-default").await?;
    assert_eq!(
        body, "woof from a plain p3 guest",
        "the guest must round-trip the object body through the default (unlabeled) backend"
    );

    Ok(())
}
