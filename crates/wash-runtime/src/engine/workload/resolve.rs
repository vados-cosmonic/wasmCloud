//! Turning an [`UnresolvedWorkload`] into a [`ResolvedWorkload`]:
//! binding plugins, linking components, and assembling the result.

use super::*;

impl UnresolvedWorkload {
    /// Iterates the workload's components in manifest order.
    pub(super) fn components_in_manifest_order(&self) -> impl Iterator<Item = &WorkloadComponent> {
        self.component_order
            .iter()
            .filter_map(|id| self.components.get(id))
    }

    /// Picks the component that serves a host-dispatched export. Used
    /// whenever the host can route an export to only one component
    /// per workload (e.g. the `wasi:http` entrypoint). When several
    /// components carry the export, a warning will be logged.
    pub(super) fn select_exporter(
        &self,
        export: &str,
        exports: impl Fn(&WorkloadComponent) -> bool,
    ) -> Option<&WorkloadComponent> {
        let mut exporters = self.components_in_manifest_order().filter(|c| exports(c));
        let selected = exporters.next()?;
        let ignored: Vec<&str> = exporters.map(|c| c.name()).collect();
        if !ignored.is_empty() {
            warn!(
                export,
                selected = selected.name(),
                ?ignored,
                "multiple components carry an export the host dispatches to only one component; routing to the first in manifest order and ignoring the rest"
            );
        }
        Some(selected)
    }

    /// Removes and returns the component `id`, if present.
    ///
    /// Used by the host-component-plugin loader, which represents a loading
    /// plugin's own native-capability imports as a synthetic single-component
    /// workload purely to reuse `bind_plugins`'s matching/rollback logic, then
    /// recovers the (now-linked) component this way instead of exposing the
    /// backing map.
    #[cfg(feature = "host-component-plugins")]
    pub(crate) fn take_component(&mut self, id: &str) -> Option<WorkloadComponent> {
        self.components.remove(id)
    }

    /// Resolves the workload by binding it to host plugins and creating the final executable workload.
    ///
    /// This method performs the final resolution step that transforms an unresolved workload
    /// into a [`ResolvedWorkload`] ready for execution. It:
    ///
    /// 1. Binds components to matching host plugins based on required interfaces
    /// 2. Configures component linkers with plugin implementations
    /// 3. Validates that all component dependencies are satisfied
    /// 4. Creates the final resolved workload representation
    /// 5. Notifies plugins that the workload has been resolved
    ///
    /// # Arguments
    /// * `plugins` - Optional map of available host plugins for binding
    ///
    /// # Returns
    /// A [`ResolvedWorkload`] ready for component instantiation and execution.
    ///
    /// # Errors
    /// Returns an error if:
    /// - Required interfaces cannot be satisfied by available plugins
    /// - Plugin binding fails
    /// - Component linking fails
    /// - Plugin notification fails
    pub async fn resolve(
        self,
        plugins: Option<&HashMap<&'static str, Arc<dyn HostPlugin + 'static>>>,
        plugin_bindings: &crate::plugin::PluginBindings,
        host: &crate::host::HostRef,
        meters: &crate::observability::Meters,
    ) -> anyhow::Result<ResolvedWorkload> {
        self.resolve_for_start(plugins, plugin_bindings, host, meters, None)
            .await
    }

