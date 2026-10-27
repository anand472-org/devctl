//! Tracing subscriber with rotated local log files (always on when a logs
//! directory is given) and optional OpenTelemetry export (OTLP over plain HTTP,
//! off by default).
//!
//! Privacy: nothing here records developer identity. File paths appear only in
//! events whose target is [`PATH_TARGET`]; every export layer drops those unless
//! `telemetry.include_paths` is set. The local log file keeps them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{TraceContextExt, TracerProvider};
use opentelemetry::{Context, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_opentelemetry::{MetricsLayer, OpenTelemetryLayer, OpenTelemetrySpanExt};
use tracing_subscriber::Registry;
use tracing_subscriber::filter::{EnvFilter, FilterExt, LevelFilter, filter_fn};
use tracing_subscriber::layer::{Layer, SubscriberExt};

use crate::config::TelemetryCfg;

/// Events that carry file paths use this target; export layers drop them unless
/// `include_paths` is set.
pub const PATH_TARGET: &str = "devctl::paths";

/// Number of daily log files kept per process.
const MAX_LOG_FILES: usize = 7;

/// Crates whose events must never reach the export layers: the export path
/// itself would otherwise report on exporting, in a loop. Everything starting
/// with `opentelemetry` (`opentelemetry_sdk`, `opentelemetry-otlp`, ...) is also
/// excluded.
const EXPORT_INTERNALS: [&str; 5] = ["reqwest", "hyper", "hyper_util", "h2", "tower"];

fn is_export_internal(target: &str) -> bool {
    target.starts_with("opentelemetry")
        || EXPORT_INTERNALS.iter().any(|t| {
            target
                .strip_prefix(t)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        })
}

/// What [`init`] needs to know.
pub struct Settings<'a> {
    /// Process name; prefixes the log file (`<process>.<date>.log`).
    pub process: &'a str,
    /// Where to write rotated log files; `None` for no local log.
    pub logs_dir: Option<&'a Path>,
    pub cfg: &'a TelemetryCfg,
    /// Upper bound on how long dropping the [`Guard`] may block on export.
    pub flush_timeout: Duration,
    /// Repository name for the `vcs.repository.name` resource attribute.
    pub repo: Option<&'a str>,
}

/// The three OpenTelemetry providers behind export. Tests pass in-memory ones
/// to [`init_with`] / [`build_with`]; build their resource with [`resource_with`].
pub struct Exporters {
    pub tracer: SdkTracerProvider,
    pub logger: SdkLoggerProvider,
    pub meter: SdkMeterProvider,
}

/// Flushes and shuts down export when dropped, and flushes the local log file.
/// Dropping never blocks longer than `flush_timeout`.
pub struct Guard {
    exporters: Option<Exporters>,
    flush_timeout: Duration,
    // Dropped after `exporters` (declaration order), flushing the log file last.
    _file: Option<WorkerGuard>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some(e) = self.exporters.take() else {
            return;
        };
        let timeout = self.flush_timeout;
        let (tx, rx) = mpsc::channel::<()>();
        // One thread per provider so a hanging one cannot starve the others.
        // Each shutdown flushes first; errors cannot be acted on here.
        let jobs: [Box<dyn FnOnce() + Send>; 3] = [
            Box::new(move || {
                let _ = e.tracer.shutdown_with_timeout(timeout);
            }),
            Box::new(move || {
                let _ = e.logger.shutdown_with_timeout(timeout);
            }),
            Box::new(move || {
                let _ = e.meter.shutdown_with_timeout(timeout);
            }),
        ];
        let mut started = 0;
        for job in jobs {
            let tx = tx.clone();
            let spawned = std::thread::Builder::new()
                .name("devctl-telemetry-flush".into())
                .spawn(move || {
                    job();
                    let _ = tx.send(());
                });
            match spawned {
                Ok(_) => started += 1,
                Err(err) => tracing::warn!("could not start telemetry flush thread: {err}"),
            }
        }
        drop(tx);
        let deadline = Instant::now() + timeout;
        for _ in 0..started {
            let left = deadline.saturating_duration_since(Instant::now());
            if rx.recv_timeout(left).is_err() {
                tracing::warn!(
                    timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    "telemetry flush timed out; abandoning unsent data"
                );
                break;
            }
        }
    }
}

/// Installs the global subscriber. Never fails: problems (unwritable log
/// directory, bad endpoint) become a warning in the log and telemetry carries on
/// without that part.
#[must_use]
pub fn init(s: &Settings) -> Guard {
    install(build_with(s, None))
}

