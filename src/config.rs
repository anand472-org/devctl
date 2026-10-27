//! Loading and validating `.devctl.toml` (policy only).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use regex::Regex;
use serde::Deserialize;

/// Branch naming and protection policy.
#[derive(Debug, Clone)]
pub struct Branches {
    pub protected: Vec<String>,
    pub pattern: Regex,
    pub targets: BTreeMap<String, String>,
}

/// Pre-push gate thresholds.
#[derive(Debug, Clone)]
pub struct Gate {
    pub new_code_coverage: u8,
}

/// Per-commit size limits: files <= `max_files`; added + removed < `max_lines`.
#[derive(Debug, Clone)]
pub struct CommitLimits {
    pub max_files: u32,
    pub max_lines: u32,
}

/// Recording settings.
#[derive(Debug, Clone)]
pub struct RecordCfg {
    pub timeout_minutes: u32,
}

/// How much history is kept.
#[derive(Debug, Clone)]
pub struct Retention {
    pub records_per_branch: usize,
    pub raw_reports_days: u32,
}

/// Telemetry settings.
#[derive(Debug, Clone)]
pub struct TelemetryCfg {
    pub endpoint: String,
    pub include_paths: bool,
    pub log_level: String,
}

/// The validated contents of `.devctl.toml`.
#[derive(Debug, Clone)]
pub struct Config {
    pub branches: Branches,
    pub gate: Gate,
    pub commit: CommitLimits,
    pub record: RecordCfg,
    pub retention: Retention,
    pub telemetry: TelemetryCfg,
}

/// Log levels accepted for `telemetry.log_level` (matched case-insensitively,
/// stored lowercase).
const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

const DEFAULT_PATTERN: &str = "^(feature|bugfix|release|hotfix|support)/[a-z0-9][a-z0-9._-]*$";
const DEFAULT_TARGETS: [(&str, &str); 5] = [
    ("feature", "dev"),
    ("bugfix", "dev"),
    ("release", "master"),
    ("hotfix", "master"),
    ("support", "master"),
];

