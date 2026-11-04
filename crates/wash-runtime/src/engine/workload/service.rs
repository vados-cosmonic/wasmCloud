//! Running and supervising a workload's long-lived service:
//! p2 and p3 execution, trigger services and their ingresses,
//! restarts on a fresh store, and stopping.

use super::*;

/// Everything needed to rebuild a p3 service's store (plain `cli/run` or
/// trigger service), captured by its supervisor so each incarnation gets a
/// FRESH store: after a guest trap the old store can no longer enter ANY
/// component instance ("cannot enter component instance"), so reusing it would
/// leave every restarted incarnation permanently broken.
pub(super) struct ServiceStoreRecipe {
    pub(super) engine: wasmtime::Engine,
    pub(super) stream_payloads: Arc<crate::engine::stream_payload::StreamPayloads>,
    /// Weak, like the workload's own; the supervisor holds this recipe for as
    /// long as the service runs.
    pub(super) http_handler: crate::host::HostRef,
    pub(super) active_template: ComponentCtxTemplate,
    pub(super) linked_templates: Vec<ComponentCtxTemplate>,
    pub(super) linked_instances: Vec<(Arc<str>, wasmtime::component::InstancePre<SharedCtx>)>,
    /// Stamped onto every incarnation's store, so a restart is measured like
    /// the start was. `None` on a host that measures nothing.
    pub(super) metering: Option<(
        crate::observability::WorkloadIdentity,
        crate::observability::InvocationMeter,
    )>,
}

impl ServiceStoreRecipe {
    /// Build a fresh service store. Always a service store: the flag it passes
    /// is what lets `cli/run` bind its loopback socket, and every store built
    /// from this recipe is an incarnation of a service that may want one.
    pub(super) async fn build(&self) -> anyhow::Result<wasmtime::Store<SharedCtx>> {
        let store = new_store_from_templates(
            &self.engine,
            &self.stream_payloads,
            &self.http_handler,
            &self.active_template,
            &self.linked_templates,
            &self.linked_instances,
            true,
        )
        .await?;
        // Every store a call is measured on has to be stamped, this one
        // included: a trigger service's deliveries are recorded through the
        // store they run on, and its messaging attributes are built by the
        // plugin rather than from the stamp — so an unstamped store loses
        // fully attributed data points rather than empty ones.
        if let Some((identity, invocation)) = &self.metering {
            store
                .data()
                .executed
                .set_identity(identity.clone(), invocation.clone());
        }
        Ok(store)
    }
}

/// Build a trigger service's host-invoked ingresses, returning them alongside the
/// paired senders to register with the host-side HTTP/messaging ingresses. Called
/// once per incarnation (start and each restart) so a restarted service gets fresh
/// channels whose senders replace the stale registrations.
///
/// The plugin-dispatch ingress is built the same way, but its sender is swapped
/// inside [`ServiceCalls`] rather than returned: dispatchers hold the ingress
/// itself, so a restart replaces the channel under them with nothing to
/// re-register.
#[allow(clippy::type_complexity)]
pub(super) fn build_trigger_ingresses(
    serves_http: bool,
    serves_messaging: bool,
    service_calls: &ServiceCalls,
) -> (
    Vec<crate::host::trigger_service::Ingress>,
    Option<tokio::sync::mpsc::Sender<crate::host::http::ServiceHttpJob>>,
    Option<tokio::sync::mpsc::Sender<crate::host::trigger_service::MessagingJob>>,
) {
    let mut ingresses = Vec::new();
    if let Some(rx) = service_calls.next_incarnation() {
        ingresses.push(crate::host::trigger_service::Ingress::Guest(rx));
    }
    let http_tx = serves_http.then(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(INGRESS_BACKLOG);
        ingresses.push(crate::host::trigger_service::Ingress::Http(rx));
        tx
    });
    let messaging_tx = serves_messaging.then(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(INGRESS_BACKLOG);
        ingresses.push(crate::host::trigger_service::Ingress::Messaging(rx));
        tx
    });
    (ingresses, http_tx, messaging_tx)
}