/// Like [`init`] but with caller-supplied exporters (tests).
#[must_use]
pub fn init_with(s: &Settings, exporters: Exporters) -> Guard {
    install(build_with(s, Some(exporters)))
}

fn install((dispatch, guard): (tracing::Dispatch, Guard)) -> Guard {
    // Only the first call in a process wins; a second init keeps the first.
    let _ = tracing::dispatcher::set_global_default(dispatch);
    guard
}

/// Builds the subscriber without installing it. With `exporters: None`, export
/// is decided by [`export_endpoint`] and uses OTLP over HTTP.
#[must_use]
pub fn build_with(s: &Settings, exporters: Option<Exporters>) -> (tracing::Dispatch, Guard) {
    let mut warnings: Vec<String> = Vec::new();
    let exporters = exporters.or_else(|| {
        let endpoint = export_endpoint(s.cfg)?;
        match otlp_exporters(s, &endpoint) {
            Ok(e) => Some(e),
            Err(why) => {
                warnings.push(format!("telemetry export disabled: {why}"));
                None
            }
        }
    });

    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();
    let mut file_guard = None;

    if let Some(dir) = s.logs_dir {
        match Builder::new()
            .rotation(Rotation::DAILY)
            .filename_prefix(s.process)
            .filename_suffix("log")
            .max_log_files(MAX_LOG_FILES)
            .build(dir)
        {
            Ok(appender) => {
                let (writer, g) = tracing_appender::non_blocking(appender);
                file_guard = Some(g);
                layers.push(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(writer)
                        .with_filter(env_filter(&s.cfg.log_level))
                        .boxed(),
                );
            }
            Err(err) => warnings.push(format!(
                "cannot write logs in {}: {err}; continuing without a local log",
                dir.display()
            )),
        }
    }

    if let Some(e) = &exporters {
        let include_paths = s.cfg.include_paths;
        // Path and internals filtering applies to every export layer.
        let export_filter = move || {
            filter_fn(move |m| {
                (include_paths || m.target() != PATH_TARGET) && !is_export_internal(m.target())
            })
        };
        // `log_level` governs the log bridge only. Spans export at info and above;
        // metric events export at any level (the metrics layer reacts only to
        // `monotonic_counter.`/`histogram.`/`counter.` fields), so counters such
        // as the debug-level `store.busy_retries` survive any `log_level`.
        layers.push(
            OpenTelemetryLayer::new(e.tracer.tracer("devctl"))
                .with_filter(export_filter().and(LevelFilter::INFO))
                .boxed(),
        );
        layers.push(
            OpenTelemetryTracingBridge::new(&e.logger)
                .with_filter(env_filter(&s.cfg.log_level).and(export_filter()))
                .boxed(),
        );
        layers.push(
            MetricsLayer::new(e.meter.clone())
                .with_filter(export_filter())
                .boxed(),
        );
    }

    let dispatch = tracing::Dispatch::new(Registry::default().with(layers));
    tracing::dispatcher::with_default(&dispatch, || {
        for w in &warnings {
            tracing::warn!("{w}");
        }
    });
    (
        dispatch,
        Guard {
            exporters,
            flush_timeout: s.flush_timeout,
            _file: file_guard,
        },
    )
}

fn env_filter(level: &str) -> EnvFilter {
    EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Resource attributes: `service.name` (`OTEL_SERVICE_NAME`, default `devctl`),
/// `service.version`, and `vcs.repository.name` when known. `get` looks up
/// environment variables.
#[must_use]
pub fn resource_with(s: &Settings, get: impl Fn(&str) -> Option<String>) -> Resource {
    let name = get("OTEL_SERVICE_NAME")
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "devctl".to_string());
    let mut attrs = vec![
        KeyValue::new("service.name", name),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ];
    if let Some(repo) = s.repo {
        attrs.push(KeyValue::new("vcs.repository.name", repo.to_string()));
    }
    Resource::builder_empty().with_attributes(attrs).build()
}

/// Joins a base endpoint and an OTLP signal path (`v1/traces`, ...).
fn signal_url(base: &str, signal: &str) -> String {
    format!("{}/v1/{signal}", base.trim_end_matches('/'))
}

