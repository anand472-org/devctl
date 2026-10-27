use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use devctl::cli::Cli;
use devctl::config::TelemetryCfg;
use devctl::telemetry;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Minimal telemetry wiring: export only when OTEL_EXPORTER_OTLP_ENDPOINT is
    // set. Per-command settings (repo config, logs dir, process names) come with
    // the command wiring.
    let cfg = TelemetryCfg {
        endpoint: String::new(),
        include_paths: false,
        log_level: "info".to_string(),
    };
    let _telemetry = telemetry::init(&telemetry::Settings {
        process: "cli",
        logs_dir: None,
        cfg: &cfg,
        flush_timeout: Duration::from_secs(2),
        repo: None,
    });
    let result = {
        let span = tracing::info_span!("devctl.cli");
        let _entered = span.enter();
        devctl::run(&cli)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
