use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use tokio::time::Duration;
use tracing::error;
use wasmcloud_control_interface::HostInventory;

use crate::lib::cli::{CliConnectionOpts, CommandOutput};
use crate::lib::common::{
    boxed_err_to_anyhow, find_host_id, get_all_inventories, FindIdError, Match,
};
use crate::lib::component::{scale_component, ComponentScaledInfo, ScaleComponentArgs};
use crate::lib::config::{host_pid_file, WashConnectionOptions};
use crate::lib::context::default_timeout_ms;
use crate::lib::id::ServerId;
use crate::lib::wait::{wait_for_provider_stop_event, FindEventOutcome, ProviderStoppedInfo};

use super::validate_component_id;

#[derive(Debug, Clone, Parser)]
pub struct StopHostCommand {
    #[clap(flatten)]
    pub opts: CliConnectionOpts,

    /// Id of host to stop. If a non-ID is provided, the host will be selected based on matching the
    /// prefix of the ID or the friendly name and will return an error if more than one host
    /// matches.
    #[clap(name = "host-id")]
    pub host_id: String,

    /// The timeout in ms for how much time to give the host for graceful shutdown
    #[clap(
        long = "host-timeout",
        default_value_t = default_timeout_ms()
    )]
    pub host_shutdown_timeout: u64,
}

async fn find_host_with_provider(
    provider_id: &str,
    ctl_client: &wasmcloud_control_interface::Client,
) -> Result<ServerId, FindIdError> {
    find_host_with_filter(ctl_client, |inv| {
        inv.providers()
            .iter()
            .any(|prov| prov.id() == provider_id)
            .then_some((inv.host_id().to_string(), inv.friendly_name().to_string()))
            .and_then(|(id, friendly_name)| id.parse().ok().map(|i| (i, friendly_name)))
    })
    .await
}

async fn find_host_with_filter<F>(
    ctl_client: &wasmcloud_control_interface::Client,
    filter: F,
) -> Result<ServerId, FindIdError>
where
    F: FnMut(HostInventory) -> Option<(ServerId, String)>,
{
    let inventories = get_all_inventories(ctl_client).await?;
    let all_matching = inventories
        .into_iter()
        .filter_map(filter)
        .collect::<Vec<(ServerId, String)>>();

    if all_matching.is_empty() {
        Err(FindIdError::NoMatches)
    } else if all_matching.len() > 1 {
        Err(FindIdError::MultipleMatches(
            all_matching
                .into_iter()
                .map(|(id, friendly_name)| Match {
                    id: id.into_string(),
                    friendly_name: Some(friendly_name),
                })
                .collect(),
        ))
    } else {
        // SAFETY: We know there is exactly one match at this point
        Ok(all_matching.into_iter().next().unwrap().0)
    }
}

/// Stop running wasmCloud hosts, returns a vector of host IDs that were stopped and
/// a boolean indicating whether any hosts remain running
pub async fn stop_hosts(
    client: wasmcloud_control_interface::client::Client,
    host_id: Option<&String>,
    all: bool,
) -> Result<(Vec<String>, bool)> {
    let hosts = client
        .get_hosts()
        .await
        .map_err(|e| anyhow!(e))?
        .into_iter()
        .filter_map(wasmcloud_control_interface::CtlResponse::into_data)
        .collect::<Vec<_>>();

    // If a host ID was supplied, stop only that host
    if let Some(host_id) = host_id {
        let host_id_string = host_id.to_string();
        client.stop_host(&host_id_string, None).await.map_err(|e| {
            anyhow!(
                "Could not stop host, ensure a host with that ID is running: {:?}",
                e
            )
        })?;

        Ok((vec![host_id_string], hosts.len() > 1))
    } else if hosts.is_empty() {
        Ok((vec![], false))
    } else if hosts.len() == 1 {
        let host_id = hosts[0].id();
        client
            .stop_host(host_id, None)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok((vec![host_id.to_string()], false))
    } else if all {
        let host_stops = hosts
            .iter()
            .map(|host| async {
                let host_id = host.id();
                match client.stop_host(host_id, None).await {
                    Ok(_) => Some(host_id.to_owned()),
                    Err(e) => {
                        error!("Could not stop host {}: {:?}", host_id, e);
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        let all_stops = futures::future::join_all(host_stops).await;
        let host_ids = all_stops
            .iter()
            // Remove any host IDs that ran into errors
            .filter_map(std::borrow::ToOwned::to_owned)
            .collect::<Vec<_>>();
        let hosts_remaining = all_stops.len() > host_ids.len();

        Ok((host_ids, hosts_remaining))
    } else {
        let running_hosts = hosts
            .into_iter()
            .map(|h| h.id().to_string())
            .collect::<Vec<_>>();
        bail!(
            "More than one host is running, please specify a host ID or use --all\nRunning hosts: {running_hosts:?}", 
        )
    }
}