impl ResolvedWorkload {
    /// Executes the service, if present, and returns whether it was run.
    #[instrument(name="execute_service", skip_all, fields(workload.id = self.id.as_ref(), workload.name = self.name.as_ref(), workload.namespace = self.namespace.as_ref()))]
    pub(crate) async fn execute_service(&mut self) -> anyhow::Result<Option<Arc<JoinHandle<()>>>> {
        // Which ingresses the service runs with is settled here, so a plugin
        // claiming it as a dispatch target from now on is refused rather than
        // handed a channel nothing would ever serve.
        self.service_calls.mark_started();
        if self
            .service
            .as_ref()
            .is_some_and(|s| s.metadata.targets_p3())
        {
            // A p3 service that also serves something the host delivers to it —
            // a host-invoked handler export (today `wasi:http/handler`), or
            // calls a plugin dispatches to it — co-drives that with `cli/run`
            // on one instance (see the `trigger service` module).
            if self.service_calls.claimed()
                || self.service.as_ref().is_some_and(|s| {
                    crate::engine::exports_wasi_http(&s.metadata.component)
                        || crate::engine::exports_messaging_handler(&s.metadata.component)
                })
            {
                return self.execute_trigger_service().await;
            }
            return self.execute_service_p3().await;
        }

        let Some(service) = self.service.as_ref() else {
            return Ok(None);
        };
        let pre = service.pre_instantiate()?;
        let mut max_restarts = service.max_restarts;
        self.resolve_service_volume_mounts().await?;
        // Re-borrow after resolving volume mounts, which needs `&mut self`
        let Some(service) = self.service.as_ref() else {
            bail!("service unexpectedly missing during execution");
        };
        let recipe = self.service_store_recipe(&service.metadata).await?;
        let mut store = recipe.build().await?;
        let mut instance = pre
            .instantiate_async(&mut store)
            .await
            .map_err(|e| crate::engine::pool_usage::explain_instantiate_failure(store.data(), e))?;
        let handle = tokio::spawn(async move {
            loop {
                match instance.wasi_cli_run().call_run(&mut store).await {
                    Ok(Ok(())) => {
                        info!("service exited successfully");
                        break;
                    }
                    Ok(Err(())) => error!(retries = max_restarts, "service exited with error"),
                    Err(e) => {
                        error!(err = ?e, retries = max_restarts, "service execution failed");
                        crate::engine::trap_remediation::note_guest_fault(store.data(), &e);
                    }
                }
                if max_restarts == 0 {
                    warn!("max restarts reached, service will not be restarted");
                    break;
                }
                max_restarts = max_restarts.saturating_sub(1);
                store = match recipe.build().await {
                    Ok(fresh) => fresh,
                    Err(e) => {
                        error!(err = ?e, "failed to rebuild service store; giving up");
                        break;
                    }
                };
                instance = match pre.instantiate_async(&mut store).await.map_err(|e| {
                    crate::engine::pool_usage::explain_instantiate_failure(store.data(), e)
                }) {
                    Ok(instance) => instance,
                    Err(e) => {
                        error!(err = ?e, "failed to instantiate service; giving up");
                        break;
                    }
                };
            }
        });

        let handle = Arc::new(handle);
        if let Some(s) = self.service_mut() {
            s.handle = Some(Arc::clone(&handle));
        }
        Ok(Some(handle))
    }

