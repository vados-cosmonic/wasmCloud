//! The one way the runtime instantiates a linked component.
//!
//! # Why this exists
//!
//! Two things have to happen around every instantiation, and neither can be
//! left to the site that performs it:
//!
//! - Under [`GuestMemoryMode::Growth`] the guest memory budget leaves the
//!   memory an instance is born with uncharged and charges only what it grows
//!   by. wasmtime reports both to the store's [`wasmtime::ResourceLimiter`]
//!   as growth from zero, so the limiter can only tell them apart by knowing
//!   *when* an instantiation is under way: the store is marked for exactly
//!   that interval, by a guard that unmarks it when dropped (see
//!   [`StoreMemoryLimiter::begin_instantiation`]).
//! - When the pooling allocator refuses an instantiation, the error wasmtime
//!   returns names a limit but not the pool, the workload, or the knob that
//!   moves it. [`pool_usage::explain_instantiate_failure`] adds all three
//!   and counts the refusal.
//!
//! A site that skipped either would still work, which is the problem: its
//! instances would have their declared initial memory charged to the
//! budget, so a host in `growth` mode would lose the density the mode exists
//! for, silently, and its pool refusals would come back as bare wasmtime
//! errors.
//!
//! [`Linked`] makes both steps part of instantiating. It wraps anything that
//! can be instantiated into a store (a raw [`InstancePre`], or one of the
//! typed `*Pre` views the WASI, HTTP and plugin bindings generate over it)
//! and offers exactly one way to do so, [`Linked::instantiate`], which
//! performs both steps. The wrapped value is reachable through
//! [`Linked::pre`] for everything else a pre is good for: its component, its
//! engine, building a typed view over it. What it is not reachable for is a
//! bare `instantiate_async`, and two checks hold that line:
//!
//! - The workspace's clippy `disallowed-methods` list names the method on
//!   the pre types wasmtime and its WASI crates define, and on `Linker`,
//!   which instantiates without a pre, so a call to one of those outside the
//!   [`LinkedPre`] impls fails `cargo clippy`.
//! - The typed pres the plugin bindings generate are not on that list: each
//!   is a `bindgen!` expansion whose own `instantiate_async` calls the raw
//!   one, so every bindings module allows the lint, and naming each generated
//!   type would tie the list to macro output. The test in this module covers
//!   them instead: it reads the runtime's source and fails on any
//!   `instantiate_async(` outside this module and the `instantiate_raw`
//!   bodies of [`LinkedPre`] impls. Test code is exempt.
//!
//! A new dispatcher that wants an instance has to go through here, or fail
//! one of the two.
//!
//! What this does not cover, by design: code outside the runtime that
//! instantiates a pre it obtained from a public `pre_instantiate` method
//! (an embedder, the baseline benches). Such instantiations are not
//! bracketed and charge initial memory under `growth`, the safe direction,
//! and are outside the runtime's own density claims.
//!
//! [`GuestMemoryMode::Growth`]: crate::engine::guest_memory::GuestMemoryMode::Growth
//! [`StoreMemoryLimiter::begin_instantiation`]: crate::engine::guest_memory::StoreMemoryLimiter::begin_instantiation
//! [`pool_usage::explain_instantiate_failure`]: crate::engine::pool_usage::explain_instantiate_failure

use std::future::Future;

use wasmtime::component::InstancePre;

use crate::engine::ctx::SharedCtx;

/// Something a store can be instantiated from: a raw [`InstancePre`] or a
/// typed view over one.
///
/// Implemented here for the pre types the runtime itself instantiates, and
/// beside the plugin bindings for theirs. An impl is the one place a pre
/// type's own `instantiate_async` is called; everything else goes through
/// [`Linked::instantiate`].
pub(crate) trait LinkedPre: Send + Sync {
    /// What instantiating yields: a bare [`wasmtime::component::Instance`],
    /// or the typed view the bindings generate.
    type Instance;

    /// The pre type's own instantiation, unbracketed. Not for callers: see
    /// [`Linked::instantiate`].
    fn instantiate_raw(
        &self,
        store: &mut wasmtime::Store<SharedCtx>,
    ) -> impl Future<Output = wasmtime::Result<Self::Instance>> + Send;
}

/// A linked component the runtime may instantiate, and the only way it does.
///
/// See the module docs for why. A pre is cheap to clone (it shares the
/// component and its linked imports), so a site that needs both forms keeps
/// this and reaches the raw one through [`Self::pre`].
#[derive(Clone)]
pub(crate) struct Linked<P>(P);

/// The raw [`InstancePre`] wrapped, the form most sites hold.
pub(crate) type LinkedInstancePre = Linked<InstancePre<SharedCtx>>;

impl<P: LinkedPre> Linked<P> {
    /// Wrap a pre. Cheap: nothing is linked or checked here.
    pub(crate) fn new(pre: P) -> Self {
        Self(pre)
    }

