//! Parse, merge and write LCOV coverage reports.
//!
//! Parsing is line-based and accepts both LF and CRLF. Unknown record types
//! and the summary records (`LF`, `LH`, `FNF`, `FNH`, `BRF`, `BRH`) are
//! ignored; summaries are recomputed from the data. Repeated `DA`, `FNDA` and
//! `BRDA` entries for the same key (within a record or across records for the
//! same `SF`) are summed. Hit counts are `u64`; a negative, non-numeric or
//! overflowing count is a [`ParseError`].

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A coverage report: one entry per source file, keyed by the `SF` path as written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub files: BTreeMap<String, FileCoverage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileCoverage {
    /// `DA` line number to hit count.
    pub lines: BTreeMap<u32, u64>,
    /// `FN`/`FNDA` function name to definition line and hits.
    pub functions: BTreeMap<String, Function>,
    /// `BRDA` (line, block, branch) to times taken; `None` when never evaluated (`-`).
    pub branches: BTreeMap<(u32, String, String), Option<u64>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Function {
    pub line: u32,
    pub hits: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineCounts {
    pub coverable: u32,
    pub covered: u32,
}

#[derive(Debug, thiserror::Error)]
#[error("line {line}: {reason}")]
pub struct ParseError {
    /// 1-based line number in the input.
    pub line: usize,
    pub reason: String,
}

fn err(line: usize, reason: impl Into<String>) -> ParseError {
    ParseError {
        line,
        reason: reason.into(),
    }
}

fn num<T: std::str::FromStr>(s: &str, what: &str, line: usize) -> Result<T, ParseError> {
    s.trim()
        .parse()
        .map_err(|_| err(line, format!("invalid {what} {s:?}")))
}

/// Parses an LCOV report.
///
/// # Errors
///
/// Returns a [`ParseError`] for a malformed `DA`/`FN`/`FNDA`/`BRDA` record or
/// one that appears outside an `SF` record.
pub fn parse(input: &str) -> Result<Report, ParseError> {
    let mut report = Report::default();
    let mut current: Option<String> = None;
    for (idx, raw) in input.lines().enumerate() {
        let n = idx + 1;
        let line = raw.trim_end_matches('\r');
        let Some((tag, rest)) = line.split_once(':') else {
            if line == "end_of_record" {
                current = None;
            }
            continue;
        };
        if tag == "SF" {
            report.files.entry(rest.to_string()).or_default();
            current = Some(rest.to_string());
            continue;
        }
        if !matches!(tag, "DA" | "FN" | "FNDA" | "BRDA") {
            continue;
        }
        let Some(path) = &current else {
            return Err(err(n, format!("{tag} record before any SF record")));
        };
        let file = report.files.entry(path.clone()).or_default();
        let fields: Vec<&str> = rest.split(',').collect();
        match tag {
            "DA" => {
                if fields.len() < 2 {
                    return Err(err(n, "DA needs line,hits"));
                }
                let l: u32 = num(fields[0], "line number", n)?;
                let h: u64 = num(fields[1], "hit count", n)?;
                let e = file.lines.entry(l).or_insert(0);
                *e = e.saturating_add(h);
            }
            "FN" => {
                let Some((l, name)) = rest.split_once(',') else {
                    return Err(err(n, "FN needs line,name"));
                };
                let l: u32 = num(l, "line number", n)?;
                // Newer LCOV: FN:start,end,name.
                let name = match name.split_once(',') {
                    Some((end, tail))
                        if end.bytes().all(|b| b.is_ascii_digit()) && !end.is_empty() =>
                    {
                        tail
                    }
                    _ => name,
                };
                file.functions
                    .entry(name.to_string())
                    .or_insert(Function { line: l, hits: 0 })
                    .line = l;
            }
            "FNDA" => {
                let Some((h, name)) = rest.split_once(',') else {
                    return Err(err(n, "FNDA needs hits,name"));
                };
                let h: u64 = num(h, "hit count", n)?;
                let f = file
                    .functions
                    .entry(name.to_string())
                    .or_insert(Function { line: 0, hits: 0 });
                f.hits = f.hits.saturating_add(h);
            }
            _ => {
                if fields.len() < 4 {
                    return Err(err(n, "BRDA needs line,block,branch,taken"));
                }
                let l: u32 = num(fields[0], "line number", n)?;
                let taken = match fields[3].trim() {
                    "-" => None,
                    t => Some(num::<u64>(t, "taken count", n)?),
                };
                let key = (l, fields[1].to_string(), fields[2].to_string());
                let e = file.branches.entry(key).or_insert(None);
                *e = add_opt(*e, taken);
            }
        }
    }
    Ok(report)
}

fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
    }
}

