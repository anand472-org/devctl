use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "devctl", version, about, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the devctl version
    Version,
    /// Open the repository context and telemetry, record one event, and exit.
    /// Internal: lets tests exercise the repository-bound command path.
    #[command(hide = true)]
    Probe,
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