fn otlp_exporters(s: &Settings, endpoint: &str) -> Result<Exporters, String> {
    if !endpoint.starts_with("http://") {
        return Err(format!(
            "endpoint {endpoint:?} is not plain http:// (https is not supported); \
             set telemetry.endpoint to an http:// collector"
        ));
    }
    let timeout = s.flush_timeout;
    let resource = resource_with(s, |k| std::env::var(k).ok());
    let err = |what: &str, e: &dyn std::fmt::Display| format!("cannot build {what} exporter: {e}");

    let spans = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(signal_url(endpoint, "traces"))
        .with_timeout(timeout)
        .build()
        .map_err(|e| err("trace", &e))?;
    let logs = LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(signal_url(endpoint, "logs"))
        .with_timeout(timeout)
        .build()
        .map_err(|e| err("log", &e))?;
    let metrics = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(signal_url(endpoint, "metrics"))
        .with_timeout(timeout)
        .build()
        .map_err(|e| err("metric", &e))?;

    Ok(Exporters {
        tracer: SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(spans)
            .build(),
        logger: SdkLoggerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(logs)
            .build(),
        meter: SdkMeterProvider::builder()
            .with_resource(resource)
            .with_periodic_exporter(metrics)
            .build(),
    })
}

/// The OTLP endpoint to export to, if any: `telemetry.endpoint`, else
/// `OTEL_EXPORTER_OTLP_ENDPOINT`. `None` when both are empty or
/// `OTEL_SDK_DISABLED=true`.
#[must_use]
pub fn export_endpoint(cfg: &TelemetryCfg) -> Option<String> {
    export_endpoint_with(cfg, |k| std::env::var(k).ok())
}

/// [`export_endpoint`] with an injectable environment lookup.
pub fn export_endpoint_with(
    cfg: &TelemetryCfg,
    get: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if get("OTEL_SDK_DISABLED").is_some_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        return None;
    }
    let endpoint = if cfg.endpoint.trim().is_empty() {
        get("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_default()
    } else {
        cfg.endpoint.clone()
    };
    let endpoint = endpoint.trim();
    (!endpoint.is_empty()).then(|| endpoint.to_string())
}

/// The W3C `traceparent` header for the current span; `None` when export is off
/// (no valid trace context exists).
#[must_use]
pub fn traceparent() -> Option<String> {
    let cx = tracing::Span::current().context();
    if !cx.span().span_context().is_valid() {
        return None;
    }
    let mut carrier: HashMap<String, String> = HashMap::new();
    TraceContextPropagator::new().inject_context(&cx, &mut carrier);
    carrier.remove("traceparent")
}

/// Makes `span` a child of the trace named by the `TRACEPARENT` environment
/// variable, if set and valid.
pub fn adopt_traceparent(span: &tracing::Span) {
    adopt_traceparent_from(span, std::env::var("TRACEPARENT").ok().as_deref());
}

