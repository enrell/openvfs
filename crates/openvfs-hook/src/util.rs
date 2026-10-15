//! Small pure helpers shared by the detours: access-mask classification and
//! DOS-style wildcard matching. Kept separate (and pure) so they can be unit
//! tested without touching the hooked process.

use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_ALWAYS, CREATE_NEW, FILE_APPEND_DATA, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, OPEN_ALWAYS,
    OPEN_EXISTING, TRUNCATE_EXISTING, WRITE_DAC, WRITE_OWNER,
};

/// `true` if a `FindFirstFile` pattern contains wildcards (`*`, `?` and the
/// DOS aliases `<`, `>`, `"`).
pub fn has_wildcards(pattern: &str) -> bool {
    pattern.contains(['*', '?', '<', '>', '"'])
}

/// Splits a folded relative pattern `dir\file.txt` into `(dir, pattern)`.
/// Returns `("", rel)` when there is no separator.
pub fn split_dir_pattern(rel: &str) -> (String, String) {
    match rel.rfind('\\') {
        Some(i) => (rel[..i].to_owned(), rel[i + 1..].to_owned()),
        None => (String::new(), rel.to_owned()),
    }
}

/// The parent of a folded relative path (`a\b\c` -> `a\b`), `None` at root.
pub fn parent_rel(rel: &str) -> Option<String> {
    rel.rfind('\\').map(|i| rel[..i].to_owned())
}

/// `true` if `dwDesiredAccess`/`dwCreationDisposition` intend to modify or
/// create the file — i.e. the open must be captured into the overwrite layer.
pub fn wants_write(access: u32, disposition: u32) -> bool {
    const WRITE_BITS: u32 = GENERIC_WRITE
        | GENERIC_ALL
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | WRITE_DAC
        | WRITE_OWNER;
    access & WRITE_BITS != 0 || matches!(disposition, CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING)
}

/// `true` if the disposition can create a missing file.
pub fn may_create(disposition: u32) -> bool {
    matches!(disposition, CREATE_NEW | CREATE_ALWAYS | OPEN_ALWAYS)
}

/// `true` if the disposition truncates existing content.
pub fn truncates(disposition: u32) -> bool {
    matches!(disposition, CREATE_ALWAYS | TRUNCATE_EXISTING)
}

/// `true` if the disposition requires the file to exist.
#[allow(dead_code)] // documents the disposition taxonomy; used by tests
pub fn requires_existing(disposition: u32) -> bool {
    matches!(disposition, OPEN_EXISTING | TRUNCATE_EXISTING)
}

/// DOS-style wildcard match on **folded** strings.
///
/// `*` matches any sequence, `?` one character. DOS aliases are honored like
/// `RtlIsNameInExpression`: `<` is a `*`, `>` is a `?` that may also match
/// empty at the end, `"` is a `.` that may also match empty at the end.
///
/// The match is a reasonable approximation of `FindFirstFile` semantics; the
/// quirky DOS rules around extensions (`*.*` matching `file` without a dot,
/// `foo.` matching `foo`) are approximated by treating a trailing `.*`/`*.`
/// as optional.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" || pattern == "*.*" || pattern == "*." {
        return true;
    }
    let pat: Vec<char> = pattern.chars().collect();
    let nam: Vec<char> = name.chars().collect();
    if wild(&pat, &nam) {
        return true;
    }
    // DOS quirk: `*.txt`-style and `*.*` semantics — a trailing `.*` is
    // optional, so `foo*` also matches `foo.bar` handled above; `dir` also
    // matches `dir.*`. Approximation: retry without a trailing `.*`.
    if let Some(stripped) = pattern.strip_suffix(".*") {
        let pat2: Vec<char> = stripped.chars().collect();
        if wild(&pat2, &nam) {
            return true;
        }
    }
    false
}

/// Iterative wildcard matcher with `*` backtracking.
fn wild(pat: &[char], nam: &[char]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < nam.len() {
        let pc = pat.get(p).copied();
        match pc {
            Some('*') | Some('<') => {
                star = Some((p, n));
                p += 1;
            }
            Some('"') => {
                // DOS_DOT: matches '.' or nothing at the end.
                if nam[n] == '.' {
                    n += 1;
                }
                p += 1;
            }
            Some('>') => {
                // DOS_QM: matches one char, or empty at the end.
                n += 1;
                p += 1;
            }
            Some('?') => {
                n += 1;
                p += 1;
            }
            Some(c) if c == nam[n] => {
                n += 1;
                p += 1;
            }
            _ => {
                if let Some((sp, sn)) = star {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, n));
                } else {
                    return false;
                }
            }
        }
    }
    while p < pat.len() {
        match pat[p] {
            '*' | '<' => p += 1,
            _ => return p == pat.len() && n == nam.len(),
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_patterns() {
        assert!(wildcard_match("*.txt", "readme.txt"));
        assert!(!wildcard_match("*.txt", "readme.md"));
        assert!(wildcard_match("file?.dat", "file1.dat"));
        assert!(!wildcard_match("file?.dat", "file12.dat"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*.*", "noext"));
        assert!(wildcard_match("dir", "dir"));
        assert!(!wildcard_match("dir", "dir2"));
        assert!(wildcard_match("sub\\*", "sub\\x"));
    }

    #[test]
    fn dos_quirks() {
        assert!(wildcard_match("foo.*", "foo.txt"));
        assert!(wildcard_match("foo.*", "foo")); // trailing .* optional
        assert!(wildcard_match("*.txt", "a.b.txt"));
        assert!(wildcard_match("readme<", "readmexyz"));
        assert!(wildcard_match("a\"b", "a.b"));
    }

    #[test]
    fn access_flags() {
        assert!(wants_write(GENERIC_WRITE, OPEN_EXISTING));
        assert!(!wants_write(0x8000_0000, OPEN_EXISTING)); // GENERIC_READ
        assert!(wants_write(0, CREATE_ALWAYS));
        assert!(wants_write(0, CREATE_NEW));
        assert!(wants_write(FILE_WRITE_DATA, OPEN_EXISTING));
        assert!(!wants_write(0, OPEN_EXISTING));
        assert!(may_create(OPEN_ALWAYS));
        assert!(!may_create(OPEN_EXISTING));
        assert!(truncates(TRUNCATE_EXISTING));
        assert!(requires_existing(OPEN_EXISTING));
    }

    #[test]
    fn splits() {
        assert_eq!(split_dir_pattern("a\\b.txt"), ("a".to_owned(), "b.txt".to_owned()));
        assert_eq!(split_dir_pattern("f.txt"), ("".to_owned(), "f.txt".to_owned()));
        assert_eq!(parent_rel("a\\b\\c"), Some("a\\b".to_owned()));
        assert_eq!(parent_rel("c"), None);
    }
}
