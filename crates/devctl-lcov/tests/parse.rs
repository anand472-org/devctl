//! Fixtures are hand-written to match the shape of each tool's real LCOV output
//! (the toolchains are not available where this crate is developed):
//!
//! - `cargo-llvm-cov`: absolute `SF` paths, `FN` with mangled Rust symbols, `BRDA`.
//! - `vitest`: relative `SF`, `TN:` header, `FNF/FNH/LF/LH`, anonymous function names.
//! - `jest`: like vitest (istanbul reporter), `BRDA` with `-` taken, a file with no lines.
//! - `pytest-cov`: `DA` only, no `FN` records.
//! - `jacoco`: `jacoco-to-lcov` style, package-relative Java paths.
//! - `go`: `gcov2lcov` style, module-path `SF`, one `DA:line,hits,checksum` line.
//! - `coverlet`: Windows `SF:C:\...` path, `BRDA` with `-`.
//! - `windows-paths`: backslash drive paths only.
//! - `no-end-of-record`: last record is unterminated.
//! - `empty`: zero bytes.
//!
//! CRLF input is built in-test from strings, because `.gitattributes` forces LF on checkout.

use std::collections::BTreeMap;

use devctl_lcov::{Function, LineCounts, Report, parse};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}.info", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn counts(name: &str) -> (Report, LineCounts) {
    let r = parse(&fixture(name)).unwrap();
    let total = r.line_counts().0;
    (r, total)
}

fn lc(coverable: u32, covered: u32) -> LineCounts {
    LineCounts { coverable, covered }
}

#[test]
fn cargo_llvm_cov_fixture() {
    let (r, total) = counts("cargo-llvm-cov");
    assert_eq!(total, lc(7, 5));
    let lib = &r.files["/home/runner/work/repo/repo/src/lib.rs"];
    assert_eq!(lib.lines[&9], 0);
    assert_eq!(lib.functions["_RNvCs1a2b3c_4repo3add"].hits, 5);
    assert_eq!(lib.functions["_RNvCs1a2b3c_4repo3sub"].line, 9);
    assert_eq!(lib.branches[&(4, "0".into(), "1".into())], Some(0));
}

#[test]
fn vitest_fixture() {
    let (r, total) = counts("vitest");
    assert_eq!(total, lc(4, 3));
    assert_eq!(r.files["src/utils.ts"].lines[&1], 4);
    assert!(
        r.files["src/utils.ts"]
            .functions
            .contains_key("(anonymous_1)")
    );
}

#[test]
fn jest_fixture() {
    let (r, total) = counts("jest");
    assert_eq!(total, lc(7, 5));
    assert_eq!(r.files.len(), 3);
    assert_eq!(r.files["src/app.js"].lines[&2], 3);
    assert_eq!(
        r.files["src/app.js"].branches[&(3, "0".into(), "1".into())],
        None
    );
}

#[test]
fn pytest_cov_fixture() {
    let (r, total) = counts("pytest-cov");
    assert_eq!(total, lc(8, 5));
    assert_eq!(r.files["pkg/core.py"].lines[&4], 0);
    assert!(r.files["pkg/core.py"].functions.is_empty());
}

#[test]
fn jacoco_fixture() {
    let (r, total) = counts("jacoco");
    assert_eq!(total, lc(6, 4));
    assert_eq!(r.files["com/example/Calc.java"].lines[&5], 2);
}

#[test]
fn go_fixture() {
    let (r, total) = counts("go");
    assert_eq!(total, lc(6, 4));
    assert_eq!(
        r.files["github.com/example/repo/pkg/greet/greet.go"].lines[&8],
        3
    );
}

#[test]
fn coverlet_fixture() {
    let (r, total) = counts("coverlet");
    assert_eq!(total, lc(5, 3));
    assert_eq!(r.files[r"C:\work\repo\src\Calc.cs"].lines[&12], 3);
}

#[test]
fn windows_paths_fixture() {
    let (r, total) = counts("windows-paths");
    assert_eq!(total, lc(3, 2));
    assert_eq!(r.files[r"C:\work\repo\src\b.cs"].lines[&1], 4);
}

#[test]
fn empty_fixture() {
    let (r, total) = counts("empty");
    assert!(r.files.is_empty());
    assert_eq!(total, lc(0, 0));
}

#[test]
fn no_end_of_record_fixture() {
    let (r, total) = counts("no-end-of-record");
    assert_eq!(total, lc(5, 3));
    assert_eq!(r.files["src/b.rs"].lines[&3], 0);
}

#[test]
fn empty_file_is_empty_report() {
    assert!(parse("").unwrap().files.is_empty());
}

#[test]
fn missing_end_of_record_keeps_last_file() {
    assert_eq!(parse(&fixture("no-end-of-record")).unwrap().files.len(), 2);
}

#[test]
fn merge_sums_hits() {
    let a = parse("SF:a.rs\nDA:1,1\nDA:2,0\nend_of_record\n").unwrap();
    let b = parse("SF:a.rs\nDA:2,3\nDA:3,0\nend_of_record\n").unwrap();
    let m = Report::merge(&[a, b]);
    assert_eq!(
        m.files["a.rs"].lines,
        BTreeMap::from([(1, 1), (2, 3), (3, 0)])
    );
    assert_eq!(
        m.line_counts().0,
        LineCounts {
            coverable: 3,
            covered: 2
        }
    );
}

