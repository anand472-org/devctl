//! `JUnit` XML parser. See [`parse`] for the accepted shapes and status rules.

use std::time::Duration;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseStatus {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Case {
    pub name: String,
    pub classname: Option<String>,
    pub status: CaseStatus,
    pub duration: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suite {
    pub name: String,
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TestRun {
    pub suites: Vec<Suite>,
}

#[derive(Default, PartialEq, Debug)]
pub struct TestCounts {
    pub passed: u32,
    pub failed: u32,
    pub errored: u32,
    pub skipped: u32,
    pub duration: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The XML is malformed (mismatched tags, bad syntax).
    #[error("malformed XML at byte {position}: {source}")]
    Xml {
        position: u64,
        #[source]
        source: quick_xml::Error,
    },
    #[error("malformed XML attribute at byte {position}: {source}")]
    Attribute {
        position: u64,
        #[source]
        source: quick_xml::events::attributes::AttrError,
    },
    /// The input ended with elements still open, e.g. a truncated report.
    #[error("unexpected end of input at byte {position}: <{element}> is never closed")]
    Unclosed { position: u64, element: String },
    /// Well-formed XML with no `<testsuites>`, `<testsuite>` or `<testcase>` element.
    #[error("no <testsuites>, <testsuite> or <testcase> element found; not a JUnit report")]
    NotJunit,
}

/// What an open element is, so direct children of a `<testcase>` can be recognised.
enum Open {
    Suites,
    Suite,
    Case,
    Other(String),
}

fn rank(s: CaseStatus) -> u8 {
    match s {
        CaseStatus::Passed => 0,
        CaseStatus::Skipped => 1,
        CaseStatus::Error => 2,
        CaseStatus::Failed => 3,
    }
}

/// Parses a `time` attribute in seconds. It may be a float and may contain
/// thousands separators (`1,234.5`). A missing, malformed, negative or
/// non-finite value is treated as zero rather than failing the whole parse.
fn parse_time(raw: Option<&str>) -> Duration {
    let Some(raw) = raw else {
        return Duration::ZERO;
    };
    let cleaned: String = raw.trim().chars().filter(|&c| c != ',').collect();
    match cleaned.parse::<f64>() {
        // Capped at ~31 years so `from_secs_f64` can never overflow.
        Ok(s) if s.is_finite() && (0.0..1.0e9).contains(&s) => Duration::from_secs_f64(s),
        _ => Duration::ZERO,
    }
}

struct Attrs {
    name: Option<String>,
    classname: Option<String>,
    time: Option<String>,
}

fn read_attrs(e: &BytesStart<'_>, reader: &Reader<&[u8]>) -> Result<Attrs, ParseError> {
    let mut out = Attrs {
        name: None,
        classname: None,
        time: None,
    };
    for attr in e.attributes() {
        let attr = attr.map_err(|source| ParseError::Attribute {
            position: reader.error_position(),
            source,
        })?;
        let slot = match attr.key.as_ref() {
            "name" => &mut out.name,
            "classname" => &mut out.classname,
            "time" => &mut out.time,
            _ => continue,
        };
        // An unresolvable entity (e.g. one defined in a DOCTYPE) falls back to the
        // raw text: an odd name must not turn a test run into a parse error.
        let value = match attr.normalized_value(XmlVersion::Implicit1_0) {
            Ok(v) => v.into_owned(),
            Err(_) => attr.value.to_string(),
        };
        *slot = Some(value);
    }
    Ok(out)
}

#[derive(Default)]
struct Builder {
    suites: Vec<Suite>,
    stack: Vec<Open>,
    suite_idx: Vec<usize>,
    implicit: Option<usize>,
    pending: Option<(usize, Case)>,
    seen: bool,
}

impl Builder {
    /// Index of the suite a new case belongs to: the innermost open suite, or an
    /// unnamed one for a `<testcase>` that has no `<testsuite>` ancestor.
    fn target_suite(&mut self) -> usize {
        if let (Some(Open::Suite), Some(&i)) = (self.stack.last(), self.suite_idx.last()) {
            return i;
        }
        *self.implicit.get_or_insert_with(|| {
            self.suites.push(Suite {
                name: String::new(),
                cases: Vec::new(),
            });
            self.suites.len() - 1
        })
    }

    fn start(
        &mut self,
        e: &BytesStart<'_>,
        reader: &Reader<&[u8]>,
        empty: bool,
    ) -> Result<(), ParseError> {
        let parent_is_case = matches!(self.stack.last(), Some(Open::Case));
        let parent_is_other = matches!(self.stack.last(), Some(Open::Other(_)));
        let local = e.local_name();
        let local = local.as_ref();
        let other = || Open::Other(e.name().as_ref().to_owned());
        let open = if parent_is_other {
            other()
        } else {
            match local {
                "testsuites" => {
                    self.seen = true;
                    Open::Suites
                }
                "testsuite" => {
                    self.seen = true;
                    let a = read_attrs(e, reader)?;
                    self.suites.push(Suite {
                        name: a.name.unwrap_or_default(),
                        cases: Vec::new(),
                    });
                    self.suite_idx.push(self.suites.len() - 1);
                    Open::Suite
                }
                "testcase" if !parent_is_case => {
                    self.seen = true;
                    let a = read_attrs(e, reader)?;
                    let idx = self.target_suite();
                    let case = Case {
                        name: a.name.unwrap_or_default(),
                        classname: a.classname,
                        status: CaseStatus::Passed,
                        duration: parse_time(a.time.as_deref()),
                    };
                    if empty {
                        self.suites[idx].cases.push(case);
                        return Ok(());
                    }
                    self.pending = Some((idx, case));
                    Open::Case
                }
                "failure" | "error" | "skipped" if parent_is_case => {
                    let status = match local {
                        "failure" => CaseStatus::Failed,
                        "error" => CaseStatus::Error,
                        _ => CaseStatus::Skipped,
                    };
                    if let Some((_, case)) = &mut self.pending
                        && rank(status) > rank(case.status)
                    {
                        case.status = status;
                    }
                    other()
                }
                _ => other(),
            }
        };
        if !empty {
            self.stack.push(open);
        } else if matches!(open, Open::Suite) {
            // An empty `<testsuite/>` opens and closes at once.
            self.suite_idx.pop();
        }
        Ok(())
    }

    fn end(&mut self) {
        match self.stack.pop() {
            Some(Open::Suite) => {
                self.suite_idx.pop();
            }
            Some(Open::Case) => {
                if let Some((idx, case)) = self.pending.take() {
                    self.suites[idx].cases.push(case);
                }
            }
            _ => {}
        }
    }
}

/// Parses a `JUnit` XML report into a flat list of suites.
///
/// Both a `<testsuites>` root and a bare `<testsuite>` root are accepted, and
/// nested suites are flattened into `suites` in document order. Unknown
/// elements and attributes (`<properties>`, `<system-out>`, ...) are ignored.
/// A leading UTF-8 BOM and CRLF line endings are accepted.
///
/// A case is `Failed` if it has a `<failure>` child, `Error` if it has an
/// `<error>` child, `Skipped` if it has a `<skipped>` child, and `Passed`
/// otherwise. When several are present the more severe wins (`Failed`, then
/// `Error`, then `Skipped`), so a failed case is never counted as passed or
/// skipped. Surefire's `<flakyFailure>`, `<flakyError>`, `<rerunFailure>` and
/// `<rerunError>` alone do not fail a case: the test passed on a rerun (a case
/// that failed every rerun also carries a `<failure>`/`<error>` child).
///
/// `time` is in seconds, may be a float and may contain thousands separators.
/// A missing or malformed `time` counts as zero and never fails the parse.
///
/// # Errors
/// Returns [`ParseError`] for malformed XML (including a truncated file or
/// mismatched tags) and for well-formed XML that is not a `JUnit` report.
pub fn parse(xml: &str) -> Result<TestRun, ParseError> {
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let mut reader = Reader::from_str(xml);
    let mut b = Builder::default();
    loop {
        let event = reader.read_event().map_err(|source| ParseError::Xml {
            position: reader.error_position(),
            source,
        })?;
        match event {
            Event::Start(e) => b.start(&e, &reader, false)?,
            Event::Empty(e) => b.start(&e, &reader, true)?,
            Event::End(_) => b.end(),
            Event::Eof => break,
            _ => {}
        }
    }
    if let Some(open) = b.stack.last() {
        let element = match open {
            Open::Suites => "testsuites".to_owned(),
            Open::Suite => "testsuite".to_owned(),
            Open::Case => "testcase".to_owned(),
            Open::Other(n) => n.clone(),
        };
        return Err(ParseError::Unclosed {
            position: reader.buffer_position(),
            element,
        });
    }
    if !b.seen {
        return Err(ParseError::NotJunit);
    }
    Ok(TestRun { suites: b.suites })
}

impl TestRun {
    /// Concatenates the suites of several runs, in order.
    #[must_use]
    pub fn merge(runs: Vec<TestRun>) -> TestRun {
        TestRun {
            suites: runs.into_iter().flat_map(|r| r.suites).collect(),
        }
    }

    /// Counts cases by status. `duration` is the sum of the case `time`s, not
    /// the suites' own `time` attributes (which tools compute inconsistently).
    #[must_use]
    pub fn counts(&self) -> TestCounts {
        let mut c = TestCounts::default();
        for case in self.suites.iter().flat_map(|s| &s.cases) {
            match case.status {
                CaseStatus::Passed => c.passed += 1,
                CaseStatus::Failed => c.failed += 1,
                CaseStatus::Error => c.errored += 1,
                CaseStatus::Skipped => c.skipped += 1,
            }
            c.duration += case.duration;
        }
        c
    }
}
