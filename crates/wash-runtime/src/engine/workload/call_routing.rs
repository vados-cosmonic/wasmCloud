//! Routing a call into a resolved workload:
//! to the long-lived service when it can take it,
//! otherwise to a component instance from its pool.

use super::*;

impl ResolvedWorkload {
    /// Resolve one of this workload's items — a component, or its service — as
    /// something a host plugin can dispatch work to.
    ///
    /// This is how a plugin that drives an external event stream reaches the
    /// workload's guest code: it resolves a target for each item it was told
    /// about, from [`HostPlugin::on_workload_resolved`], and dispatches through
    /// it for as long as the workload runs. Where the call ends up running —
    /// a warm instance, a store built for it, or the running service — is the
    /// host's to decide; see [`crate::engine::dispatch`].
    ///
    /// `plugin` is the dispatcher's own [`HostPlugin::id`], which names it in
    /// the guest-execution metrics its calls produce. It is `'static` because a
    /// plugin id is fixed for the life of the build, and the attribute would
    /// otherwise be a dimension traffic could invent.
    ///
    /// # What it changes for a service
    ///
    /// A claimed service is run with an ingress for those calls, so it outlives
    /// its own `cli/run`: a push-mode handler whose `run` returns — or which has
    /// no long-running work at all — stays up to keep receiving. Its restart
    /// budget still means what the workload said it means, though: a `cli/run`
    /// that *fails* ends the incarnation and spends one restart, exactly as it
    /// would without any of this.
    ///
    /// # Errors
    ///
    /// - the workload has no such item, or
    /// - the item is a service that cannot serve dispatched calls: a p2 service
    ///   has no concurrent driver to run one alongside its `cli/run`, and a
    ///   service already running cannot have an ingress added to it — which is
    ///   why this belongs in `on_workload_resolved`, before the workload
    ///   starts.
    ///
    /// [`HostPlugin::on_workload_resolved`]: crate::plugin::HostPlugin::on_workload_resolved
    pub async fn dispatch_target(
        &self,
        item_id: &str,
        plugin: &'static str,
    ) -> anyhow::Result<DispatchTarget> {
        DispatchTarget::resolve(self, item_id, plugin).await
    }

    /// Whether `item_id` names this workload's service rather than one of its
    /// components.
    ///
    /// Says nothing about whether that service *can* serve dispatched calls —
    /// see [`Self::service_can_dispatch`], which the callers ask separately
    /// because they answer a refusal differently.
    pub(crate) fn is_service_item(&self, item_id: &str) -> bool {
        self.service.as_ref().is_some_and(|s| s.id() == item_id)
    }

    /// Claim this workload's service as a dispatch target, refusing when it
    /// could never serve one. The claim keeps the service's ingress in place
    /// (see [`ServiceClaim`]); dropping or releasing it before the service
    /// starts takes the ingress back.
    pub(crate) fn claim_service_dispatch(&self) -> anyhow::Result<ServiceClaim> {
        self.service_can_dispatch()?;
        self.service_calls.claim()
    }

    /// Whether this workload's service could serve a call dispatched to it.
    ///
    /// A p2 service drives `wasi:cli/run` and nothing else: it has no
    /// concurrent driver for a call to run alongside that, so there is nowhere
    /// to deliver one. Asking for such a target is an error; *finding* one while
    /// binding a plugin is not, and leaves the workload deploying without that
    /// route.
    pub(crate) fn service_can_dispatch(&self) -> anyhow::Result<()> {
        let service = self
            .service
            .as_ref()
            .context("workload has no service to dispatch to")?;
        ensure!(
            service.metadata.targets_p3(),
            "workload '{}' runs a p2 service, which drives `wasi:cli/run` alone and cannot serve \
             a call dispatched to it; a service a plugin calls into must target wasip3",
            self.id
        );
        Ok(())
    }

    /// What dispatching to `component_id` needs, resolved once: this workload's
    /// own key for it (so a caller holding one does not keep a second copy of
    /// the id), the component's kept link, and the warm set
    /// its calls run on.
    ///
    /// All three under one read lock, so a dispatch takes none: what they answer
    /// is settled when the workload resolves (see
    /// [`WorkloadComponent::pre_instantiate_ref`] and [`instance_pool::poolable`]).
    pub(crate) async fn component_dispatch(
        &self,
        component_id: &str,
    ) -> anyhow::Result<(Arc<str>, InstancePre<SharedCtx>, Option<Arc<InstancePool>>)> {
        let components = self.components.read().await;
        let (id, component) = components.get_key_value(component_id).with_context(|| {
            format!(
                "workload '{}' has no item '{component_id}' to dispatch to",
                self.id
            )
        })?;
        let pre = component.pre_instantiate_ref().with_context(|| {
            format!("component '{component_id}' cannot be pre-instantiated to dispatch to")
        })?;
        let pool = instance_pool::poolable(
            &components,
            component_id,
            &component.metadata.linked_components,
        );
        Ok((Arc::clone(id), pre, pool))
    }

    pub(crate) async fn instance_pool_for_component(
        &self,
        component_id: &str,
    ) -> Option<Arc<InstancePool>> {
        let components = self.components.read().await;
        let component = components.get(component_id)?;
        instance_pool::poolable(
            &components,
            component_id,
            &component.metadata.linked_components,
        )
    }

    /// The instance policy that actually applies to `component_id`'s stores:
    /// [`InstancePolicy::Ephemeral`] unless it *and every component linked
    /// into its store* opted into pooling — the same rule
    /// [`Self::instance_pool_for_component`] applies, for the same reason: a
    /// store holds the whole linked set, so keeping it warm keeps all of them
    /// warm.
    ///
    /// For a plugin that has to keep a warm set of its own rather than
    /// dispatching through [`Self::dispatch_target`] — the `wasmcloud:nats`
    /// subscriber's JetStream deliveries, whose call carries a typed `resource`
    /// handle that is an index into the very store table it must run in, so its
    /// argument cannot exist before the store is chosen. Anything that *can*
    /// take the dispatch path should, and get the pool itself rather than only
    /// the numbers describing it.
    pub async fn warm_instance_policy(&self, component_id: &str) -> InstancePolicy {
        match self.instance_pool_for_component(component_id).await {
            Some(pool) => pool.policy(),
            None => InstancePolicy::Ephemeral,
        }
    }
}
