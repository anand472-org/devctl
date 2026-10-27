use std::process::ExitCode;

use clap::Parser;
use devctl::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Telemetry is started per command by `run` (after the repository context
    // is open), so errors before that point are only printed here.
    match devctl::run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
