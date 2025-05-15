use clap::Parser;

mod component;
mod provider;

#[derive(Debug, Clone, Parser)]
pub enum StartCommand {
    /// Launch a component in a host
    #[clap(name = "component")]
    Component(component::StartComponentCommand),

    /// Launch a provider in a host
    #[clap(name = "provider")]
    Provider(provider::StartProviderCommand),
}