    #[instrument(name="resolve_workload", skip_all, fields(workload.id = self.id.as_ref(), workload.name = self.name.as_ref(), workload.namespace = self.namespace.as_ref()))]
    pub(crate) async fn resolve_for_start(
        mut self,
        plugins: Option<&HashMap<&'static str, Arc<dyn HostPlugin>>>,
        plugin_bindings: &crate::plugin::PluginBindings,
        host: &crate::host::HostRef,
        meters: &crate::observability::Meters,
        cleanup: Option<&WorkloadResources>,
    ) -> anyhow::Result<ResolvedWorkload> {
        // Bind to plugins
        let bound_plugins = if let Some(plugins) = plugins {
            trace!("binding plugins to workload");
            self.bind_plugins_for_start(plugins, plugin_bindings, cleanup)
                .await?
        } else {
            Vec::new()
        };

        let incoming_http_component = {
            match self
                .host_interfaces
                .iter()
                .any(|hi| hi.is_incoming_http_handler())
            {
                // http was not part of the requested interfaces
                false => None,
                // find first component in the manifest to serve the wasi:http export, if any
                true => self
                    .select_exporter("wasi:http", |c| c.exports_wasi_http())
                    .map(|c| c.id().to_string()),
            }
        };

        // Resolve the workload
        let mut resolved_workload = ResolvedWorkload {
            stream_payloads: self.stream_payloads,
            id: self.id.clone(),
            name: self.name.clone(),
            namespace: self.namespace.clone(),
            component_count: self.components.len(),
            components: Arc::new(RwLock::new(self.components)),
            service: self.service.map(Arc::new),
            service_calls: Arc::default(),
            released: Arc::default(),
            host_interfaces: Arc::new(self.host_interfaces),
            teardown_bindings: Arc::default(),
            http_handler: host.clone(),
            invocation: meters.invocation.clone(),
            #[cfg(feature = "wasi-tls")]
            tls_provider: self.tls_provider,
            scratch: self.scratch,
        };

        // Record the workload in the journal *before* linking,
        // so a link failure below is rolled back by `WorkloadResources::release`,
        // which needs it for instance shutdown and the http unbinds.
        // `seal_linkers` records it again once linking may have copied the service.
        if let Some(cleanup) = cleanup {
            cleanup.resolved(&resolved_workload);
        }
        if let Err(e) = resolved_workload.link_components().await {
            // If linking fails, unbind all plugins before returning the error
            warn!(
                error = ?e,
                "failed to link components, unbinding all plugins"
            );
            if cleanup.is_none() {
                let _ = resolved_workload.unbind_all_plugins().await;
            }
            bail!(e);
        }

        // Canonicalize component volume mounts up front so bad paths fail at
        // deploy time, not on first request (the service resolves its own).
        let all_component_ids: Vec<Arc<str>> = resolved_workload
            .components
            .read()
            .await
            .keys()
            .cloned()
            .collect();
        if let Err(e) = resolved_workload
            .resolve_component_volume_mounts(&all_component_ids)
            .await
        {
            // Same rollback as the linking failure above: plugins are already
            // bound at this point, so a bad hostPath would otherwise leave
            // their state — a live NATS connection among it — registered
            // against a workload that never deploys.
            warn!(
                error = ?e,
                "failed to resolve component volume mounts, unbinding all plugins"
            );
            if cleanup.is_none() {
                let _ = resolved_workload.unbind_all_plugins().await;
            }
            bail!(e);
        }

        // Binding and linking are done, including the service's sibling
        // imports. Keep the links before notifying plugins, so their
        // `instantiate_pre` finds them already kept.
        resolved_workload.seal_linkers(cleanup).await;

        // Notify plugins of the resolved workload
        for (plugin, component_ids) in bound_plugins.iter() {
            debug!(
                plugin_id = plugin.id(),
                component_count = component_ids.len(),
                "notifying plugin of resolved workload"
            );
            // Call on_workload_resolved for each component this plugin is bound to
            for component_id in component_ids {
                if let Err(e) = plugin
                    .on_workload_resolved(&resolved_workload, component_id.as_str())
                    .await
                {
                    // If we fail to notify a plugin, unbind all plugins that were already bound
                    warn!(
                        plugin_id = plugin.id(),
                        component_id,
                        error = ?e,
                        "failed to notify plugin of resolved workload, unbinding all plugins"
                    );
                    if cleanup.is_none() {
                        let _ = resolved_workload.unbind_all_plugins().await;
                    }
                    bail!(e);
                }
            }
        }

        if let Some(component_id) = incoming_http_component
            && let Some(http_handler) = host.handler()
            && let Err(e) = http_handler
                .on_workload_resolved(&resolved_workload, &component_id)
                .await
        {
            warn!(
                component_id = component_id,
                error = ?e,
                "failed to notify HTTP handler of resolved workload, unbinding all plugins"
            );
            if cleanup.is_none() {
                let _ = resolved_workload.unbind_all_plugins().await;
            }
            bail!(e);
        }

        Ok(resolved_workload)
    }
}
