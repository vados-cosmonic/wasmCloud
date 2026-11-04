#![cfg(feature = "wasm_component_model_implements")]
//! A **plain** (unlabeled) async `wasmcloud:keyvalue@0.2.0` `store` import, served
//! by a default backend through a **real P3 guest** — no `(implements ..)` label.
//!
//! The `keyvalue-default-p3` fixture opens a bucket via `wasmcloud:keyvalue/store`
//! plainly. On each request it sets a key, increments a counter through the
//! standalone `atomics` import, reads the value back, and returns it.
//!
//! The host binds a single [`MultiplexedAsyncKeyValue`] with the in-memory
//! provider and declares an **unnamed** keyvalue interface (the default route). A
//! 200 whose body echoes the written value proves that a component gets a working
//! backend from a plain `store` import — no label needed — exercising the standard
//! (non-`(implements)`) binding to the workload's default backend.
//!
//! Like the blobstore-default counterpart, this uses the in-memory backend, so it
//! needs no external services and is not `#[ignore]`d.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use anyhow::Result;

use wash_runtime::{
    plugin::wasi_keyvalue::{InMemoryProvider, MultiplexedAsyncKeyValue},
    types::LocalResources,
    wit::WitInterface,
};

mod common;
use common::{
    component_workload_request, get_ok_body, http_incoming_handler_interface,
    start_dev_router_host, start_running_workload,
};

const KEYVALUE_DEFAULT_P3_WASM: &[u8] = include_bytes!("wasm/keyvalue_default_p3.wasm");

#[tokio::test]
async fn p3_guest_plain_keyvalue_uses_default_backend() -> Result<()> {
    // --- host with the async multiplexed keyvalue plugin + in-memory provider ---
    let (addr, host) = start_dev_router_host(|builder| {
        builder.with_plugin(Arc::new(
            MultiplexedAsyncKeyValue::new().with_provider(Arc::new(InMemoryProvider)),
        ))
    })
    .await?;

    // The guest imports `store` *plainly* (no label). An UNNAMED interface,
    // with no backend override, is the workload's default route: the plugin
    // binds it via the standard host interface, falling back to in-memory.
    let host_interfaces = vec![
        http_incoming_handler_interface("kv-default", None),
        WitInterface::from("wasmcloud:keyvalue/store@0.2.0"),
    ];

    // Binding runs the *standard* `store::add_to_linker` (not the named one)
    // for the plain import; reaching Running proves it resolved to a default backend.
    start_running_workload(
        &host,
        component_workload_request(
            "keyvalue-default-p3.wasm",
            "keyvalue-default-p3",
            KEYVALUE_DEFAULT_P3_WASM,
            LocalResources::default(),
            host_interfaces,
        ),
    )
    .await?;

    // Drive the guest: open a bucket, set a key, increment a counter, read the
    // value back, and echo it. A matching body proves the plain import reached a
    // working default backend — with no label.
    let body = get_ok_body(addr, "kv-default").await?;
    assert_eq!(
        body, "woof from a plain p3 keyvalue guest",
        "the guest must round-trip the value through the default (unlabeled) backend"
    );

    Ok(())
}
