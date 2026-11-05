//! Releasing a workload:
//! the resource journal ([`WorkloadResources`]) kept across cancelled starts and stops,
//! unbinding plugins, and removing scratch volumes.

use super::*;

/// Resources a native workload has acquired, recorded before each binding await.
/// The host retains this journal when a start or stop is cancelled, until teardown
/// succeeds; a later stop can retry a failed cleanup without reusing its ID.
#[derive(Clone, Default)]
pub(crate) struct WorkloadResources {
    pub(super) state: Arc<std::sync::Mutex<WorkloadResourceState>>,
    pub(super) release_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingHttpUnbind {
    Workload,
    ServiceHttp,
    ServiceMessaging,
}

#[derive(Clone, Default)]
pub(super) struct WorkloadResourceState {
    pub(super) bindings: Vec<Option<PendingBinding>>,
    pub(super) resolved: Option<ResolvedWorkload>,
    pub(super) http_unbinds: Vec<PendingHttpUnbind>,
}

impl WorkloadResources {
    pub(crate) fn commit(&self, workload: &mut ResolvedWorkload) {
        workload.teardown_bindings = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bindings
            .iter()
            .flatten()
            .cloned()
            .collect();
    }

    pub(crate) fn for_teardown(workload: &ResolvedWorkload) -> Self {
        let resources = Self::default();
        for (plugin, interfaces) in workload.teardown_bindings.iter() {
            resources.binding(Arc::clone(plugin), interfaces.clone());
        }
        resources.resolved(workload);
        resources
    }

    pub(super) fn binding(&self, plugin: Arc<dyn HostPlugin>, interfaces: HashSet<WitInterface>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bindings
            .push(Some((plugin, interfaces)));
    }

    /// Record the workload this journal tears down,
    /// and the http hooks [`Self::release`] must call for it.
    ///
    /// Called before linking (so a link failure can be rolled back)
    /// and again from [`ResolvedWorkload::seal_linkers`].
    ///
    /// The hooks listed here must match the http hooks
    /// [`ResolvedWorkload::unbind_all_plugins`] calls on the path without a journal.
    /// Add a hook to one and you must add it to the other.
    pub(crate) fn resolved(&self, workload: &ResolvedWorkload) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.resolved = Some(workload.clone());
        state.http_unbinds = vec![PendingHttpUnbind::Workload];
        if workload.service.is_some() {
            state.http_unbinds.extend([
                PendingHttpUnbind::ServiceHttp,
                PendingHttpUnbind::ServiceMessaging,
            ]);
        }
    }

