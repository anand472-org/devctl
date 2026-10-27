//! Fixtures are hand-written to match the shape of each tool's real `JUnit` output
//! (the toolchains are not available where this crate is developed):
//!
//! - `nextest`: `<testsuites>` root, `system-out`/`system-err` inside a failed case.
//! - `vitest`: entity-escaped names, CDATA `failure`, `skipped` child.
//! - `jest-junit`: `<properties>`, failure text with escaped angle brackets.
//! - `pytest`: single-line output, `<error>` for setup errors, `skipped` with text.
//! - `surefire`: bare `<testsuite>` root, `time="1,234.5"`, `flakyFailure` (counts as passed).
//! - `go-junit-report`: tab-indented, empty-element pairs, `<testsuites>` root.
//! - `dotnet-junit-logger`: seven-digit times.
//! - `single-suite-root`: bare root, comment and processing instruction, missing `time`.
//! - `nested-suites`: DOCTYPE, three levels of nesting, all four statuses.
//!
//! CRLF input and BOMs are built in-test from strings, because `.gitattributes` forces LF on checkout.

use std::time::Duration;

use devctl_junit::{CaseStatus, TestCounts, TestRun, parse};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}.xml", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn counts(xml: &str) -> TestCounts {
    parse(xml).unwrap().counts()
}

fn assert_counts(c: &TestCounts, pfes: (u32, u32, u32, u32), secs: f64) {
    assert_eq!((c.passed, c.failed, c.errored, c.skipped), pfes, "{c:?}");
    let got = c.duration.as_secs_f64();
    assert!((got - secs).abs() < 1e-6, "duration {got} != {secs}");
}

fn names(run: &TestRun) -> Vec<&str> {
    run.suites
        .iter()
        .flat_map(|s| s.cases.iter().map(|c| c.name.as_str()))
        .collect()
}

#[test]
fn nextest() {
    let run = parse(&fixture("nextest")).unwrap();
    assert_counts(&run.counts(), (2, 1, 0, 1), 0.46);
    assert!(names(&run).contains(&"rejects_garbage"));
    assert_eq!(run.suites[0].name, "devctl-lcov");
    assert_eq!(
        run.suites[0].cases[0].classname.as_deref(),
        Some("devctl-lcov::parse")
    );
}

#[test]
fn vitest() {
    let run = parse(&fixture("vitest")).unwrap();
    assert_counts(&run.counts(), (2, 1, 0, 1), 0.006);
    assert!(names(&run).contains(&"adds > negative"));
}

#[test]
fn jest_junit() {
    let run = parse(&fixture("jest-junit")).unwrap();
    assert_counts(&run.counts(), (1, 1, 0, 1), 0.009);
    assert!(names(&run).contains(&"Calculator divides & rounds"));
}

#[test]
fn pytest() {
    let run = parse(&fixture("pytest")).unwrap();
    assert_counts(&run.counts(), (2, 1, 1, 1), 0.010);
    assert!(names(&run).contains(&"test_param[a-1]"));
}

#[test]
fn surefire() {
    let run = parse(&fixture("surefire")).unwrap();
    // testFlaky has only a flakyFailure: it passed on rerun, so it is Passed.
    assert_counts(&run.counts(), (2, 1, 1, 1), 1235.03);
    let flaky = run.suites[0]
        .cases
        .iter()
        .find(|c| c.name == "testFlaky")
        .unwrap();
    assert_eq!(flaky.status, CaseStatus::Passed);
}

#[test]
fn go_junit_report() {
    let run = parse(&fixture("go-junit-report")).unwrap();
    assert_counts(&run.counts(), (1, 1, 0, 1), 0.010);
    assert!(names(&run).contains(&"TestB"));
}

#[test]
fn dotnet_junit_logger() {
    let run = parse(&fixture("dotnet-junit-logger")).unwrap();
    assert_counts(&run.counts(), (1, 1, 0, 1), 0.011_234);
    assert!(names(&run).contains(&"Div_Throws"));
}

#[test]
fn single_suite_root() {
    let run = parse(&fixture("single-suite-root")).unwrap();
    assert_counts(&run.counts(), (2, 0, 0, 0), 2.5);
    assert_eq!(run.suites.len(), 1);
    assert_eq!(run.suites[0].name, "solo");
}

#[test]
fn nested_suites_are_flattened() {
    let run = parse(&fixture("nested-suites")).unwrap();
    assert_counts(&run.counts(), (2, 1, 1, 1), 10.5);
    assert_eq!(run.suites.len(), 3);
    let by_name = |n: &str| run.suites.iter().find(|s| s.name == n).unwrap();
    // Cases after a nested suite closes still belong to the outer suite.
    assert_eq!(
        by_name("outer")
            .cases
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["o1", "o2"]
    );
    assert_eq!(by_name("inner").cases.len(), 2);
    assert_eq!(by_name("innermost").cases[0].status, CaseStatus::Error);
}