// Raw, as-written shapes. Every field is optional; validation fills defaults.
// The coverage threshold is `i64` so that out-of-range values get a
// key-specific message instead of a generic type error.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    branches: RawBranches,
    gate: RawGate,
    commit: RawCommit,
    record: RawRecord,
    retention: RawRetention,
    telemetry: RawTelemetry,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawBranches {
    protected: Option<Vec<String>>,
    pattern: Option<String>,
    targets: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawGate {
    new_code_coverage: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawCommit {
    max_files: Option<u32>,
    max_lines: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawRecord {
    timeout_minutes: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawRetention {
    records_per_branch: Option<usize>,
    raw_reports_days: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawTelemetry {
    endpoint: Option<String>,
    include_paths: Option<bool>,
    log_level: Option<String>,
}

fn invalid(key: &str, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("invalid .devctl.toml: {key}: {reason}")
}

fn at_least_one(key: &str, value: Option<u32>, default: u32) -> Result<u32> {
    match value.unwrap_or(default) {
        0 => Err(invalid(key, "must be at least 1, got 0")),
        n => Ok(n),
    }
}

impl RawConfig {
    fn validate(self) -> Result<Config> {
        let pattern_src = self.branches.pattern.as_deref().unwrap_or(DEFAULT_PATTERN);
        let pattern = Regex::new(pattern_src).map_err(|e| {
            let reason = e
                .to_string()
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
            invalid("branches.pattern", reason)
        })?;
        let protected = self
            .branches
            .protected
            .unwrap_or_else(|| vec!["master".into(), "dev".into()]);
        let targets = self.branches.targets.unwrap_or_else(|| {
            DEFAULT_TARGETS
                .iter()
                .map(|&(k, v)| (k.to_owned(), v.to_owned()))
                .collect()
        });

        let coverage = self.gate.new_code_coverage.unwrap_or(95);
        let new_code_coverage = u8::try_from(coverage)
            .ok()
            .filter(|n| *n <= 100)
            .ok_or_else(|| {
                invalid(
                    "gate.new_code_coverage",
                    format!("must be 0–100, got {coverage}"),
                )
            })?;

        let level = self.telemetry.log_level.unwrap_or_else(|| "info".into());
        let log_level = level.to_lowercase();
        if !LOG_LEVELS.contains(&log_level.as_str()) {
            return Err(invalid(
                "telemetry.log_level",
                format!("must be one of {}, got \"{level}\"", LOG_LEVELS.join(", ")),
            ));
        }

        Ok(Config {
            branches: Branches {
                protected,
                pattern,
                targets,
            },
            gate: Gate { new_code_coverage },
            commit: CommitLimits {
                max_files: at_least_one("commit.max_files", self.commit.max_files, 10)?,
                max_lines: at_least_one("commit.max_lines", self.commit.max_lines, 500)?,
            },
            record: RecordCfg {
                timeout_minutes: at_least_one(
                    "record.timeout_minutes",
                    self.record.timeout_minutes,
                    30,
                )?,
            },
            retention: Retention {
                records_per_branch: self.retention.records_per_branch.unwrap_or(200),
                raw_reports_days: self.retention.raw_reports_days.unwrap_or(90),
            },
            telemetry: TelemetryCfg {
                endpoint: self.telemetry.endpoint.unwrap_or_default(),
                include_paths: self.telemetry.include_paths.unwrap_or(false),
                log_level,
            },
        })
    }
}

/// The `[table]` name if `line` is a plain table header.
fn header(line: &str) -> Option<String> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    (!inner.starts_with('[') && !inner.ends_with(']')).then(|| inner.trim().to_owned())
}

/// Best-effort dotted key for a toml error: the `key =` on the error's line,
/// or a `[table]` header on that line, qualified by the nearest header above.
fn key_at(text: &str, offset: usize) -> Option<String> {
    let offset = offset.min(text.len());
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
    let line = text[line_start..line_end].trim();
    if let Some(h) = header(line) {
        return Some(h);
    }
    let (key, _) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let section = text[..line_start].lines().rev().find_map(header);
    Some(section.map_or_else(|| key.to_owned(), |s| format!("{s}.{key}")))
}

fn describe(text: &str, e: &toml::de::Error) -> anyhow::Error {
    let msg = e.message().trim();
    let Some(span) = e.span() else {
        return anyhow!("invalid .devctl.toml: {msg}");
    };
    let before = &text[..span.start.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    let place = format!("line {line}, column {col}");
    match key_at(text, span.start) {
        Some(key) => anyhow!("invalid .devctl.toml: {key}: {msg} ({place})"),
        None => anyhow!("invalid .devctl.toml: {place}: {msg}"),
    }
}

impl Default for Config {
    fn default() -> Self {
        RawConfig::default()
            .validate()
            .expect("built-in defaults are valid")
    }
}

impl Config {
    /// Reads `<worktree_root>/.devctl.toml`. A missing file means defaults.
    ///
    /// # Errors
    ///
    /// Fails if the file exists but cannot be read (permissions, it is a
    /// directory, not UTF-8), or if its contents are invalid.
    pub fn load(worktree_root: &Path) -> Result<Config> {
        let path = worktree_root.join(".devctl.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "cannot read {}; fix its permissions or remove it",
                path.display()
            ))),
        }
    }

    /// Parses and validates `.devctl.toml` text. LF and CRLF both work.
    ///
    /// # Errors
    ///
    /// Fails with `invalid .devctl.toml: <key>: <reason>` on a syntax error, an
    /// unknown key, a wrong type, or a value out of range.
    pub fn parse(text: &str) -> Result<Config> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let raw: RawConfig = toml::from_str(text).map_err(|e| describe(text, &e))?;
        raw.validate()
    }

    /// The target branch for `branch`, looked up by the prefix before the first
    /// `/`. `None` if there is no `/` or no target for that prefix.
    #[must_use]
    pub fn target_for(&self, branch: &str) -> Option<&str> {
        let (prefix, _) = branch.split_once('/')?;
        self.branches.targets.get(prefix).map(String::as_str)
    }

    /// Whether `branch` is exactly one of `branches.protected`.
    #[must_use]
    pub fn is_protected(&self, branch: &str) -> bool {
        self.branches.protected.iter().any(|p| p == branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.gate.new_code_coverage, 95);
        assert_eq!(c.commit.max_files, 10);
        assert_eq!(c.commit.max_lines, 500);
        assert_eq!(c.branches.protected, ["master", "dev"]);
        assert_eq!(c.retention.records_per_branch, 200);
        assert_eq!(c.retention.raw_reports_days, 90);
        assert_eq!(c.telemetry.endpoint, "");
        assert!(!c.telemetry.include_paths);
        assert_eq!(c.telemetry.log_level, "info");
        assert!(c.branches.pattern.is_match("feature/x-1"));
        assert!(!c.branches.pattern.is_match("Feature/X"));
    }

    #[test]
    fn partial_section_keeps_other_defaults() {
        let c = Config::parse("[gate]\nnew_code_coverage = 80").unwrap();
        assert_eq!(c.record.timeout_minutes, 30);
        assert_eq!(c.gate.new_code_coverage, 80);
    }

    #[test]
    fn threshold_over_100_names_key() {
        assert_eq!(
            Config::parse("[gate]\nnew_code_coverage = 120")
                .unwrap_err()
                .to_string(),
            "invalid .devctl.toml: gate.new_code_coverage: must be 0–100, got 120"
        );
    }

    #[test]
    fn threshold_huge_or_negative_names_key() {
        assert_eq!(
            Config::parse("[gate]\nnew_code_coverage = 100000")
                .unwrap_err()
                .to_string(),
            "invalid .devctl.toml: gate.new_code_coverage: must be 0–100, got 100000"
        );
        assert_eq!(
            Config::parse("[gate]\nnew_code_coverage = -1")
                .unwrap_err()
                .to_string(),
            "invalid .devctl.toml: gate.new_code_coverage: must be 0–100, got -1"
        );
    }

    #[test]
    fn bad_regex_names_key() {
        assert!(
            Config::parse("[branches]\npattern = \"(\"")
                .unwrap_err()
                .to_string()
                .starts_with("invalid .devctl.toml: branches.pattern:")
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        let e = Config::parse("[gate]\nfoo = 1").unwrap_err().to_string();
        assert!(e.starts_with("invalid .devctl.toml: "), "{e}");
        assert!(e.contains("foo"), "{e}");
    }

    #[test]
    fn unknown_section_is_rejected() {
        let e = Config::parse("[test]\nx = 1").unwrap_err().to_string();
        assert!(e.starts_with("invalid .devctl.toml: "), "{e}");
        assert!(e.contains("test"), "{e}");
    }

    #[test]
    fn type_mismatch_names_key() {
        let e = Config::parse("[record]\ntimeout_minutes = \"soon\"")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("invalid .devctl.toml: record.timeout_minutes: "),
            "{e}"
        );
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn syntax_error_has_prefix() {
        let e = Config::parse("[gate").unwrap_err().to_string();
        assert!(e.starts_with("invalid .devctl.toml: "), "{e}");
    }

    #[test]
    fn target_for_uses_prefix() {
        let c = Config::default();
        assert_eq!(c.target_for("feature/x"), Some("dev"));
        assert_eq!(c.target_for("hotfix/y"), Some("master"));
        assert_eq!(c.target_for("wip/z"), None);
        assert_eq!(c.target_for("feature"), None);
        assert_eq!(c.target_for("feature/a/b"), Some("dev"));
    }

    #[test]
    fn is_protected_is_exact() {
        let c = Config::default();
        assert!(c.is_protected("master"));
        assert!(c.is_protected("dev"));
        assert!(!c.is_protected("devx"));
        assert!(!c.is_protected("feature/dev"));
    }

    #[test]
    fn telemetry_log_level_validated() {
        assert!(Config::parse("[telemetry]\nlog_level = \"loud\"").is_err());
        assert_eq!(
            Config::parse("[telemetry]\nlog_level = \"DEBUG\"")
                .unwrap()
                .telemetry
                .log_level,
            "debug"
        );
    }

    #[test]
    fn zero_minimums_rejected() {
        for (k, s) in [
            ("record.timeout_minutes", "[record]\ntimeout_minutes = 0"),
            ("commit.max_files", "[commit]\nmax_files = 0"),
            ("commit.max_lines", "[commit]\nmax_lines = 0"),
        ] {
            let e = Config::parse(s).unwrap_err().to_string();
            assert!(
                e.starts_with(&format!("invalid .devctl.toml: {k}: ")),
                "{e}"
            );
        }
    }

    #[test]
    fn crlf_loads() {
        let c = Config::parse(
            "[gate]\r\nnew_code_coverage = 80\r\n[record]\r\ntimeout_minutes = 5\r\n",
        )
        .unwrap();
        assert_eq!(c.gate.new_code_coverage, 80);
        assert_eq!(c.record.timeout_minutes, 5);
    }

    #[test]
    fn custom_targets_and_protected() {
        let c = Config::parse("[branches]\nprotected = [\"main\"]\ntargets = { topic = \"main\" }")
            .unwrap();
        assert!(c.is_protected("main"));
        assert!(!c.is_protected("dev"));
        assert_eq!(c.target_for("topic/x"), Some("main"));
        assert_eq!(c.target_for("feature/x"), None);
    }

    #[test]
    fn load_missing_is_defaults() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(d.path()).unwrap().gate.new_code_coverage, 95);
    }

    #[test]
    fn load_reads_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".devctl.toml"),
            "[gate]\nnew_code_coverage = 70\n",
        )
        .unwrap();
        assert_eq!(Config::load(d.path()).unwrap().gate.new_code_coverage, 70);
    }

    #[test]
    fn load_unreadable_names_path() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(".devctl.toml")).unwrap();
        let e = Config::load(d.path()).unwrap_err().to_string();
        assert!(e.contains(".devctl.toml"), "{e}");
    }

    #[test]
    fn load_invalid_keeps_prefix() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(".devctl.toml"),
            "[gate]\nnew_code_coverage = 120\n",
        )
        .unwrap();
        assert!(
            Config::load(d.path())
                .unwrap_err()
                .to_string()
                .starts_with("invalid .devctl.toml: gate.new_code_coverage")
        );
    }
}
