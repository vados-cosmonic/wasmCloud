use clap::Parser;

use crate::lib::cli::CliConnectionOpts;
use crate::lib::context::default_timeout_ms;

#[derive(Debug, Clone, Parser)]
pub struct StartComponentCommand {
    #[clap(flatten)]
    pub opts: CliConnectionOpts,

    /// Id of host or a string to match on the friendly name of a host. if omitted the component will be
    /// auctioned in the lattice to find a suitable host. If a string is supplied to match against,
    /// then the matching host ID will be used. If more than one host matches, then an error will be
    /// returned
    #[clap(long = "host-id")]
    pub host_id: Option<String>,

    /// Component reference, e.g. the absolute file path or OCI URL.
    #[clap(name = "component-ref")]
    pub component_ref: String,

    /// Unique ID to use for the component
    #[clap(name = "component-id", value_parser = validate_component_id)]
    pub component_id: String,

    /// Maximum number of instances this component can run concurrently.
    #[clap(
        long = "max-instances",
        alias = "max-concurrent",
        alias = "max",
        alias = "count",
        default_value_t = 1
    )]
    pub max_instances: u32,

    /// Constraints for component auction in the form of "label=value". If host-id is supplied, this list is ignored
    #[clap(short = 'c', long = "constraint", name = "constraints")]
    pub constraints: Option<Vec<String>>,

    /// Timeout to await an auction response, defaults to 2000 milliseconds
    #[clap(long = "auction-timeout-ms", default_value_t = default_timeout_ms())]
    pub auction_timeout_ms: u64,

    /// By default, the command will wait until the component has been started.
    /// If this flag is passed, the command will return immediately after acknowledgement from the host, without waiting for the component to start.
    /// If this flag is omitted, the timeout will be adjusted to 5 seconds to account for component download times
    #[clap(long = "skip-wait")]
    pub skip_wait: bool,

    /// List of named configuration to apply to the component, may be empty
    #[clap(long = "config")]
    pub config: Vec<String>,
}