    /// Execute a service using P3 (wasi:cli@0.3) CommandPre.
    pub(super) async fn execute_service_p3(
        &mut self,
    ) -> anyhow::Result<Option<Arc<JoinHandle<()>>>> {
        let service = match self.service.as_ref() {
            Some(s) => Some((
                anyhow::Context::with_context(s.pre_instantiate_p3(), || {
                    format!(
                        "failed to pre-instantiate the P3 service of workload '{}/{}'",
                        s.metadata.workload_namespace(),
                        s.metadata.workload_name()
                    )
                })?,
                s.max_restarts,
            )),
            None => None,
        };

        if let Some((pre, mut max_restarts)) = service {
            self.resolve_service_volume_mounts().await?;
            // Capture the store recipe so each restarted incarnation gets a
            // FRESH store: a trapped store cannot re-enter any component
            // instance, so reuse after a fault would leave every restart
            // permanently broken (and even a clean restart would accumulate
            // stale instances in a reused store).
            let recipe = {
                let Some(service) = self.service.as_ref() else {
                    bail!("service unexpectedly missing during execution");
                };
                self.service_store_recipe(&service.metadata).await?
            };
            let mut store = recipe.build().await?;
            let mut instance = pre
                .instantiate_async(&mut store)
                .await
                .map_err(|e| {
                    crate::engine::pool_usage::explain_instantiate_failure(store.data(), e)
                })
                .context("failed to instantiate P3 service")?;
            let handle = tokio::spawn(async move {
                loop {
                    let result = store
                        .run_concurrent(async move |accessor| {
                            instance.wasi_cli_run().call_run(accessor).await
                        })
                        .await;
                    match result {
                        Ok(Ok(Ok(()))) => {
                            info!("P3 service exited successfully");
                            break;
                        }
                        Ok(Ok(Err(()))) => {
                            error!(retries = max_restarts, "P3 service exited with error");
                            if max_restarts == 0 {
                                warn!("max restarts reached, P3 service will not be restarted");
                                break;
                            }
                        }
                        Ok(Err(e)) | Err(e) => {
                            // `?e`, not `%e`: a trap's message is in the cause
                            // chain, which `Display` leaves out.
                            error!(err = ?e, retries = max_restarts, "P3 service execution failed");
                            crate::engine::trap_remediation::note_guest_fault(store.data(), &e);
                            if max_restarts == 0 {
                                warn!("max restarts reached, P3 service will not be restarted");
                                break;
                            }
                        }
                    }
                    max_restarts = max_restarts.saturating_sub(1);
                    match recipe.build().await {
                        Ok(fresh) => store = fresh,
                        Err(e) => {
                            error!(err = %e, "failed to rebuild P3 service store; giving up");
                            break;
                        }
                    }
                    instance = match pre.instantiate_async(&mut store).await.map_err(|e| {
                        crate::engine::pool_usage::explain_instantiate_failure(store.data(), e)
                    }) {
                        Ok(instance) => instance,
                        Err(e) => {
                            error!(err = ?e, "failed to instantiate P3 service; giving up");
                            break;
                        }
                    };
                }
            });

            let handle = Arc::new(handle);
            if let Some(s) = self.service_mut() {
                s.handle = Some(Arc::clone(&handle));
            }
            Ok(Some(handle))
        } else {
            Ok(None)
        }
    }

