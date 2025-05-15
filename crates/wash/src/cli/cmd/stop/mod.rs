use clap::Parser;

mod component;
mod host;
mod provider;

#[derive(Debug, Clone, Parser)]
pub enum StopCommand {
    /// Stop a component running in a host
    #[clap(name = "component")]
    Component(component::StopComponentCommand),

    /// Stop a provider running in a host
    #[clap(name = "provider")]
    Provider(provider::StopProviderCommand),

    /// Purge and stop a running host
    #[clap(name = "host")]
    Host(host::StopHostCommand),
}
