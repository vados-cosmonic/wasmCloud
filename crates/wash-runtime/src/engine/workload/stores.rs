//! Building stores for a resolved workload's items,
//! admitting calls against its concurrency limits,
//! and identifying components for metering.

use super::*;

impl ResolvedWorkload {
    /// Cost exactly the components `new_store` instantiates, plus the active
    /// component its caller instantiates afterwards. Resolve this once when a
    /// trigger binds, under the same metadata lock used to build stores.
    pub(crate) async fn admit_store(
        &self,
        component_id: &str,
        pool: &crate::engine::pool_share::PoolShare,
    ) -> anyhow::Result<crate::engine::pool_share::PoolCost> {
        let components = self.components.read().await;
        let component = components
            .get(component_id)
            .context("component ID not found in workload")?;
        let mut instantiated = Vec::with_capacity(component.metadata.linked_components.len() + 1);
        instantiated.push(component.metadata.component());
        let mut may_use_gc = component.metadata.may_use_gc;
        for linked_id in &component.metadata.linked_components {
            let linked = components
                .get(linked_id)
                .with_context(|| format!("linked component '{linked_id}' not found in workload"))?;
            instantiated.push(linked.metadata.component());
            may_use_gc |= linked.metadata.may_use_gc;
        }
        pool.admit_store(instantiated, may_use_gc)
    }

    /// A store for `component_id`, with the components it links to, stamped
    /// with whose execution it runs.
    ///
    /// Built under one read lock: `pre_instantiate_ref` needs only read
    /// access, so concurrent callers don't serialize on a write lock, and
    /// nothing is retained past the returned store.
    ///
    /// The stamp is what lets the epoch callback credit `guest.execution.total`
    /// without a call to hang the number on — the only correct instrument for a
    /// store several calls share. See
    /// [`crate::engine::abandon::GuestExecution`].
    ///
    /// This is the raw store, and a store of its own is what the component asked
    /// *not* to have when it set `poolSize`. A host plugin calling into a
    /// workload should go through [`Self::dispatch_target`] instead, which
    /// serves the call on a warm instance where there is one — and reaches the
    /// workload's service, which has no store to build at all.
    pub async fn new_store(
        &self,
        component_id: &str,
    ) -> anyhow::Result<wasmtime::Store<SharedCtx>> {
        self.new_store_as(component_id, None).await
    }

    /// [`Self::new_store`] for a caller that resolved the component's
    /// [`Self::component_identity`] once, where it registered the route or
    /// subscription, so a request pays neither the read lock nor the
    /// allocations of looking it up again.
    pub(crate) async fn new_store_as(
        &self,
        component_id: &str,
        identity: Option<&crate::observability::WorkloadIdentity>,
    ) -> anyhow::Result<wasmtime::Store<SharedCtx>> {
        let (engine, active_template, linked_templates, linked_instances) = {
            let components = self.components.read().await;
            let component = components
                .get(component_id)
                .context("component ID not found in workload")?;
            let metadata = &component.metadata;
            let active_template = self.component_ctx_template(metadata);
            let mut linked_templates = Vec::with_capacity(metadata.linked_components.len());
            let mut linked_instances = Vec::with_capacity(metadata.linked_components.len());
            for linked_id in &metadata.linked_components {
                let linked = components.get(linked_id).with_context(|| {
                    format!("linked component '{linked_id}' not found in workload")
                })?;
                linked_templates.push(self.component_ctx_template(&linked.metadata));
                linked_instances.push((linked_id.clone(), linked.pre_instantiate_ref()?));
            }
            (
                metadata.engine().clone(),
                active_template,
                linked_templates,
                linked_instances,
            )
        };
        let store = new_store_from_templates(
            &engine,
            &self.stream_payloads,
            &self.http_handler,
            &active_template,
            &linked_templates,
            &linked_instances,
            false,
        )
        .await?;
        // This host's own meter, not a process-wide one: the read lock and the
        // allocations below are on the hot path of the p2 HTTP path, which
        // builds a store per request, and of the per-message messaging paths.
        // A host asked to measure nothing must not pay for them because some
        // other host in the process measures.
        if self.invocation.is_enabled() {
            let identity = match identity {
                Some(identity) => identity.clone(),
                None => self.component_identity(component_id).await,
            };
            store
                .data()
                .executed
                .set_identity(identity, self.invocation.clone());
        }
        Ok(store)
    }

    /// The pool a request store for `component_id` may be parked in, or `None`
    /// when it must be built and dropped per request.
    ///
    /// The component's linked components are instantiated into the same store
    /// by [`Self::new_store`] and live exactly as long as it does, so they have
    /// to have opted in too — see [`instance_pool::poolable`].
    /// Guest calls `component_id` may have in flight at once, as it declared
    /// them. One for an unknown component, or one that keeps no instances.
    ///
    /// The outbound HTTP pool sizes itself off this: a component's concurrent
    /// outbound requests scale with the calls it runs at once.
    pub(crate) async fn call_concurrency(&self, component_id: &str) -> usize {
        self.components
            .read()
            .await
            .get(component_id)
            .map_or(1, |component| component.instances.call_concurrency())
    }

    /// The manifest identity an item's guest-execution measurements carry.
    ///
    /// Manifest names throughout, never the ids — see
    /// [`crate::observability::WorkloadIdentity`] for why. Resolve it once
    /// where a subscription or a route is set up rather than per call: the
    /// name costs a read lock, and it cannot change under a resolved workload.
    pub async fn component_identity(
        &self,
        item_id: &str,
    ) -> crate::observability::WorkloadIdentity {
        // The workload's service is bound as an item like any component and its
        // id arrives here the same way, but it lives outside the component map
        // — so answer for it explicitly rather than letting the lookup miss.
        // Every messaging delivery to a trigger service passes a service id,
        // so a miss here is the steady state for those workloads, not a race.
        // `service` is the name `wasmcloud:messaging`'s admission counter uses
        // for the same item, so the two metrics agree.
        if let Some(service) = &self.service
            && service.id() == item_id
        {
            return crate::observability::WorkloadIdentity::new(
                self.namespace(),
                self.name(),
                "service",
            );
        }
        let name = self
            .components
            .read()
            .await
            .get(item_id)
            .map(|component| Arc::<str>::from(component.name()));
        crate::observability::WorkloadIdentity::new(
            self.namespace(),
            self.name(),
            // Neither a live component nor the service: an item being torn
            // down. The call that raced it still has to be measured under
            // something.
            name.as_deref().unwrap_or("unknown"),
        )
    }
}
