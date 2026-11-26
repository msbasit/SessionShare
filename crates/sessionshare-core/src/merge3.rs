//! Line-based diff/merge used to reconcile files edited on two PCs.
//!
//! With a common ancestor (the version both PCs last agreed on) this is a classic 3-way merge:
//! regions changed on only one side are taken automatically; regions changed differently on both
//! sides are left for the user. Without an ancestor every difference needs a decision.

use serde::{Deserialize, Serialize};
use similar::{capture_diff_slices, Algorithm, DiffTag};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    Local,
    Remote,
}

/// A run of lines in the merge. Lines keep their original line endings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Identical on both sides; no decision needed.
    pub stable: bool,
    /// Ancestor lines for this region (`None` when no common version is known).
    pub base: Option<Vec<String>>,
    pub local: Vec<String>,
    pub remote: Vec<String>,
    /// Side picked automatically because only that side changed this region.
    pub auto: Option<Pick>,
}

fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

fn owned(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|s| s.to_string()).collect()
}

fn stable(lines: &[&str]) -> Chunk {
    Chunk {
        stable: true,
        base: Some(owned(lines)),
        local: owned(lines),
        remote: owned(lines),
        auto: None,
    }
}

/// For each base line, the index of the line it is matched to in `other` (if unchanged).
fn matches(base: &[&str], other: &[&str]) -> Vec<Option<usize>> {
    let mut m = vec![None; base.len()];
    for op in capture_diff_slices(Algorithm::Myers, base, other) {
        let (tag, old, new) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            for (o, n) in old.zip(new) {
                m[o] = Some(n);
            }
        }
    }
    m
}

fn push_change(out: &mut Vec<Chunk>, base: &[&str], local: &[&str], remote: &[&str]) {
    if base.is_empty() && local.is_empty() && remote.is_empty() {
        return;
    }
    if local == remote {
        // Both PCs made the same change.
        out.push(stable(local));
        return;
    }
    let auto = if local == base {
        Some(Pick::Remote)
    } else if remote == base {
        Some(Pick::Local)
    } else {
        None
    };
    out.push(Chunk {
        stable: false,
        base: Some(owned(base)),
        local: owned(local),
        remote: owned(remote),
        auto,
    });
}

/// 3-way merge of `local` and `remote` against their common ancestor `base`.
pub fn merge3(base: &str, local: &str, remote: &str) -> Vec<Chunk> {
    let (o, a, b) = (split_lines(base), split_lines(local), split_lines(remote));
    let (ma, mb) = (matches(&o, &a), matches(&o, &b));
    let mut out = Vec::new();
    let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);

    loop {
        let mut n = 0;
        while i + n < o.len() && ma[i + n] == Some(j + n) && mb[i + n] == Some(k + n) {
            n += 1;
        }
        if n > 0 {
            out.push(stable(&o[i..i + n]));
            i += n;
            j += n;
            k += n;
            continue;
        }
        // Next base line that is unchanged on both sides (matches are monotonic, so >= j / k).
        match (i..o.len()).find(|&x| ma[x].is_some() && mb[x].is_some()) {
            Some(x) => {
                let (ja, kb) = (ma[x].unwrap_or(j), mb[x].unwrap_or(k));
                push_change(&mut out, &o[i..x], &a[j..ja], &b[k..kb]);
                i = x;
                j = ja;
                k = kb;
            }
            None => {
                push_change(&mut out, &o[i..], &a[j..], &b[k..]);
                break;
            }
        }
    }
    out
}

/// 2-way comparison when no common ancestor is known: every difference needs a decision.
pub fn merge2(local: &str, remote: &str) -> Vec<Chunk> {
    let (a, b) = (split_lines(local), split_lines(remote));
    let mut out = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &a, &b) {
        let (tag, l, r) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            out.push(stable(&a[l]));
        } else {
            out.push(Chunk {
                stable: false,
                base: None,
                local: owned(&a[l]),
                remote: owned(&b[r]),
                auto: None,
            });
        }
    }
    out
}

fn append(out: &mut String, lines: &[String]) {
    for line in lines {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(line);
    }
}

/// The merged text if every chunk is stable or was resolved automatically.
pub fn auto_result(chunks: &[Chunk]) -> Option<String> {
    let mut out = String::new();
    for c in chunks {
        match (c.stable, c.auto) {
            (true, _) | (false, Some(Pick::Local)) => append(&mut out, &c.local),
            (false, Some(Pick::Remote)) => append(&mut out, &c.remote),
            (false, None) => return None,
        }
    }
    Some(out)
}

pub fn unresolved_count(chunks: &[Chunk]) -> usize {
    chunks.iter().filter(|c| !c.stable && c.auto.is_none()).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_overlapping_edits_merge_automatically() {
        let base = "a\nb\nc\nd\n";
        let local = "A\nb\nc\nd\n";
        let remote = "a\nb\nc\nD\ne\n";
        let chunks = merge3(base, local, remote);
        assert_eq!(unresolved_count(&chunks), 0);
        assert_eq!(auto_result(&chunks).unwrap(), "A\nb\nc\nD\ne\n");
    }

    #[test]
    fn overlapping_edits_need_a_decision() {
        let chunks = merge3("a\nb\nc\n", "a\nLOCAL\nc\n", "a\nREMOTE\nc\n");
        assert_eq!(unresolved_count(&chunks), 1);
        assert!(auto_result(&chunks).is_none());
        let c = chunks.iter().find(|c| !c.stable).unwrap();
        assert_eq!(c.local, vec!["LOCAL\n"]);
        assert_eq!(c.remote, vec!["REMOTE\n"]);
        assert_eq!(c.base.as_deref(), Some(&["b\n".to_string()][..]));
    }

    #[test]
    fn same_change_on_both_sides_is_not_a_conflict() {
        let chunks = merge3("a\nb\n", "a\nX\n", "a\nX\n");
        assert_eq!(auto_result(&chunks).unwrap(), "a\nX\n");
    }

    #[test]
    fn two_way_marks_every_difference_unresolved() {
        let chunks = merge2("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(unresolved_count(&chunks), 2);
        assert!(chunks[0].stable);
    }

    #[test]
    fn missing_trailing_newline_is_preserved() {
        let chunks = merge3("a\nb", "a\nb\nc", "z\na\nb");
        assert_eq!(auto_result(&chunks).unwrap(), "z\na\nb\nc");
    }
}