fn merge_file(into: &mut FileCoverage, from: FileCoverage) {
    for (l, h) in from.lines {
        let e = into.lines.entry(l).or_insert(0);
        *e = e.saturating_add(h);
    }
    for (name, f) in from.functions {
        let e = into.functions.entry(name).or_insert(Function {
            line: f.line,
            hits: 0,
        });
        if e.line == 0 {
            e.line = f.line;
        }
        e.hits = e.hits.saturating_add(f.hits);
    }
    for (k, t) in from.branches {
        let e = into.branches.entry(k).or_insert(None);
        *e = add_opt(*e, t);
    }
}

fn counts_of(f: &FileCoverage) -> LineCounts {
    let coverable = u32::try_from(f.lines.len()).unwrap_or(u32::MAX);
    let covered = u32::try_from(f.lines.values().filter(|&&h| h > 0).count()).unwrap_or(u32::MAX);
    LineCounts { coverable, covered }
}

impl Report {
    /// Sums hits per file, line, function and branch.
    #[must_use]
    pub fn merge(reports: &[Report]) -> Report {
        let mut out = Report::default();
        for r in reports {
            for (path, f) in &r.files {
                merge_file(out.files.entry(path.clone()).or_default(), f.clone());
            }
        }
        out
    }

    /// Serialises to LCOV; `parse(to_lcov(r)) == r`.
    #[must_use]
    pub fn to_lcov(&self) -> String {
        let mut s = String::new();
        for (path, f) in &self.files {
            let _ = writeln!(s, "SF:{path}");
            for (name, func) in &f.functions {
                let _ = writeln!(s, "FN:{},{name}", func.line);
            }
            for (name, func) in &f.functions {
                let _ = writeln!(s, "FNDA:{},{name}", func.hits);
            }
            let _ = writeln!(s, "FNF:{}", f.functions.len());
            let _ = writeln!(
                s,
                "FNH:{}",
                f.functions.values().filter(|x| x.hits > 0).count()
            );
            for (l, h) in &f.lines {
                let _ = writeln!(s, "DA:{l},{h}");
            }
            let c = counts_of(f);
            let _ = writeln!(s, "LF:{}\nLH:{}", c.coverable, c.covered);
            for ((l, block, branch), t) in &f.branches {
                match t {
                    Some(t) => writeln!(s, "BRDA:{l},{block},{branch},{t}"),
                    None => writeln!(s, "BRDA:{l},{block},{branch},-"),
                }
                .ok();
            }
            let _ = writeln!(s, "BRF:{}", f.branches.len());
            let _ = writeln!(
                s,
                "BRH:{}",
                f.branches
                    .values()
                    .filter(|t| t.is_some_and(|t| t > 0))
                    .count()
            );
            s.push_str("end_of_record\n");
        }
        s
    }

    /// Total and per-file line counts (a line is covered when its hits are above zero).
    #[must_use]
    pub fn line_counts(&self) -> (LineCounts, BTreeMap<String, LineCounts>) {
        let mut total = LineCounts::default();
        let mut per = BTreeMap::new();
        for (path, f) in &self.files {
            let c = counts_of(f);
            total.coverable = total.coverable.saturating_add(c.coverable);
            total.covered = total.covered.saturating_add(c.covered);
            per.insert(path.clone(), c);
        }
        (total, per)
    }

    /// Rewrites file paths; `None` drops the file, and paths that collide are merged.
    #[must_use]
    pub fn map_paths(self, mut f: impl FnMut(&str) -> Option<String>) -> Report {
        let mut out = Report::default();
        for (path, cov) in self.files {
            if let Some(new) = f(&path) {
                merge_file(out.files.entry(new).or_default(), cov);
            }
        }
        out
    }
}