    /// The pre itself, for everything but instantiating it: its component,
    /// its engine, a typed view built over it.
    pub(crate) fn pre(&self) -> &P {
        &self.0
    }

    /// Instantiate into `store`.
    ///
    /// The store's guest memory limiter is told an instantiation is under way
    /// for as long as this runs, and no longer: a future dropped before it
    /// completes (a cancelled `select!`) unmarks the store with it. A pooling
    /// allocator refusal comes back explained and counted; any other error
    /// comes back as wasmtime returned it.
    pub(crate) async fn instantiate(
        &self,
        store: &mut wasmtime::Store<SharedCtx>,
    ) -> wasmtime::Result<P::Instance> {
        let instantiating = store.data_mut().memory_limiter.begin_instantiation();
        let instantiated = self.0.instantiate_raw(&mut *store).await;
        drop(instantiating);
        instantiated
            .map_err(|e| crate::engine::pool_usage::explain_instantiate_failure(store.data(), e))
    }
}

// The impls below are the one place the runtime calls a pre type's own
// `instantiate_async`; see the module docs and the workspace clippy.toml.

#[allow(clippy::disallowed_methods)]
impl LinkedPre for InstancePre<SharedCtx> {
    type Instance = wasmtime::component::Instance;

    async fn instantiate_raw(
        &self,
        store: &mut wasmtime::Store<SharedCtx>,
    ) -> wasmtime::Result<Self::Instance> {
        self.instantiate_async(store).await
    }
}

#[allow(clippy::disallowed_methods)]
impl LinkedPre for wasmtime_wasi::p2::bindings::CommandPre<SharedCtx> {
    type Instance = wasmtime_wasi::p2::bindings::Command;

    async fn instantiate_raw(
        &self,
        store: &mut wasmtime::Store<SharedCtx>,
    ) -> wasmtime::Result<Self::Instance> {
        self.instantiate_async(store).await
    }
}

#[allow(clippy::disallowed_methods)]
impl LinkedPre for wasmtime_wasi::p3::bindings::CommandPre<SharedCtx> {
    type Instance = wasmtime_wasi::p3::bindings::Command;

    async fn instantiate_raw(
        &self,
        store: &mut wasmtime::Store<SharedCtx>,
    ) -> wasmtime::Result<Self::Instance> {
        self.instantiate_async(store).await
    }
}

#[allow(clippy::disallowed_methods)]
impl LinkedPre for wasmtime_wasi_http::p2::bindings::ProxyPre<SharedCtx> {
    type Instance = wasmtime_wasi_http::p2::bindings::Proxy;

    async fn instantiate_raw(
        &self,
        store: &mut wasmtime::Store<SharedCtx>,
    ) -> wasmtime::Result<Self::Instance> {
        self.instantiate_async(store).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// Every `.rs` file under `dir`, recursively.
    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The guard for what clippy's list cannot name; see the module docs.
    ///
    /// Production code may call `instantiate_async(` in this module and in
    /// the body of a `fn instantiate_raw` (a [`super::LinkedPre`] impl).
    /// A file's inline test modules (a column-zero `#[cfg(test)]` before a
    /// `mod … {`, through its column-zero closing brace) and files that are
    /// test modules themselves (`*_tests.rs`, `tests.rs`) are skipped; an
    /// out-of-line `#[cfg(test)] mod x;` skips nothing here.
    #[test]
    fn nothing_outside_linked_instantiates_a_pre_directly() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        let mut offenders = Vec::new();
        for file in files {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            let skipped = file.ends_with("engine/linked.rs")
                || name == "tests.rs"
                || name.ends_with("_tests.rs");
            if skipped {
                continue;
            }
            let source = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = source.lines().collect();
            let mut in_test_module = false;
            // The signature of the function the scan is in.
            let mut function = "";
            for (index, line) in lines.iter().enumerate() {
                if in_test_module {
                    in_test_module = *line != "}";
                    continue;
                }
                let next = lines.get(index + 1).map_or("", |next| next.trim_start());
                let module = next.starts_with("mod ") || next.contains(" mod ");
                if *line == "#[cfg(test)]" && module && next.ends_with('{') {
                    in_test_module = true;
                    continue;
                }
                // A column-zero brace closes the item the scan was in.
                if *line == "}" {
                    function = "";
                }
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                if code.starts_with("fn ") || code.contains(" fn ") {
                    function = code;
                }
                let raw_impl = function.contains("fn instantiate_raw");
                if code.contains("instantiate_async(") && !raw_impl {
                    offenders.push(format!("{}:{}: {code}", file.display(), index + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "instantiate through `engine::linked::Linked`, not a pre's own \
             `instantiate_async`:\n{}",
            offenders.join("\n")
        );
    }
}
