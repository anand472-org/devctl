//! Pure path normalization, compiled on every OS so both flavors are testable everywhere.

/// Collapses `.` and `..` components. `..` above the root is dropped.
fn collapse<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for part in parts {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    out
}

/// Normalizes a Windows path to an absolute, `/`-separated form with no `\\?\` prefix and an
/// upper-case drive letter. Purely lexical: nothing is read from the file system. The input
/// must be absolute for the result to be; a relative input is treated as relative to the root.
#[must_use]
pub fn normalize_windows(p: &str) -> String {
    let s = p.replace('\\', "/");
    let (s, verbatim) = match s.strip_prefix("//?/").or_else(|| s.strip_prefix("//./")) {
        Some(rest) => (rest, true),
        None => (s.as_str(), false),
    };

    // `//?/UNC/server/share/...` and `//server/share/...`
    let unc = if let (true, Some(rest)) = (verbatim, s.strip_prefix("UNC/")) {
        Some(rest)
    } else if s.starts_with("//") {
        Some(s.trim_start_matches('/'))
    } else {
        None
    };
    if let Some(rest) = unc {
        let parts = collapse(rest.split('/'));
        return format!("//{}", parts.join("/"));
    }

    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = char::from(bytes[0]).to_ascii_uppercase();
        let parts = collapse(s[2..].split('/'));
        return format!("{drive}:/{}", parts.join("/"));
    }

    format!("/{}", collapse(s.split('/')).join("/"))
}

/// Normalizes a Unix path to `/`-rooted form with `.` and `..` collapsed. Purely lexical; the
/// input must be absolute for the result to be, a relative input is treated as relative to `/`.
#[must_use]
pub fn normalize_unix(p: &str) -> String {
    format!("/{}", collapse(p.split('/')).join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_unc_and_root() {
        assert_eq!(
            normalize_windows(r"\\?\UNC\srv\share\a\..\b"),
            "//srv/share/b"
        );
        assert_eq!(normalize_windows(r"d:\"), "D:/");
        assert_eq!(normalize_windows(r"C:\a\..\..\b"), "C:/b");
    }

    #[test]
    fn bare_unc_segment_is_not_special() {
        assert_eq!(normalize_windows(r"UNC\x\y"), "/UNC/x/y");
    }

    #[test]
    fn unix_root_and_extra_slashes() {
        assert_eq!(normalize_unix("/"), "/");
        assert_eq!(normalize_unix("//a///b/"), "/a/b");
        assert_eq!(normalize_unix("/../a"), "/a");
    }
}