#[test]
fn duration_with_thousands_separator() {
    let c = counts(r#"<testsuite><testcase name="a" time="1,234.5"/></testsuite>"#);
    assert_eq!(c.duration, Duration::from_secs_f64(1234.5));
}

#[test]
fn missing_time_is_zero() {
    let c = counts(r#"<testsuite><testcase name="a"/></testsuite>"#);
    assert_eq!(c.duration, Duration::ZERO);
    assert_eq!(c.passed, 1);
}

#[test]
fn malformed_time_is_zero_not_an_error() {
    for t in ["abc", "", "-1", "NaN", "inf", "1e999", "1.2.3"] {
        let xml = format!(r#"<testsuite><testcase name="a" time="{t}"/></testsuite>"#);
        let c = counts(&xml);
        assert_eq!(c.duration, Duration::ZERO, "time={t:?}");
        assert_eq!(c.passed, 1);
    }
}

#[test]
fn suite_duration_is_sum_of_cases() {
    let xml = r#"<testsuites time="999"><testsuite name="s" time="500">
        <testcase name="a" time="1"/><testcase name="b" time="2.5"/></testsuite></testsuites>"#;
    assert_eq!(counts(xml).duration, Duration::from_secs_f64(3.5));
}

#[test]
fn crlf_xml_parses() {
    for name in ["nextest", "vitest", "pytest", "nested-suites", "surefire"] {
        let lf = fixture(name);
        let crlf = lf.replace('\n', "\r\n");
        assert_ne!(lf, crlf);
        assert_eq!(counts(&lf), counts(&crlf), "{name}");
    }
}

#[test]
fn utf8_bom_is_stripped() {
    let xml = format!("\u{feff}{}", fixture("nextest"));
    assert_counts(&counts(&xml), (2, 1, 0, 1), 0.46);
}

#[test]
fn failure_and_error_win_over_skipped() {
    let xml = r"<testsuite>
        <testcase name='a'><skipped/><failure message='x'/></testcase>
        <testcase name='b'><skipped/><error message='x'/></testcase>
        <testcase name='c'><failure/><error/></testcase>
        <testcase name='d'><error/><failure/></testcase>
        </testsuite>";
    let run = parse(xml).unwrap();
    let st: Vec<_> = run.suites[0].cases.iter().map(|c| c.status).collect();
    assert_eq!(
        st,
        [
            CaseStatus::Failed,
            CaseStatus::Error,
            CaseStatus::Failed,
            CaseStatus::Failed
        ]
    );
}

#[test]
fn rerun_elements_alone_do_not_fail_a_case() {
    let xml = r"<testsuite><testcase name='a'><rerunFailure/><rerunError/><flakyError/></testcase></testsuite>";
    assert_eq!(counts(xml).passed, 1);
}

#[test]
fn rerun_failure_with_final_failure_is_failed() {
    let xml = r"<testsuite><testcase name='a'><failure/><rerunFailure/></testcase></testsuite>";
    assert_eq!(counts(xml).failed, 1);
}

#[test]
fn unknown_elements_and_attributes_are_ignored() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <testsuites foo="bar"><testsuite name="s" weird="1"><properties><property name="k" value="v"/></properties>
        <testcase name="a" extra="1"><system-out>x</system-out><custom><failure/></custom></testcase>
        <system-err>e</system-err></testsuite></testsuites>"#;
    // A <failure> nested inside an unknown element is not a direct child of the case.
    assert_counts(&counts(xml), (1, 0, 0, 0), 0.0);
}

#[test]
fn entities_in_names_are_unescaped() {
    let xml = r#"<testsuite name="a&amp;b"><testcase name="x &lt; y &quot;q&quot; &#65;" classname="c&apos;d"/></testsuite>"#;
    let run = parse(xml).unwrap();
    assert_eq!(run.suites[0].name, "a&b");
    assert_eq!(run.suites[0].cases[0].name, "x < y \"q\" A");
    assert_eq!(run.suites[0].cases[0].classname.as_deref(), Some("c'd"));
}

#[test]
fn empty_testsuites_is_an_empty_run() {
    let run = parse("<testsuites/>").unwrap();
    assert_eq!(run.counts(), TestCounts::default());
    let run = parse("<testsuite name='e'></testsuite>").unwrap();
    assert_eq!(run.counts(), TestCounts::default());
}

#[test]
fn merge_concatenates_suites() {
    let a = parse(&fixture("nextest")).unwrap();
    let b = parse(&fixture("pytest")).unwrap();
    let merged = TestRun::merge(vec![a, b]);
    assert_eq!(merged.suites.len(), 3);
    assert_counts(&merged.counts(), (4, 2, 1, 2), 0.47);
    assert_eq!(TestRun::merge(vec![]).counts(), TestCounts::default());
}

#[test]
fn malformed_xml_errors() {
    assert!(parse("<testsuite><testcase>").is_err());
}

#[test]
fn other_malformed_input_errors() {
    for xml in [
        "",
        "   ",
        "not xml",
        "<html><body>hi</body></html>",
        "<testsuite><testcase name='a'></testsuite>",
        "<testsuite><testcase name='a'/>",
        "<testsuite name=unquoted></testsuite>",
        "<testsuites><testsuite></testsuites>",
    ] {
        assert!(parse(xml).is_err(), "should fail: {xml:?}");
    }
}

#[test]
fn error_message_has_position() {
    let e = parse("<testsuite></testcase>").unwrap_err().to_string();
    assert!(e.contains("byte"), "{e}");
}

#[test]
fn suite_inside_unknown_wrapper_is_counted() {
    let xml = r"<report>
        <testsuite name='ok'><testcase name='a'/></testsuite>
        <wrapper><testsuite name='w'><testcase name='b'><failure/></testcase></testsuite></wrapper>
        </report>";
    assert_eq!(counts(xml).failed, 1);
    assert_eq!(counts(xml).passed, 1);
}