    pub(crate) async fn release(&self, workload_id: &str) -> anyhow::Result<()> {
        // Record each successful step before another await, so failed or
        // cancelled cleanup resumes without repeating completed callbacks.
        let _release = self.release_lock.lock().await;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut failure = None;
        if let Some(resolved) = &state.resolved {
            resolved.begin_teardown();
            for component in resolved.components.read().await.values() {
                component.instances.close();
            }
            if let Some(handler) = resolved.http_handler.handler() {
                // Each hook tolerates an ID that was never registered.
                for step in state.http_unbinds {
                    let result = match step {
                        PendingHttpUnbind::Workload => {
                            handler.on_workload_unbind(workload_id).await
                        }
                        PendingHttpUnbind::ServiceHttp => {
                            handler.on_service_http_unbind(workload_id).await
                        }
                        PendingHttpUnbind::ServiceMessaging => {
                            handler
                                .on_trigger_service_messaging_unbind(workload_id)
                                .await
                        }
                    };
                    match result {
                        Ok(()) => self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .http_unbinds
                            .retain(|pending| *pending != step),
                        Err(error) => failure = Some(error),
                    }
                }
            } else {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .http_unbinds
                    .clear();
            }
            let mut pending = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.http_unbinds.is_empty() {
                pending.resolved = None;
            }
        }
        for (index, binding) in state.bindings.iter().enumerate().rev() {
            let Some((plugin, interfaces)) = binding else {
                continue;
            };
            match plugin
                .on_workload_unbind(workload_id, WitInterfaces::new(interfaces))
                .await
            {
                Ok(()) => {
                    if let Some(pending) = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .bindings
                        .get_mut(index)
                    {
                        *pending = None;
                    }
                }
                Err(error) => {
                    warn!(plugin_id = plugin.id(), workload_id, %error, "cancelled workload cleanup failed");
                    failure = Some(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Unbind every plugin already bound for `workload_id`, newest first.
///
/// The rollback every bind failure performs: without it a plugin that bound
/// successfully keeps tracking a workload that never deploys. Cleanup errors
/// are logged rather than returned — the caller is already failing, and the
/// error it has is the one worth reporting.
///
/// A no-op when `cleanup` is `Some`:
/// the journal owns rollback then,
/// and [`WorkloadResources::release`] unbinds what it recorded.
pub(super) async fn unbind_all(
    workload_id: &str,
    bound: &[BoundPluginWithInterfaces],
    reason: &str,
    cleanup: Option<&WorkloadResources>,
) {
    // Native starts roll back through their guard's journal, which records
    // completed steps. Other callers perform their rollback here.
    if cleanup.is_some() {
        return;
    }
    for (plugin, interfaces, _) in bound.iter().rev() {
        debug!(
            plugin_id = plugin.id(),
            reason, "calling on_workload_unbind for cleanup"
        );
        if let Err(cleanup_err) = plugin
            .on_workload_unbind(workload_id, WitInterfaces::new(interfaces))
            .await
        {
            warn!(
                plugin_id = plugin.id(),
                reason,
                error = ?cleanup_err,
                "failed to cleanup plugin after bind failure"
            );
        }
    }
}

impl ResolvedWorkload {
    /// Remove this workload's `emptyDir` volumes. Called on release, not left
    /// to whichever clone of this workload happens to drop last.
    pub(crate) fn remove_scratch(&self) {
        if let Some(scratch) = &self.scratch {
            scratch.remove();
        }
    }

    /// Let go of everything this workload is running, as the first step of
    /// tearing it down (see `crate::host::release`, the one funnel every
    /// teardown path goes through).
    ///
    /// Dispatch stops here rather than when the plugins are unbound: a plugin
    /// still holding a [`DispatchTarget`] must not build — or park — an
    /// instance in a workload that is going away.
    pub(crate) fn begin_teardown(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Close the dispatch ingress too, so a plugin mid-delivery to the
        // service is told now rather than waiting on a reply from a driver that
        // is about to be aborted.
        self.service_calls.shutdown();
        self.stop_service();
    }

    /// Whether this workload is being torn down, and so must take no more
    /// dispatched calls.
    pub(crate) fn released(&self) -> bool {
        self.released.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Unbind all plugins from all components in this workload.
    ///
    /// This should be called when stopping a workload to ensure proper cleanup
    /// of plugin resources. Errors from individual plugin unbind operations are
    /// logged but do not prevent the overall unbind from completing.
    #[instrument(name="unbind_all_plugins", skip_all, fields(workload.id = self.id.as_ref(), workload.name = self.name.as_ref()))]
    pub async fn unbind_all_plugins(&self) -> anyhow::Result<()> {
        trace!(
            workload_id = self.id.as_ref(),
            workload_name = self.name.as_ref(),
            "unbinding all plugins from workload"
        );

        for component in self.components.read().await.values() {
            // Warm instances hold guest resources (sockets, open files) for as
            // long as they stay parked, so release them with the rest of the
            // workload's teardown, and close the pool against a call that is
            // building one right now. Calls still in flight own their own
            // stores and are unaffected.
            component.instances.close();

            if let Some(plugins) = component.plugins() {
                for (plugin_id, plugin) in plugins.iter() {
                    trace!(
                        plugin_id,
                        component_id = component.id(),
                        workload_id = self.id.as_ref(),
                        "unbinding plugin from component"
                    );

                    // Recompute what this plugin was bound to
                    // as "what the component imports ∩ what the plugin imports".
                    // This is not always the set bind passed to `on_workload_bind`
                    // (see `bind_plugins_for_start`, which resolves labels and policy);
                    // the journal path unbinds the exact recorded set instead.
                    let world = component.world();
                    let plugin_world = plugin.world();

                    // Find the intersection of what the component imports and what the plugin provides
                    let bound_interfaces = world
                        .imports
                        .iter()
                        .filter(|import| plugin_world.imports.contains(import))
                        .cloned()
                        .collect::<std::collections::HashSet<_>>();

                    if let Err(e) = plugin
                        .on_workload_unbind(self.id(), WitInterfaces::new(&bound_interfaces))
                        .await
                    {
                        warn!(
                            plugin_id,
                            component_id = component.id(),
                            workload_id = self.id.as_ref(),
                            error = ?e,
                            "failed to unbind plugin from workload, continuing cleanup"
                        );
                    }
                }
            }
        }

        // Once per workload, whatever it exports: a workload that only sends
        // HTTP still holds egress state (pooled connections, TLS sessions,
        // connection permits) to release. A handler that is already gone has
        // nothing left to unbind from, so teardown treats that as done.
        if let Some(http_handler) = self.http_handler.handler()
            && let Err(e) = http_handler.on_workload_unbind(self.id()).await
        {
            warn!(
                workload_id = self.id.as_ref(),
                error = ?e,
                "failed to unbind HTTP handler from workload, continuing cleanup"
            );
        }

        // The service item records plugin bindings just like a component;
        // unbind them the same way so a plugin bound only to the service is
        // not left tracking a stopped workload.
        if let Some(service) = self.service.as_ref()
            && let Some(plugins) = service.plugins()
        {
            for (plugin_id, plugin) in plugins.iter() {
                let world = service.world();
                let plugin_world = plugin.world();
                let bound_interfaces = world
                    .imports
                    .iter()
                    .filter(|import| plugin_world.imports.contains(import))
                    .cloned()
                    .collect::<std::collections::HashSet<_>>();

                if let Err(e) = plugin
                    .on_workload_unbind(self.id(), WitInterfaces::new(&bound_interfaces))
                    .await
                {
                    warn!(
                        plugin_id,
                        service_id = service.id(),
                        workload_id = self.id.as_ref(),
                        error = ?e,
                        "failed to unbind plugin from service, continuing cleanup"
                    );
                }
            }
        }

        // These http hooks, with the workload one above,
        // must match the list `WorkloadResources::resolved` records for the journal path.
        //
        // A trigger service registered its HTTP/messaging handlers at start
        // (`execute_trigger_service`); drop those registrations on stop so it no
        // longer receives host-invoked deliveries on a torn-down instance.
        if self.service.is_some()
            && let Some(http_handler) = self.http_handler.handler()
        {
            if let Err(e) = http_handler.on_service_http_unbind(self.id()).await {
                tracing::error!(workload.id = %self.id(), err = %e, "failed to unbind service HTTP handler, continuing");
            }
            if let Err(e) = http_handler
                .on_trigger_service_messaging_unbind(self.id())
                .await
            {
                tracing::error!(workload.id = %self.id(), err = %e, "failed to unbind trigger service messaging handler, continuing");
            }
        }

        Ok(())
    }
}
