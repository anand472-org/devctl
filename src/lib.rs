pub mod cli;

use anyhow::Result;

use cli::{Cli, Command};

/// Dispatches a parsed command line to its subcommand handler.
///
/// # Errors
///
/// Returns an error if the subcommand fails.
pub fn run(cli: &Cli) -> Result<()> {
    match cli.command {
        Command::Version => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
