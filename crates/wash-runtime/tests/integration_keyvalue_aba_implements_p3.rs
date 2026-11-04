#![cfg(feature = "wasm_component_model_implements")]
//! End-to-end ABA test for async `wasmcloud:keyvalue` compare-and-swap through a
//! **real P3 guest**, routed via `(implements ..)`.
//!
//! The `keyvalue-implements-p3` fixture opens a bucket through the labeled `kv`
//! (`store`) import, then `set k=A`, `current(k) -> v1`, `set k=B`, `set k=A`,
//! and asserts (via `store`/`cas`/`atomics`/`batch`, all on that one bucket):
//!
//! - `swap(k, C, require_version=v1)` is `stale` — the value returned to
//!   identical bytes (A → B → A), so a content-hash version would wrongly
//!   succeed; the backend's monotonic version makes it stale (ABA detected).
//! - a swap pinned to the *current* version succeeds — so the stale result is a
//!   real precondition miss, not an always-stale backend.
//! - a swap with empty `cas-options` is rejected with `invalid-argument`.
//! - `atomics.increment` and `batch` get/set/delete-many round-trip correctly.
//!
//! The guest answers `ok` only when all hold.
//!
//! This exercises the full async/concurrent host ABI through a guest — the
//! `store.open` label routing, the shared `types.bucket` resource, and the
//! standalone `cas` `current`/`swap` host methods calling the backend's atomic
//! compare-and-set — against the in-memory backend, so it needs no Docker.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use anyhow::{Context, Result};

use wash_runtime::{
    plugin::wasi_keyvalue::{InMemoryProvider, MultiplexedAsyncKeyValue},
    types::LocalResources,
    wit::WitInterface,
};

mod common;
use common::component_workload_request;
use common::get_ok_body;
use common::http_incoming_handler_interface;
use common::start_dev_router_host;
use common::start_running_workload;

const KEYVALUE_IMPLEMENTS_P3_WASM: &[u8] = include_bytes!("wasm/keyvalue_implements_p3.wasm");

/// The labeled `store` import (`kv`), routed to an in-memory backend. `cas` is
/// bound standalone and needs no declaration — it operates on the bucket the
/// guest opens through this label.
fn kv_store_iface(name: &str) -> WitInterface {
    WitInterface {
        namespace: "wasmcloud".to_string(),
        package: "keyvalue".to_string(),
        interfaces: ["store".to_string()].into_iter().collect(),
        version: Some(semver::Version::parse("0.2.0").unwrap()),
        config: wash_runtime::wit::Config::from_iter([(
            "backend".to_string(),
            "in-memory".to_string(),
        )]),
        name: Some(name.to_string()),
    }
}

#[tokio::test]
async fn p3_guest_cas_swap_detects_aba() -> Result<()> {
    let (addr, host) = start_dev_router_host(|builder| {
        builder.with_plugin(Arc::new(
            MultiplexedAsyncKeyValue::new().with_provider(Arc::new(InMemoryProvider)),
        ))
    })
    .await?;

    let host_interfaces = vec![
        http_incoming_handler_interface("kv-impl", None),
        kv_store_iface("kv"),
    ];

    let req = component_workload_request(
        "keyvalue-implements-p3.wasm",
        "keyvalue-implements-p3",
        KEYVALUE_IMPLEMENTS_P3_WASM,
        LocalResources::default(),
        host_interfaces,
    );

    // Binding runs `add_to_linker` for the named `store` plus standalone
    // `types`/`cas`. Assert the workload actually reached Running (a labeled
    // store + cas guest is exactly the case the WIT restructure enables).
    start_running_workload(&host, req)
        .await
        .context("workload should resolve (labeled store + standalone cas)")?;

    let body = get_ok_body(addr, "kv-impl").await?;
    assert_eq!(
        body, "ok",
        "guest must observe: ABA swap stale, current-version swap succeeds, empty cas-options rejected"
    );

    Ok(())
}
