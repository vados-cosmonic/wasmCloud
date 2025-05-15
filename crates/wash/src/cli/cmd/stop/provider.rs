use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use std::collections::HashMap;
use tokio::time::Duration;
use tracing::error;
use wasmcloud_control_interface::HostInventory;

use crate::lib::{
    cli::{CliConnectionOpts, CommandOutput},
    common::{boxed_err_to_anyhow, find_host_id, get_all_inventories, FindIdError, Match},
    component::{scale_component, ComponentScaledInfo, ScaleComponentArgs},
    config::{host_pid_file, WashConnectionOptions},
    context::default_timeout_ms,
    id::ServerId,
    wait::{wait_for_provider_stop_event, FindEventOutcome, ProviderStoppedInfo},
};

use super::validate_component_id;

#[derive(Debug, Clone, Parser)]
pub struct StopProviderCommand {
    #[clap(flatten)]
    pub opts: CliConnectionOpts,

    /// Id of host to stop provider on. If a non-ID is provided, the host will be selected based on
    /// matching the prefix of the ID or the friendly name and will return an error if more than one
    /// host matches. If no host ID is passed, a host will be selected based on whether or not the
    /// provider is running on it. If more than 1 host is running this provider, an error will be returned
    /// with a list of hosts running the provider
    #[clap(long = "host-id")]
    pub host_id: Option<String>,

    /// Provider Id (e.g. the public key for the provider) or a string to match on the prefix of the
    /// ID, or friendly name, or call alias of the provider. If multiple providers are matched, then
    /// an error will be returned with a list of all matching options
    #[clap(name = "provider-id", value_parser = validate_component_id)]
    pub provider_id: String,

    /// By default, the command will wait until the provider has been stopped. If this flag is
    /// passed, the command will return immediately after acknowledgement from the host, without
    /// waiting for the provider to stop.
    #[clap(long = "skip-wait")]
    pub skip_wait: bool,
}

pub async fn handle_stop_provider(cmd: StopProviderCommand) -> Result<CommandOutput> {
    let timeout_ms = cmd.opts.timeout_ms;
    let wco: WashConnectionOptions = cmd.opts.try_into()?;
    let ctl_client = wco.into_ctl_client(None).await?;
    stop_provider(
        &ctl_client,
        cmd.host_id.as_deref(),
        &cmd.provider_id,
        cmd.skip_wait,
        timeout_ms,
    )
    .await?;

    let text = if cmd.skip_wait {
        format!("Provider {} stop request received", &cmd.provider_id)
    } else {
        format!("Provider [{}] stopped successfully", &cmd.provider_id)
    };

    Ok(CommandOutput::new(
        text.clone(),
        HashMap::from([
            ("result".into(), text.into()),
            ("provider_id".into(), cmd.provider_id.into()),
            ("host_id".into(), cmd.host_id.into()),
        ]),
    ))
}

pub async fn stop_provider(
    client: &wasmcloud_control_interface::Client,
    host_id: Option<&str>,
    provider_id: &str,
    skip_wait: bool,
    timeout_ms: u64,
) -> Result<()> {
    let mut receiver = client
        .events_receiver(vec![
            "provider_stopped".to_string(),
            "provider_stop_failed".to_string(),
        ])
        .await
        .map_err(boxed_err_to_anyhow)?;

    let host_id = if let Some(host_id) = host_id {
        find_host_id(host_id, client).await?.0
    } else {
        find_host_with_provider(provider_id, client).await?
    };

    let ack = client
        .stop_provider(&host_id, provider_id)
        .await
        .map_err(boxed_err_to_anyhow)?;

    if !ack.succeeded() {
        bail!("Operation failed: {}", ack.message());
    }
    if skip_wait {
        return Ok(());
    }

    let event = wait_for_provider_stop_event(
        &mut receiver,
        Duration::from_millis(timeout_ms),
        host_id.to_string(),
        provider_id.to_string(),
    )
    .await?;

    match event {
        FindEventOutcome::Success(ProviderStoppedInfo { .. }) => Ok(()),
        FindEventOutcome::Failure(err) => bail!("{}", err),
    }
}
