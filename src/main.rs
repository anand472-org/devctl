use std::process::ExitCode;

use clap::Parser;
use devctl::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match devctl::run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
