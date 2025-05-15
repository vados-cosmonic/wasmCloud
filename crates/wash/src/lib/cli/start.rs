use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Context, Result};
use clap::Parser;
use tokio::time::Duration;

use crate::lib::cli::{input_vec_to_hashmap, CliConnectionOpts, CommandOutput};
use crate::lib::common::{boxed_err_to_anyhow, find_host_id};
use crate::lib::component::{scale_component, ComponentScaledInfo, ScaleComponentArgs};
use crate::lib::config::{
    WashConnectionOptions, DEFAULT_NATS_TIMEOUT_MS, DEFAULT_START_COMPONENT_TIMEOUT_MS,
    DEFAULT_START_PROVIDER_TIMEOUT_MS,
};
use crate::lib::context::default_timeout_ms;
use crate::lib::wait::{wait_for_provider_start_event, FindEventOutcome, ProviderStartedInfo};

use super::validate_component_id;

pub async fn handle_start_component(cmd: StartComponentCommand) -> Result<CommandOutput> {
    // If timeout isn't supplied, override with a longer timeout for starting component
    let timeout_ms = if cmd.opts.timeout_ms == DEFAULT_NATS_TIMEOUT_MS {
        DEFAULT_START_COMPONENT_TIMEOUT_MS
    } else {
        cmd.opts.timeout_ms
    };
    let client = <CliConnectionOpts as TryInto<WashConnectionOptions>>::try_into(cmd.opts)?
        .into_ctl_client(Some(cmd.auction_timeout_ms))
        .await?;

    let component_ref = resolve_ref(&cmd.component_ref).await?;

    let host = if let Some(host) = cmd.host_id {
        find_host_id(&host, &client).await?.0
    } else {
        let suitable_hosts = client
            .perform_component_auction(
                &component_ref,
                &cmd.component_id,
                BTreeMap::from_iter(input_vec_to_hashmap(cmd.constraints.unwrap_or_default())?),
            )
            .await
            .map_err(boxed_err_to_anyhow)
            .with_context(|| {
                format!(
                    "Failed to auction component {} to hosts in lattice",
                    &component_ref
                )
            })?;
        if suitable_hosts.is_empty() {
            bail!("No suitable hosts found for component {}", component_ref);
        } else {
            let acks = suitable_hosts
                .into_iter()
                .filter_map(wasmcloud_control_interface::CtlResponse::into_data)
                .collect::<Vec<_>>();
            let ack = acks.first().context("No suitable hosts found")?;
            ack.host_id()
                .parse()
                .with_context(|| format!("Failed to parse host id: {}", ack.host_id()))?
        }
    };

    // Start the component
    let ComponentScaledInfo {
        host_id,
        component_ref,
        component_id,
    } = scale_component(ScaleComponentArgs {
        client: &client,
        host_id: &host,
        component_ref: &component_ref,
        component_id: &cmd.component_id,
        max_instances: cmd.max_instances,
        skip_wait: cmd.skip_wait,
        timeout_ms: Some(timeout_ms),
        annotations: None,
        config: cmd.config,
    })
    .await?;

    let text = if cmd.skip_wait {
        format!("Start component [{component_ref}] request received on host [{host_id}]",)
    } else {
        format!("Component [{component_id}] (ref: [{component_ref}]) started on host [{host_id}]",)
    };

    Ok(CommandOutput::new(
        text.clone(),
        HashMap::from([
            ("result".into(), text.into()),
            ("component_ref".into(), component_ref.into()),
            ("component_id".into(), component_id.into()),
            ("host_id".into(), host_id.into()),
        ]),
    ))
}

