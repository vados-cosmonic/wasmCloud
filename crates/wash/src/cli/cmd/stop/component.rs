
#[derive(Debug, Clone, Parser)]
pub struct StopComponentCommand {
    #[clap(flatten)]
    pub opts: CliConnectionOpts,

    /// Id of host to stop component on. If a non-ID is provided, the host will be selected based
    /// on matching the prefix of the ID or the friendly name and will return an error if more than
    /// one host matches. If no host ID is passed, a host will be selected based on whether or not
    /// the component is running on it. If more than 1 host is running this component, an error will be
    /// returned with a list of hosts running the component
    #[clap(long = "host-id")]
    pub host_id: Option<String>,

    /// Unique component Id or a string to match on the prefix of the ID. If multiple components are matched, then an error
    /// will be returned with a list of all matching options
    #[clap(name = "component-id", value_parser = validate_component_id)]
    pub component_id: String,

    /// By default, the command will wait until the component has been stopped.
    /// If this flag is passed, the command will return immediately after acknowledgement from the host, without waiting for the component to stp[].
    #[clap(long = "skip-wait")]
    pub skip_wait: bool,
}

pub async fn handle_stop_component(cmd: StopComponentCommand) -> Result<CommandOutput> {
    let timeout_ms = cmd.opts.timeout_ms;
    let wco: WashConnectionOptions = cmd.opts.try_into()?;
    let client = wco.into_ctl_client(None).await?;

    let component_id = cmd.component_id;

    let inventory = if let Some(host_id) = cmd.host_id {
        client
            .get_host_inventory(&host_id)
            .await
            .map(wasmcloud_control_interface::CtlResponse::into_data)
            .map_err(boxed_err_to_anyhow)?
            .context("Supplied host did not respond to inventory query")?
    } else {
        let inventories = get_all_inventories(&client).await?;
        inventories
            .into_iter()
            .find(|inv| {
                inv.components()
                    .iter()
                    .any(|component| component.id() == component_id)
            })
            .ok_or_else(|| anyhow::anyhow!("No host found running component [{}]", component_id))?
    };

    let Some((host_id, component_ref)) = inventory
        .components()
        .iter()
        .find(|component| component.id() == component_id)
        .map(|component| {
            (
                inventory.host_id().to_string(),
                component.image_ref().to_string(),
            )
        })
    else {
        bail!(
            "No component with id [{component_id}] found on host [{}]",
            inventory.host_id()
        );
    };

    let ComponentScaledInfo {
        component_id,
        host_id,
        ..
    } = scale_component(ScaleComponentArgs {
        client: &client,
        host_id: &host_id,
        component_id: &component_id,
        component_ref: &component_ref,
        max_instances: 0,
        annotations: None,
        config: vec![],
        skip_wait: cmd.skip_wait,
        timeout_ms: Some(timeout_ms),
    })
    .await?;

    let text = if cmd.skip_wait {
        format!("Request to stop component [{component_id}] received",)
    } else {
        format!("Component [{component_id}] stopped")
    };

    Ok(CommandOutput::new(
        text.clone(),
        HashMap::from([
            ("result".into(), text.into()),
            ("component_id".into(), component_id.into()),
            ("host_id".into(), host_id.into()),
        ]),
    ))
}

pub async fn stop_host(cmd: StopHostCommand) -> Result<CommandOutput> {
    let wco: WashConnectionOptions = cmd.opts.try_into()?;
    let client = wco.into_ctl_client(None).await?;

    let (_, hosts_remain) = stop_hosts(client, Some(&cmd.host_id), false).await?;
    let pid_file_exists = tokio::fs::try_exists(host_pid_file()?).await?;
    if !hosts_remain && pid_file_exists {
        tokio::fs::remove_file(host_pid_file()?).await?;
    }

    Ok(CommandOutput::from_key_and_text(
        "result",
        format!("Host {} acknowledged stop request", cmd.host_id),
    ))
}
