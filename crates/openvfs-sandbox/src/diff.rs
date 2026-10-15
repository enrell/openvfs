//! A compact line diff (LCS-based) rendered in unified format.

use std::fmt::Write as _;

/// Above this many cells in the LCS table we give up and print a summary.
const MAX_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op<'a> {
    Same(&'a str),
    Del(&'a str),
    Add(&'a str),
}

/// ANSI styling, or none.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn paint(self, code: &str, text: &str) -> String {
        if self.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }
}

/// Renders the differences between `old` and `new` as unified-diff hunks
/// with `context` lines around each change. Returns `None` when the inputs
/// are too large to diff line by line.
#[allow(clippy::many_single_char_names)] // a/b, o/n: the usual diff vocabulary
pub fn unified(old: &str, new: &str, context: usize, style: Style) -> Option<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = diff_lines(&a, &b)?;

    let changed: Vec<usize> =
        ops.iter().enumerate().filter(|(_, op)| !matches!(op, Op::Same(_))).map(|(i, _)| i).collect();

    // Group nearby changes into hunks: inclusive ranges of op indices.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let (start, end) = (i.saturating_sub(context), (i + context).min(ops.len() - 1));
        match hunks.last_mut() {
            Some(h) if start <= h.1 + 1 => h.1 = h.1.max(end),
            _ => hunks.push((start, end)),
        }
    }

    // Line numbers (0-based) at each op index, for the hunk headers.
    let mut old_line = Vec::with_capacity(ops.len());
    let mut new_line = Vec::with_capacity(ops.len());
    let (mut o, mut n) = (0, 0);
    for op in &ops {
        old_line.push(o);
        new_line.push(n);
        match op {
            Op::Same(_) => (o, n) = (o + 1, n + 1),
            Op::Del(_) => o += 1,
            Op::Add(_) => n += 1,
        }
    }

    let mut out = String::new();
    for (start, end) in hunks {
        let slice = &ops[start..=end];
        let old_len = slice.iter().filter(|op| !matches!(op, Op::Add(_))).count();
        let new_len = slice.iter().filter(|op| !matches!(op, Op::Del(_))).count();
        let header = format!("@@ -{},{old_len} +{},{new_len} @@", old_line[start] + 1, new_line[start] + 1);
        let _ = writeln!(out, "{}", style.paint("36", &header));
        for op in slice {
            let _ = match op {
                Op::Same(l) => writeln!(out, " {l}"),
                Op::Del(l) => writeln!(out, "{}", style.paint("31", &format!("-{l}"))),
                Op::Add(l) => writeln!(out, "{}", style.paint("32", &format!("+{l}"))),
            };
        }
    }
    Some(out)
}

/// Classic LCS dynamic programming, after trimming the common prefix and
/// suffix (which is what typically dominates small edits to big files).
#[allow(clippy::many_single_char_names)]
fn diff_lines<'a>(a: &[&'a str], b: &[&'a str]) -> Option<Vec<Op<'a>>> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (ma, mb) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let (n, m) = (ma.len(), mb.len());
    if (n + 1).saturating_mul(m + 1) > MAX_CELLS {
        return None;
    }

    // table[i * w + j] = length of the LCS of ma[i..] and mb[j..]
    let w = m + 1;
    let mut table = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * w + j] = if ma[i] == mb[j] {
                table[(i + 1) * w + j + 1] + 1
            } else {
                table[(i + 1) * w + j].max(table[i * w + j + 1])
            };
        }
    }

    let mut ops: Vec<Op<'a>> = a[..prefix].iter().map(|l| Op::Same(l)).collect();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if ma[i] == mb[j] {
            ops.push(Op::Same(ma[i]));
            (i, j) = (i + 1, j + 1);
        } else if table[(i + 1) * w + j] >= table[i * w + j + 1] {
            ops.push(Op::Del(ma[i]));
            i += 1;
        } else {
            ops.push(Op::Add(mb[j]));
            j += 1;
        }
    }
    ops.extend(ma[i..].iter().map(|l| Op::Del(l)));
    ops.extend(mb[j..].iter().map(|l| Op::Add(l)));
    ops.extend(a[a.len() - suffix..].iter().map(|l| Op::Same(l)));
    Some(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Style = Style { color: false };

    #[test]
    fn identical_inputs_produce_nothing() {
        assert_eq!(unified("a\nb\n", "a\nb\n", 3, PLAIN).unwrap(), "");
    }

    #[test]
    fn single_line_change_with_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n";
        let new = "1\n2\n3\nfour\n5\n6\n7\n";
        let diff = unified(old, new, 1, PLAIN).unwrap();
        assert_eq!(diff, "@@ -3,3 +3,3 @@\n 3\n-4\n+four\n 5\n");
    }

    #[test]
    fn distant_changes_make_separate_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "A\nb\nc\nd\ne\nf\ng\nH\n";
        let diff = unified(old, new, 1, PLAIN).unwrap();
        assert_eq!(diff.matches("@@ -").count(), 2);
    }

    #[test]
    fn pure_insertion_and_deletion() {
        assert_eq!(unified("", "x\ny\n", 3, PLAIN).unwrap(), "@@ -1,0 +1,2 @@\n+x\n+y\n");
        assert_eq!(unified("x\n", "", 3, PLAIN).unwrap(), "@@ -1,1 +1,0 @@\n-x\n");
    }
}