/// [`adopt_traceparent`] with the header passed in.
pub fn adopt_traceparent_from(span: &tracing::Span, header: Option<&str>) {
    let Some(header) = header.filter(|h| !h.is_empty()) else {
        return;
    };
    let carrier = HashMap::from([("traceparent".to_string(), header.to_string())]);
    let cx = TraceContextPropagator::new().extract_with_context(&Context::new(), &carrier);
    if cx.span().span_context().is_valid() {
        // Fails only when export is off (no OpenTelemetry layer): nothing to link.
        let _ = span.set_parent(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::KeyValue;
    use opentelemetry::Value;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing::{info, info_span};

    fn cfg(endpoint: &str, include_paths: bool) -> TelemetryCfg {
        TelemetryCfg {
            endpoint: endpoint.to_string(),
            include_paths,
            log_level: "info".to_string(),
        }
    }

    fn settings<'a>(c: &'a TelemetryCfg, dir: Option<&'a Path>) -> Settings<'a> {
        Settings {
            process: "cli",
            logs_dir: dir,
            cfg: c,
            flush_timeout: Duration::from_secs(2),
            repo: Some("demo"),
        }
    }

    struct Mem {
        spans: InMemorySpanExporter,
        logs: InMemoryLogExporter,
        metrics: InMemoryMetricExporter,
    }

    fn mem(s: &Settings) -> (Mem, Exporters) {
        let spans = InMemorySpanExporter::default();
        let logs = InMemoryLogExporter::default();
        let metrics = InMemoryMetricExporter::default();
        let res = resource_with(s, |_| None);
        let e = Exporters {
            tracer: SdkTracerProvider::builder()
                .with_resource(res.clone())
                .with_simple_exporter(spans.clone())
                .build(),
            logger: SdkLoggerProvider::builder()
                .with_resource(res.clone())
                .with_simple_exporter(logs.clone())
                .build(),
            meter: opentelemetry_sdk::metrics::SdkMeterProvider::builder()
                .with_resource(res)
                .with_reader(PeriodicReader::builder(metrics.clone()).build())
                .build(),
        };
        (
            Mem {
                spans,
                logs,
                metrics,
            },
            e,
        )
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn endpoint_resolution() {
        let env = |k: &str| (k == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://env:4318".into());
        assert_eq!(
            export_endpoint_with(&cfg("http://cfg:1", false), env).as_deref(),
            Some("http://cfg:1")
        );
        assert_eq!(
            export_endpoint_with(&cfg("", false), env).as_deref(),
            Some("http://env:4318")
        );
        assert_eq!(export_endpoint_with(&cfg("", false), no_env), None);
        let disabled = |k: &str| match k {
            "OTEL_SDK_DISABLED" => Some("true".to_string()),
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("http://env:4318".to_string()),
            _ => None,
        };
        assert_eq!(
            export_endpoint_with(&cfg("http://cfg:1", false), disabled),
            None
        );
    }

    #[test]
    fn spans_and_metrics_reach_exporter() {
        let c = cfg("", false);
        let s = settings(&c, None);
        let (m, e) = mem(&s);
        let (dispatch, guard) = build_with(&s, Some(e));
        tracing::dispatcher::with_default(&dispatch, || {
            let span = info_span!("gate.rule", rule = "tests");
            let _g = span.enter();
            info!(
                monotonic_counter.devctl.gate.result = 1_u64,
                rule = "tests",
                outcome = "pass"
            );
        });
        // Simple processors export synchronously; shutdown would clear them.
        let spans = m.spans.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "gate.rule");
        drop(guard);
        let finished = m.metrics.get_finished_metrics().unwrap();
        let metric = finished
            .iter()
            .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|x| x.name() == "devctl.gate.result")
            .expect("metric devctl.gate.result");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("expected u64 sum");
        };
        let points: Vec<_> = sum.data_points().collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].value(), 1);
        let attrs: Vec<&KeyValue> = points[0].attributes().collect();
        let has = |k: &str, v: &str| {
            attrs
                .iter()
                .any(|kv| kv.key.as_str() == k && kv.value.as_str() == v)
        };
        assert!(has("rule", "tests") && has("outcome", "pass"), "{attrs:?}");
    }

    #[test]
    fn service_name_from_env() {
        let c = cfg("", false);
        let s = settings(&c, None);
        let get = |r: &Resource, k: &str| {
            r.get(&k.to_string().into())
                .map(|v: Value| v.as_str().to_string())
        };
        let env = |k: &str| (k == "OTEL_SERVICE_NAME").then(|| "x".to_string());
        assert_eq!(
            get(&resource_with(&s, env), "service.name").as_deref(),
            Some("x")
        );
        let r = resource_with(&s, no_env);
        assert_eq!(get(&r, "service.name").as_deref(), Some("devctl"));
        assert_eq!(get(&r, "vcs.repository.name").as_deref(), Some("demo"));
    }

    #[test]
    fn paths_not_exported_unless_enabled() {
        for include in [false, true] {
            let c = cfg("", include);
            let s = settings(&c, None);
            let (m, e) = mem(&s);
            let (dispatch, guard) = build_with(&s, Some(e));
            tracing::dispatcher::with_default(&dispatch, || {
                info!(target: PATH_TARGET, path = "/secret/x", "wrote file");
                info!("plain event");
            });
            let logs = m.logs.get_emitted_logs().unwrap();
            drop(guard);
            let paths = logs
                .iter()
                .filter(|l| l.record.target().is_some_and(|t| t == PATH_TARGET))
                .count();
            assert_eq!(paths, usize::from(include), "include_paths={include}");
            assert_eq!(logs.len(), 1 + usize::from(include));
        }
    }

    #[test]
    fn traceparent_round_trips() {
        let c = cfg("", false);
        let s = settings(&c, None);
        let (_m, e) = mem(&s);
        let (dispatch, guard) = build_with(&s, Some(e));
        tracing::dispatcher::with_default(&dispatch, || {
            let parent = info_span!("hook");
            let header = parent.in_scope(traceparent).expect("traceparent");
            let child = info_span!("worker");
            adopt_traceparent_from(&child, Some(&header));
            let back = child.in_scope(traceparent).expect("child traceparent");
            // 00-<trace id>-<span id>-<flags>
            assert_eq!(header.split('-').nth(1), back.split('-').nth(1));
        });
        drop(guard);
    }

    #[test]
    fn traceparent_none_when_export_off() {
        let c = cfg("", false);
        let s = settings(&c, None);
        let (dispatch, guard) = build_with(&s, None);
        tracing::dispatcher::with_default(&dispatch, || {
            assert!(info_span!("x").in_scope(traceparent).is_none());
        });
        drop(guard);
    }

    #[test]
    fn local_log_file_written() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg("", false);
        let s = settings(&c, Some(dir.path()));
        let (dispatch, guard) = build_with(&s, None);
        tracing::dispatcher::with_default(&dispatch, || info!("hello log"));
        drop(guard);
        let found: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("cli."))
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        let text = std::fs::read_to_string(dir.path().join(&found[0])).unwrap();
        assert!(text.contains("hello log"), "{text}");
    }

    #[test]
    fn drop_does_not_block_past_flush_timeout() {
        use std::net::TcpListener;
        // A collector that accepts connections and never answers.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let c = cfg(&format!("http://127.0.0.1:{port}"), false);
        let mut s = settings(&c, None);
        s.flush_timeout = Duration::from_millis(500);
        let (dispatch, guard) = build_with(&s, None);
        tracing::dispatcher::with_default(&dispatch, || {
            info_span!("x").in_scope(|| info!("e"));
        });
        let t = std::time::Instant::now();
        drop(guard);
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "{:?}",
            t.elapsed()
        );
        drop(l);
    }

    #[test]
    fn export_internals_do_not_reach_log_exporter() {
        let c = cfg("", false);
        let s = settings(&c, None);
        let (m, e) = mem(&s);
        let (dispatch, guard) = build_with(&s, Some(e));
        tracing::dispatcher::with_default(&dispatch, || {
            info!(target: "opentelemetry_sdk", "internal");
            info!(target: "opentelemetry-otlp", "internal");
            info!(target: "hyper_util::client", "internal");
            info!(target: "reqwest::connect", "internal");
            info!("kept");
        });
        let logs = m.logs.get_emitted_logs().unwrap();
        drop(guard);
        assert_eq!(logs.len(), 1, "{:?}", logs.len());
    }

    #[test]
    fn log_level_governs_logs_not_spans_or_metrics() {
        let mut c = cfg("", false);
        c.log_level = "warn".to_string();
        let s = settings(&c, None);
        let (m, e) = mem(&s);
        let (dispatch, guard) = build_with(&s, Some(e));
        tracing::dispatcher::with_default(&dispatch, || {
            info_span!("work").in_scope(|| {
                info!(monotonic_counter.devctl.worker.cancelled = 1_u64);
                info!("info log");
                tracing::warn!("warn log");
            });
        });
        assert_eq!(m.spans.get_finished_spans().unwrap().len(), 1);
        assert_eq!(m.logs.get_emitted_logs().unwrap().len(), 1);
        drop(guard);
        let finished = m.metrics.get_finished_metrics().unwrap();
        assert!(
            finished
                .iter()
                .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
                .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
                .any(|x| x.name() == "devctl.worker.cancelled")
        );
    }

    #[test]
    fn drop_flushes_other_providers_when_one_hangs() {
        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::trace::{SpanData, SpanExporter};
        #[derive(Debug)]
        struct Hang;
        impl SpanExporter for Hang {
            fn export(&self, _b: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> {
                std::future::ready(Ok(()))
            }
            fn shutdown_with_timeout(&self, _t: Duration) -> OTelSdkResult {
                std::thread::sleep(Duration::from_secs(30));
                Ok(())
            }
        }
        let c0 = cfg("", false);
        let mut s = settings(&c0, None);
        s.flush_timeout = Duration::from_millis(500);
        let (mem_out, mut exp) = mem(&s);
        exp.tracer = SdkTracerProvider::builder()
            .with_simple_exporter(Hang)
            .build();
        let (dispatch, guard) = build_with(&s, Some(exp));
        tracing::dispatcher::with_default(&dispatch, || {
            info_span!("x").in_scope(|| info!(monotonic_counter.devctl.worker.cancelled = 1_u64));
        });
        let start = Instant::now();
        drop(guard);
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        // Metrics were still flushed even though tracing hung.
        assert!(!mem_out.metrics.get_finished_metrics().unwrap().is_empty());
    }

    #[test]
    fn debug_level_metric_events_are_exported() {
        let c = cfg("", false);
        let s = settings(&c, None);
        let (m, e) = mem(&s);
        let (dispatch, guard) = build_with(&s, Some(e));
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::debug!(monotonic_counter.devctl.store.busy_retries = 1_u64);
        });
        drop(guard);
        let finished = m.metrics.get_finished_metrics().unwrap();
        assert!(
            finished
                .iter()
                .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
                .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
                .any(|x| x.name() == "devctl.store.busy_retries")
        );
    }
}
