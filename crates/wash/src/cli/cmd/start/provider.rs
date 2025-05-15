use clap::Parser;
use anyhow::bail;

use crate::lib::cli::CliConnectionOpts;
use crate::lib::config::{
    WashConnectionOptions, DEFAULT_NATS_TIMEOUT_MS, DEFAULT_START_PROVIDER_TIMEOUT_MS,
};
use crate::lib::context::default_timeout_ms;

#[derive(Debug, Clone, Parser)]
pub struct StartProviderCommand {
    #[clap(flatten)]
    pub opts: CliConnectionOpts,

    /// Id of host or a string to match on the friendly name of a host. if omitted the provider will
    /// be auctioned in the lattice to find a suitable host. If a string is supplied to match
    /// against, then the matching host ID will be used. If more than one host matches, then an
    /// error will be returned
    #[clap(long = "host-id")]
    pub host_id: Option<String>,

    /// Provider reference, e.g. the OCI URL for the provider
    #[clap(name = "provider-ref")]
    pub provider_ref: String,

    /// Unique provider ID to use for the provider
    #[clap(name = "provider-id", value_parser = validate_component_id)]
    pub provider_id: String,

    /// Link name of provider
    #[clap(short = 'l', long = "link-name", default_value = "default")]
    pub link_name: String,

    /// Constraints for provider auction in the form of "label=value". If host-id is supplied, this list is ignored
    #[clap(short = 'c', long = "constraint", name = "constraints")]
    pub constraints: Option<Vec<String>>,

    /// Timeout to await an auction response, defaults to 2000 milliseconds
    #[clap(long = "auction-timeout-ms", default_value_t = default_timeout_ms())]
    pub auction_timeout_ms: u64,

    /// List of named configuration to apply to the provider, may be empty
    #[clap(long = "config")]
    pub config: Vec<String>,

    /// By default, the command will wait until the provider has been started.
    /// If this flag is passed, the command will return immediately after acknowledgement from the host, without waiting for the provider to start.
    /// If this flag is omitted, the timeout will be adjusted to 30 seconds to account for provider download times
    #[clap(long = "skip-wait")]
    pub skip_wait: bool,
}

pub async fn handle_start_provider(cmd: StartProviderCommand) -> Result<CommandOutput> {
    // If timeout isn't supplied, override with a longer timeout for starting provider
    let timeout_ms = if cmd.opts.timeout_ms == DEFAULT_NATS_TIMEOUT_MS {
        DEFAULT_START_PROVIDER_TIMEOUT_MS
    } else {
        cmd.opts.timeout_ms
    };
    let client = <CliConnectionOpts as TryInto<WashConnectionOptions>>::try_into(cmd.opts)?
        .into_ctl_client(Some(cmd.auction_timeout_ms))
        .await?;

    // Attempt to parse the provider_ref from strings that may look like paths or be OCI references
    let provider_ref = resolve_ref(&cmd.provider_ref).await?;

    let host = if let Some(host) = cmd.host_id {
        find_host_id(&host, &client).await?.0
    } else {
        let suitable_hosts = client
            .perform_provider_auction(
                &provider_ref,
                &cmd.link_name,
                BTreeMap::from_iter(input_vec_to_hashmap(cmd.constraints.unwrap_or_default())?),
            )
            .await
            .map_err(boxed_err_to_anyhow)
            .with_context(|| {
                format!(
                    "Failed to auction provider {} with link name {} to hosts in lattice",
                    &provider_ref, &cmd.link_name
                )
            })?;
        if suitable_hosts.is_empty() {
            bail!("No suitable hosts found for provider {}", provider_ref);
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

    let mut receiver = client
        .events_receiver(vec![
            "provider_started".to_string(),
            "provider_start_failed".to_string(),
        ])
        .await
        .map_err(boxed_err_to_anyhow)
        .context("Failed to get lattice event channel")?;

    let ack = client
        .start_provider(&host, &provider_ref, &cmd.provider_id, None, cmd.config)
        .await
        .map_err(boxed_err_to_anyhow)
        .with_context(|| {
            format!(
                "Failed to start provider {} on host {:?}",
                &cmd.provider_id, &host
            )
        })?;

    if !ack.succeeded() {
        bail!("Start provider ack not accepted: {}", ack.message());
    }

    if cmd.skip_wait {
        let text = format!("Start provider request received: {}", &provider_ref);
        return Ok(CommandOutput::new(
            text.clone(),
            HashMap::from([
                ("result".into(), text.into()),
                ("provider_ref".into(), provider_ref.into()),
                ("link_name".into(), cmd.link_name.into()),
                ("host_id".into(), host.to_string().into()),
            ]),
        ));
    }

    let event = wait_for_provider_start_event(
        &mut receiver,
        Duration::from_millis(timeout_ms),
        host.to_string(),
        provider_ref.clone(),
    )
    .await
    .with_context(|| {
        format!(
            "Timed out waiting for start event for provider {} on host {}",
            &provider_ref, &host
        )
    })?;

    match event {
        FindEventOutcome::Success(ProviderStartedInfo {
            provider_id,
            provider_ref,
            host_id,
        }) => {
            let text = format!(
                "Provider [{}] (ref: [{}]) started on host [{}]",
                &provider_id, &provider_ref, &host_id
            );
            Ok(CommandOutput::new(
                text.clone(),
                HashMap::from([
                    ("result".into(), text.into()),
                    ("provider_ref".into(), provider_ref.into()),
                    ("provider_id".into(), provider_id.into()),
                    ("host_id".into(), host_id.into()),
                ]),
            ))
        }
        FindEventOutcome::Failure(err) => Err(err).with_context(|| {
            format!(
                "Failed starting provider {} on host {}",
                &provider_ref, &host
            )
        }),
    }
}
