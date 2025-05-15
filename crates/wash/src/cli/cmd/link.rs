use anyhow::{Context, Result};
use clap::Parser;
use wasmcloud_control_interface::{CtlResponse, Link};

use crate::lib::{
    cli::CliConnectionOpts, common::boxed_err_to_anyhow, config::WashConnectionOptions,
};

use super::validate_component_id;



/// Query links for a given Wash instance
///
/// # Arguments
///
/// * `wco` - Options for connecting to wash
///
/// # Examples
///
/// ```no_run
/// # use wash::lib::cli::link::get_links;
/// use wash::lib::config::WashConnectionOptions;
/// # async fn doc() -> anyhow::Result<()> {
/// let links = get_links(WashConnectionOptions::default()).await?;
/// println!("{links:?}");
/// # anyhow::Ok(())
/// # }
/// ```
pub async fn get_links(wco: WashConnectionOptions) -> Result<Vec<Link>> {
    wco.into_ctl_client(None)
        .await?
        .get_links()
        .await
        .map(|ctl| ctl.into_data().unwrap_or_default())
        .map_err(boxed_err_to_anyhow)
}

/// Delete a single link
///
/// # Arguments
///
/// * `wco` - Options for connecting to wash
/// * `source_id` - The ID of the source attached to the link
/// * `link_name` - The link name of the link ('default')
/// * `wit_namespace` - The WIT namespace of the link
/// * `wit_package` - The WIT package of the link
///
/// # Examples
///
/// ```no_run
/// # use wash::lib::cli::link::delete_link;
/// use wash::lib::config::WashConnectionOptions;
/// # async fn doc() -> anyhow::Result<()> {
/// let ack = delete_link(
///   WashConnectionOptions::default(),
///   "httpserver",
///   "default",
///   "wasi",
///   "http",
/// ).await?;
/// assert!(ack.succeeded());
/// # anyhow::Ok(())
/// # }
/// ```
pub async fn delete_link(
    wco: WashConnectionOptions,
    source_id: &str,
    link_name: &str,
    wit_namespace: &str,
    wit_package: &str,
) -> Result<CtlResponse<()>> {
    let ctl_client = wco.into_ctl_client(None).await?;
    ctl_client
        .delete_link(source_id, link_name, wit_namespace, wit_package)
        .await
        .map_err(boxed_err_to_anyhow)
        .with_context(|| {
            format!(
                "Failed to remove link from {source_id} on {wit_namespace}:{wit_package} with link name {link_name}",
            )
        })
}

/// Put a new link
///
/// # Arguments
///
/// * `wco` - Options for connecting to wash
/// * `link` - The [`wasmcloud_control_interface::Link`] to create
///
/// # Examples
///
/// ```no_run
/// # use wash::lib::cli::link::put_link;
/// use wash::lib::config::WashConnectionOptions;
/// use wasmcloud_control_interface::Link;
/// # async fn doc() -> anyhow::Result<()> {
/// let ack = put_link(
///     WashConnectionOptions::default(),
///     Link::builder()
///         .source_id("httpserver")
///         .target("echo")
///         .wit_namespace("wasi")
///         .wit_package("http")
///         .name("default")
///         .interfaces(vec!["incoming-handler".to_string()])
///         .build()
///         .unwrap(),
/// )
/// .await?;
/// assert!(ack.succeeded());
/// # anyhow::Ok(())
/// # }
/// ```
pub async fn put_link(wco: WashConnectionOptions, link: Link) -> Result<CtlResponse<()>> {
    let ctl_client = wco.into_ctl_client(None).await?;
    ctl_client
        .put_link(link.clone())
        .await
        .map_err(boxed_err_to_anyhow)
        .with_context(|| {
            format!(
                "Failed to create link between {} and {} on {}:{}/{:?}. Link name: {}",
                link.source_id(),
                link.target(),
                link.wit_namespace(),
                link.wit_package(),
                link.interfaces(),
                link.name()
            )
        })
}