    /// Execute a p3 service that also exports a host-invoked handler (today
    /// `wasi:http/handler` and `wasmcloud:messaging`): one instance co-drives
    /// `cli/run` and the handler under a single `run_concurrent` (see
    /// [`crate::host::trigger_service`]). The service runs in its own
    /// long-lived store.
    /// `is_service = true` lets the `cli/run` side bind its loopback socket.
    pub(super) async fn execute_trigger_service(
        &mut self,
    ) -> anyhow::Result<Option<Arc<JoinHandle<()>>>> {
        let Some(service) = self.service.as_ref() else {
            return Ok(None);
        };
        let pre = service.pre_instantiate_raw()?;
        let (serves_http, serves_messaging, max_restarts) = (
            crate::engine::exports_wasi_http(&service.metadata.component),
            crate::engine::exports_messaging_handler(&service.metadata.component),
            service.max_restarts,
        );
        self.resolve_service_volume_mounts().await?;

        // Capture the store recipe so the supervisor below can rebuild a FRESH
        // store per incarnation: a trapped store cannot re-enter any component
        // instance, so a restart must never reuse it.
        let recipe = {
            let Some(service) = self.service.as_ref() else {
                bail!("service unexpectedly missing during execution");
            };
            self.service_store_recipe(&service.metadata).await?
        };
        let mut store = recipe.build().await?;
        let http_handler = self.http_handler.clone();
        let workload_id: Arc<str> = Arc::from(self.id());
        // Carried for the supervisor's own logs: the id alone does not say
        // which workload's service a restart decision is about.
        let workload_name: Arc<str> = self.name.clone();
        let workload_namespace: Arc<str> = self.namespace.clone();
        // The ingress routes this service serves HTTP on, derived once from the
        // workload's declared interfaces. Passed to every HTTP registration
        // (the first below and each restart re-registration in the supervisor)
        // so a hostname-keyed router can resolve requests to this service.
        let ingress_routes = crate::host::http::http_ingress_routes(self.host_interfaces())?;

        // Build the first incarnation's host-invoked ingresses. Each paired sender
        // is registered with its host-side ingress (the HTTP server, the messaging
        // subscriber), which then delivers to this live instance instead of
        // instantiating a component per request/message. The first registration is
        // synchronous (before the driver spawns) so a delivery immediately after
        // start finds the handler; restarts re-register from inside the supervisor.
        // What a failing `cli/run` costs this service. A service the host only
        // *dispatches* to would, without that ingress, run as a plain p3
        // service — where a `cli/run` error ends the incarnation and spends a
        // restart. Keep that: which plugins a workload binds must not change
        // what its `maxRestarts` means. A service whose handler exports are the
        // point keeps the standing behavior, where the handlers go on serving.
        let cli_run_error = if serves_http || serves_messaging {
            crate::host::trigger_service::CliRunError::Logged
        } else {
            crate::host::trigger_service::CliRunError::Fatal
        };
        let service_calls = Arc::clone(&self.service_calls);
        let (ingresses, http_tx, messaging_tx) =
            build_trigger_ingresses(serves_http, serves_messaging, &service_calls);
        let first = crate::host::trigger_service::PreparedTriggerDriver::instantiate(
            &mut store, &pre, ingresses,
        )
        .await?;
        if let Some(http_tx) = http_tx {
            self.http_handler()?
                .on_service_http_resolved(self.id(), &ingress_routes, http_tx)
                .await
                .map_err(|e| anyhow::anyhow!("failed to register service HTTP handler: {e:#}"))?;
        }
        if let Some(messaging_tx) = messaging_tx {
            self.http_handler()?
                .on_trigger_service_messaging_resolved(self.id(), messaging_tx)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("failed to register trigger service messaging handler: {e:#}")
                })?;
        }

        // Each restart gets a fresh store and replaces the ingress senders.
        let needs_handler = serves_http || serves_messaging;
        let handle = tokio::spawn(async move {
            let mut first = Some(first);
            let mut restarts = max_restarts;
            loop {
                let result = match first.take() {
                    Some(first) => first.run(&mut store, cli_run_error).await,
                    None => {
                        let (ingresses, http_tx, messaging_tx) =
                            build_trigger_ingresses(serves_http, serves_messaging, &service_calls);
                        // A service with ingresses cannot come back without a
                        // handler to re-register them with. One the host merely
                        // dispatches to has none, and restarts either way.
                        if needs_handler {
                            let Some(http_handler) = http_handler.handler() else {
                                error!(
                                    workload.id = %workload_id,
                                    workload.namespace = %workload_namespace,
                                    workload.name = %workload_name,
                                    "host HTTP handler is no longer available; \
                                     not restarting this workload's service"
                                );
                                break;
                            };
                            if let Some(http_tx) = http_tx
                                && let Err(e) = http_handler
                                    .on_service_http_resolved(
                                        &workload_id,
                                        &ingress_routes,
                                        http_tx,
                                    )
                                    .await
                            {
                                error!(
                                    workload.id = %workload_id,
                                    workload.namespace = %workload_namespace,
                                    workload.name = %workload_name,
                                    err = %e,
                                    "failed to re-register service HTTP handler on restart"
                                );
                            }
                            if let Some(messaging_tx) = messaging_tx
                                && let Err(e) = http_handler
                                    .on_trigger_service_messaging_resolved(
                                        &workload_id,
                                        messaging_tx,
                                    )
                                    .await
                            {
                                error!(
                                    workload.id = %workload_id,
                                    workload.namespace = %workload_namespace,
                                    workload.name = %workload_name,
                                    err = %e,
                                    "failed to re-register trigger service messaging handler on restart"
                                );
                            }
                        }
                        crate::host::trigger_service::run_trigger_driver(
                            &mut store,
                            &pre,
                            ingresses,
                            cli_run_error,
                        )
                        .await
                    }
                };
                match result {
                    Ok(()) => {
                        info!("trigger service exited");
                        break;
                    }
                    Err(e) if restarts == 0 => {
                        error!(err = %e, "trigger service faulted; max restarts reached");
                        break;
                    }
                    Err(e) => {
                        warn!(err = %e, retries = restarts, "trigger service faulted; restarting");
                        restarts = restarts.saturating_sub(1);
                        // The faulted store cannot re-enter any component
                        // instance; the next incarnation needs a fresh one.
                        match recipe.build().await {
                            Ok(fresh) => store = fresh,
                            Err(e) => {
                                error!(err = %e, "failed to rebuild trigger service store; giving up");
                                break;
                            }
                        }
                    }
                }
            }
        });

        let handle = Arc::new(handle);
        if let Some(s) = self.service_mut() {
            s.handle = Some(Arc::clone(&handle));
        }
        Ok(Some(handle))
    }

    /// The service, for one of the few writes after resolution. Copies it
    /// first when a clone of this workload shares it, so that clone keeps
    /// what it saw, as it did when the service was held by value.
    pub(super) fn service_mut(&mut self) -> Option<&mut WorkloadService> {
        self.service.as_mut().map(Arc::make_mut)
    }

    /// Aborts the running service [`JoinHandle`] if it exists.
    pub(super) fn stop_service(&self) {
        if let Some(service) = &self.service
            && let Some(handle) = &service.handle
        {
            handle.abort();
            debug!(
                workload_id = self.id.as_ref(),
                "service for workload aborted"
            );
        }
    }

    /// The identity a service's store runs under.
    ///
    /// Named `service` rather than by a component name it does not have — the
    /// same name `wasmcloud:messaging`'s admission counter gives it, so the two
    /// metrics join. A component's store is stamped by [`Self::new_store`],
    /// which resolves its real manifest name.
    pub(super) fn service_identity(
        &self,
        metadata: &WorkloadMetadata,
    ) -> crate::observability::WorkloadIdentity {
        crate::observability::WorkloadIdentity::new(
            metadata.workload_namespace(),
            metadata.workload_name(),
            "service",
        )
    }

    /// Capture everything needed to (re)build this service's store, so the
    /// trigger-service supervisor can construct a fresh store per incarnation
    /// without borrowing `self`.
    pub(super) async fn service_store_recipe(
        &self,
        metadata: &WorkloadMetadata,
    ) -> anyhow::Result<ServiceStoreRecipe> {
        let linked_component_ids = metadata
            .linked_components
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let linked_metadata = {
            let components = self.components.read().await;
            linked_component_ids
                .iter()
                .map(|linked_component_id| {
                    components
                        .get(linked_component_id)
                        .map(|component| component.metadata.clone())
                        .with_context(|| {
                            format!("linked component '{linked_component_id}' not found")
                        })
                })
                .collect::<wasmtime::Result<Vec<_>>>()?
        };
        let active_template = self.component_ctx_template(metadata);
        let linked_templates = linked_metadata
            .iter()
            .map(|metadata| self.component_ctx_template(metadata))
            .collect::<Vec<_>>();
        let mut linked_instances = Vec::with_capacity(linked_component_ids.len());
        for linked_component_id in &linked_component_ids {
            linked_instances.push((
                linked_component_id.clone(),
                self.instantiate_pre(linked_component_id.as_ref()).await?,
            ));
        }

        Ok(ServiceStoreRecipe {
            engine: metadata.engine().clone(),
            stream_payloads: self.stream_payloads.clone(),
            http_handler: self.http_handler.clone(),
            active_template,
            linked_templates,
            linked_instances,
            metering: self
                .invocation
                .is_enabled()
                .then(|| (self.service_identity(metadata), self.invocation.clone())),
        })
    }
}
