//! New-code coverage (spec section 4.2, rule 6): of the lines a push adds,
//! those with a `DA` entry in the record's LCOV are coverable; the share of
//! them with a non-zero hit count must reach `gate.new_code_coverage`.

use std::collections::BTreeMap;

use devctl_lcov::Report;

use crate::git::AddedLines;

/// Coverage of the added lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewCode {
    /// Added lines with a `DA` entry.
    pub coverable: u32,
    /// Coverable lines hit at least once.
    pub covered: u32,
    /// Path to the coverable lines never hit, ascending.
    pub uncovered: BTreeMap<String, Vec<u32>>,
}

/// Counts the added lines that `report` covers. Paths match exactly: both are
/// repository-relative and `/`-separated.
#[must_use]
pub fn new_code(added: &AddedLines, report: &Report) -> NewCode {
    let mut n = NewCode::default();
    for (path, lines) in added {
        let Some(file) = report.files.get(path) else {
            continue;
        };
        for line in lines {
            match file.lines.get(line) {
                Some(0) => {
                    n.coverable += 1;
                    n.uncovered.entry(path.clone()).or_default().push(*line);
                }
                Some(_) => {
                    n.coverable += 1;
                    n.covered += 1;
                }
                None => {}
            }
        }
    }
    n
}

/// Whether `n` reaches `threshold` percent. No coverable new line passes.
#[must_use]
pub fn passes(n: &NewCode, threshold: u8) -> bool {
    n.coverable == 0 || u64::from(n.covered) * 100 >= u64::from(threshold) * u64::from(n.coverable)
}

/// Ascending line numbers as ranges: `[3, 4, 5, 9]` -> `"3-5, 9"`.
#[must_use]
pub fn compress_lines(lines: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let start = lines[i];
        let mut end = start;
        while i + 1 < lines.len() && lines[i + 1] == end + 1 {
            i += 1;
            end = lines[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn added(path: &str, lines: &[u32]) -> AddedLines {
        let mut a = AddedLines::new();
        a.insert(
            path.to_string(),
            lines.iter().copied().collect::<BTreeSet<_>>(),
        );
        a
    }

    #[test]
    fn new_code_only_counts_da_lines() {
        let report = devctl_lcov::parse("SF:a.rs\nDA:1,1\nDA:2,0\nend_of_record\n").unwrap();
        let n = new_code(&added("a.rs", &[1, 2, 3]), &report);
        assert_eq!(n.coverable, 2);
        assert_eq!(n.covered, 1);
        assert_eq!(n.uncovered, BTreeMap::from([("a.rs".to_string(), vec![2])]));
    }

    #[test]
    fn files_missing_from_the_report_are_not_coverable() {
        let report = devctl_lcov::parse("SF:a.rs\nDA:1,0\nend_of_record\n").unwrap();
        let n = new_code(&added("README.md", &[1, 2]), &report);
        assert_eq!(n, NewCode::default());
    }

    #[test]
    fn no_coverable_lines_passes() {
        assert!(passes(
            &NewCode {
                coverable: 0,
                covered: 0,
                ..NewCode::default()
            },
            95
        ));
        assert!(passes(&NewCode::default(), 100));
    }

    #[test]
    fn threshold_boundary() {
        let n = |covered| NewCode {
            coverable: 20,
            covered,
            ..NewCode::default()
        };
        assert!(passes(&n(19), 95));
        assert!(!passes(&n(18), 95));
        assert!(passes(&n(20), 100));
        assert!(!passes(&n(19), 100));
        assert!(passes(&n(0), 0));
        // No overflow on huge counts.
        let big = NewCode {
            coverable: u32::MAX,
            covered: u32::MAX,
            ..NewCode::default()
        };
        assert!(passes(&big, 100));
    }

    #[test]
    fn compresses_ranges() {
        assert_eq!(compress_lines(&[3, 4, 5, 9]), "3-5, 9");
        assert_eq!(compress_lines(&[]), "");
        assert_eq!(compress_lines(&[7]), "7");
        assert_eq!(compress_lines(&[1, 2, 4, 5, 7]), "1-2, 4-5, 7");
    }
}