#[test]
fn merge_sums_functions_and_branches() {
    let a = parse("SF:a\nFN:1,f\nFNDA:1,f\nBRDA:1,0,0,-\nBRDA:1,0,1,2\nend_of_record\n").unwrap();
    let b = parse("SF:a\nFN:1,f\nFNDA:2,f\nBRDA:1,0,0,4\nBRDA:1,0,1,-\nend_of_record\n").unwrap();
    let m = Report::merge(&[a, b]);
    let f = &m.files["a"];
    assert_eq!(f.functions["f"].hits, 3);
    assert_eq!(f.branches[&(1, "0".into(), "0".into())], Some(4));
    assert_eq!(f.branches[&(1, "0".into(), "1".into())], Some(2));
}

#[test]
fn round_trips() {
    for name in [
        "cargo-llvm-cov",
        "vitest",
        "jest",
        "pytest-cov",
        "jacoco",
        "go",
        "coverlet",
        "windows-paths",
        "no-end-of-record",
        "empty",
    ] {
        let r = parse(&fixture(name)).unwrap();
        assert_eq!(parse(&r.to_lcov()).unwrap(), r, "{name}");
    }
}

#[test]
fn crlf_line_endings_parse() {
    assert_eq!(
        parse("SF:a.rs\r\nDA:1,2\r\nend_of_record\r\n")
            .unwrap()
            .files["a.rs"]
            .lines[&1],
        2
    );
}

#[test]
fn crlf_fixture_parses_same_as_lf() {
    let lf = fixture("coverlet");
    let crlf = lf.replace('\n', "\r\n");
    assert_eq!(parse(&crlf).unwrap(), parse(&lf).unwrap());
}

#[test]
fn bad_da_reports_line_number() {
    assert_eq!(parse("SF:a\nDA:x,1\n").unwrap_err().line, 2);
}

#[test]
fn negative_hits_clamp_and_overflow_saturates() {
    let r = parse(
        "SF:a\nDA:1,-1\nDA:2,99999999999999999999999\nFN:1,f\nFNDA:-3,f\nBRDA:1,0,0,-1\nend_of_record\n",
    )
    .unwrap();
    let f = &r.files["a"];
    assert_eq!(f.lines[&1], 0);
    assert_eq!(f.lines[&2], u64::MAX);
    assert_eq!(f.functions["f"].hits, 0);
    assert_eq!(f.branches[&(1, "0".into(), "0".into())], Some(0));
}

#[test]
fn non_numeric_hits_are_errors() {
    assert_eq!(parse("SF:a\nDA:1,abc\n").unwrap_err().line, 2);
    assert_eq!(parse("SF:a\nDA:1,-\n").unwrap_err().line, 2);
}

#[test]
fn leading_bom_is_ignored() {
    let r = parse("\u{feff}SF:a.rs\nDA:1,2\nend_of_record\n").unwrap();
    assert_eq!(r.files["a.rs"].lines[&1], 2);
}

#[test]
fn surrounding_whitespace_is_ignored() {
    let r = parse("  SF:a.rs\n  DA:1,1\t\nend_of_record \nSF:b.rs\nDA:1,1\n").unwrap();
    assert_eq!(r.files.len(), 2);
    assert_eq!(r.files["a.rs"].lines[&1], 1);
}

#[test]
fn fn_with_end_line_uses_name() {
    let r = parse("SF:a\nFN:3,5,my_fn\nFNDA:2,my_fn\nend_of_record\n").unwrap();
    assert_eq!(
        r.files["a"].functions["my_fn"],
        Function { line: 3, hits: 2 }
    );
}

#[test]
fn fnda_without_fn_is_kept() {
    let r = parse("SF:a\nFNDA:4,orphan\nend_of_record\n").unwrap();
    assert_eq!(
        r.files["a"].functions["orphan"],
        Function { line: 0, hits: 4 }
    );
}

#[test]
fn da_before_sf_is_error() {
    assert_eq!(parse("DA:1,1\n").unwrap_err().line, 1);
}

#[test]
fn unknown_records_are_ignored() {
    let r = parse("TN:x\nSF:a\nVER:1\nDA:1,1\nend_of_record\n").unwrap();
    assert_eq!(r.files["a"].lines[&1], 1);
}

#[test]
fn duplicate_da_lines_sum() {
    let r = parse("SF:a\nDA:1,1\nDA:1,2\nend_of_record\n").unwrap();
    assert_eq!(r.files["a"].lines[&1], 3);
}

#[test]
fn map_paths_drops_and_merges() {
    let r = parse(
        "SF:/w/a.rs\nDA:1,1\nend_of_record\nSF:a.rs\nDA:1,2\nDA:2,0\nend_of_record\nSF:/skip/x.rs\nDA:1,1\nend_of_record\n",
    )
    .unwrap();
    let m = r.map_paths(|p| {
        if p.starts_with("/skip") {
            None
        } else {
            Some(p.trim_start_matches("/w/").to_string())
        }
    });
    assert_eq!(m.files.len(), 1);
    assert_eq!(m.files["a.rs"].lines, BTreeMap::from([(1, 3), (2, 0)]));
}

#[test]
fn per_file_line_counts() {
    let (total, per) = parse(&fixture("pytest-cov")).unwrap().line_counts();
    assert_eq!(total, lc(8, 5));
    assert_eq!(per["pkg/util.py"], lc(3, 3));
}
