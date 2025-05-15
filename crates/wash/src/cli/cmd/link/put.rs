//! Functionality enabling the `wash link put` subcommand

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use serde_json::json;
use wasmcloud_control_interface::Link;

use crate::lib::cli::CliConnectionOpts;

use crate::appearance::spinner::Spinner;
use crate::lib::cli::link::{put_link, LinkPutCommand};
use crate::lib::cli::{CommandOutput, OutputKind};

#[derive(Parser, Debug, Clone)]
pub struct LinkPutCommand {
    #[clap(flatten)]
    pub opts: CliConnectionOpts,

    /// The ID of the component to link from
    #[clap(name = "source-id", value_parser = validate_component_id)]
    pub source_id: String,

    /// The ID of the component to link to
    #[clap(name = "target", value_parser = validate_component_id)]
    pub target: String,

    /// The WIT namespace of the link, e.g. "wasi" in "wasi:http/incoming-handler"
    #[clap(name = "wit-namespace")]
    pub wit_namespace: String,

    /// The WIT package of the link, e.g. "http" in "wasi:http/incoming-handler"
    #[clap(name = "wit-package")]
    pub wit_package: String,

    /// The interface of the link, e.g. "incoming-handler" in "wasi:http/incoming-handler"
    #[clap(long = "interface", alias = "interfaces", required = true)]
    pub interfaces: Vec<String>,

    /// List of named configuration to make available to the source
    #[clap(long = "source-config")]
    pub source_config: Vec<String>,

    /// List of named configuration to make available to the target
    #[clap(long = "target-config")]
    pub target_config: Vec<String>,

    /// Link name, defaults to "default". Used for scenarios where a single source
    /// may have multiple links to the same target, or different targets with the same
    /// WIT namespace, package, and interface.
    #[clap(short = 'l', long = "link-name")]
    pub link_name: Option<String>,
}

/// Invoke `wash link put` subcommand
pub async fn invoke(
    LinkPutCommand {
        opts,
        source_id,
        target,
        link_name,
        wit_namespace,
        wit_package,
        interfaces,
        source_config,
        target_config,
    }: LinkPutCommand,
    output_kind: OutputKind,
) -> Result<CommandOutput> {
    let sp: Spinner = Spinner::new(&output_kind)?;
    sp.update_spinner_message(format!("Defining link {source_id} -> {target} ... ",));

    let name = link_name.unwrap_or_else(|| "default".to_string());

    let failure = put_link(
        opts.try_into()?,
        Link::builder()
            .source_id(&source_id)
            .target(&target)
            .name(&name)
            .wit_namespace(&wit_namespace)
            .wit_package(&wit_package)
            .interfaces(interfaces)
            .source_config(source_config)
            .target_config(target_config)
            .build()
            .map_err(|e| anyhow!(e).context("failed to build link"))?,
    )
    .await
    .map_or_else(
        |e| Some(format!("{e}")),
        // If the operation was unsuccessful, return the error message
        |ctl_response| (!ctl_response.succeeded()).then_some(ctl_response.message().to_string()),
    );

    link_put_output(&source_id, &target, failure)
}

/// Generate output for `wash link put` command
fn link_put_output(
    source_id: impl AsRef<str>,
    target: impl AsRef<str>,
    failure: Option<String>,
) -> Result<CommandOutput> {
    let source_id = source_id.as_ref();
    let target = target.as_ref();
    match failure {
        None => {
            let mut map = HashMap::new();
            map.insert("source_id".to_string(), json!(source_id));
            map.insert("target".to_string(), json!(target));
            Ok(CommandOutput::new(
                format!("Published link ({source_id}) -> ({target}) successfully"),
                map,
            ))
        }
        Some(f) => bail!("Error putting link: {f}"),
    }
}
