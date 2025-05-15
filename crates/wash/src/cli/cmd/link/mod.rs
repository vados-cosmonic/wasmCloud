//! Functionality enabling the `wash link` group of subcommands

use anyhow::Result;

use clap::Parser;

use crate::cli::{CommandOutput, OutputKind};

mod del;
mod put;
mod query;

#[derive(Debug, Clone, Parser)]
pub enum LinkCommand {
    /// Query all links, same as `wash get links`
    #[clap(name = "query", alias = "get")]
    Query(query::LinkQueryCommand),

    /// Put a link from a source to a target on a given WIT interface
    #[clap(name = "put")]
    Put(put::LinkPutCommand),

    /// Delete a link
    #[clap(name = "del", alias = "delete")]
    Del(del::LinkDelCommand),
}

/// Invoke `wash link` subcommand
pub async fn invoke(command: LinkCommand, output_kind: OutputKind) -> Result<CommandOutput> {
    match command {
        LinkCommand::Del(cmd) => del::invoke(cmd, output_kind).await,
        LinkCommand::Put(cmd) => put::invoke(cmd, output_kind).await,
        LinkCommand::Query(cmd) => query::invoke(cmd, output_kind).await,
    }
}
