//! Reading what git says changed, in the shapes the daemon needs.
//!
//! **What is left of the batch's patch machinery.** The batch applied proposed
//! diffs itself — three `git apply` passes, a fold per thread, a manual phase —
//! and that whole flow is gone: a review session's agent writes the code in its
//! own worktree, so the daemon never applies a patch. What survived is what other
//! modules read git through: the one `--numstat -z` parser, and the dirty-path
//! list the review gate refuses on.

use anyhow::Result;
use std::path::Path;

/// One `--numstat -z` record. `from` is set for a rename: the path the change
/// deleted to make `path`.
#[derive(Debug, PartialEq, Eq)]
pub struct NumstatRow {
    pub path: String,
    pub from: Option<String>,
    pub added: u32,
    pub deleted: u32,
    /// git printed `-` for both counts. Kept as a flag because the counts are
    /// surfaced as zero, and a binary file is otherwise indistinguishable from a
    /// change that added and removed nothing — which the diff pane has to tell
    /// apart, since it collapses one and renders the other.
    pub binary: bool,
}

/// The one numstat parser, for `git apply --numstat -z` and `git diff --numstat -z`
/// alike — it used to be written twice, once per flag shape.
///
/// `-z` so paths come through raw, matching `git::status`, and so a rename is
/// visible at all: the plain form prints only the new path. The record shapes
/// differ: an ordinary entry is `added\tdeleted\tpath\0`, a rename is
/// `added\tdeleted\t\0old\0new\0` — the path field is empty and the two paths
/// follow as their own records. A binary file prints `-\t-`, surfaced as zero
/// counts rather than dropped: it is still being written.
pub fn parse_numstat_z(raw: &str) -> Vec<NumstatRow> {
    let mut fields = raw.split('\0');
    let mut rows = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let mut cols = field.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        let n = |s: &str| s.parse().unwrap_or(0);
        let (path, from) = if path.is_empty() {
            let old = fields.next().map(str::to_string);
            match fields.next() {
                Some(new) => (new.to_string(), old),
                None => continue,
            }
        } else {
            (path.to_string(), None)
        };
        rows.push(NumstatRow {
            path,
            from,
            added: n(added),
            deleted: n(deleted),
            binary: added == "-" || deleted == "-",
        });
    }
    rows
}

/// Everything `git status` calls changed, staged or not, tracked or not.
///
/// The one source of path strings for the manual phase. A rename appears once, as its
/// new path, because that is what `--porcelain=v2` reports.
pub fn dirty_paths(cwd: &Path) -> Result<Vec<String>> {
    let set = orchd_base::git::status(cwd, None, orchd_base::git::Untracked::Each)?;
    Ok(set
        .staged
        .iter()
        .chain(set.unstaged.iter())
        .chain(set.untracked.iter())
        .map(|f| f.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_parses_counts_keeps_binary_paths_and_sees_both_halves_of_a_rename() {
        let rows = parse_numstat_z(concat!(
            "2\t1\tsrc/f.rs\0",
            "-\t-\tlogo.png\0",
            "0\t0\t\0old.rs\0new.rs\0"
        ));
        assert_eq!(
            rows,
            vec![
                NumstatRow {
                    path: "src/f.rs".into(),
                    from: None,
                    added: 2,
                    deleted: 1,
                    binary: false
                },
                // `-\t-` counts as zero lines, and says so: the flag is what tells
                // a binary file from a change that added and removed nothing.
                NumstatRow {
                    path: "logo.png".into(),
                    from: None,
                    added: 0,
                    deleted: 0,
                    binary: true
                },
                NumstatRow {
                    path: "new.rs".into(),
                    from: Some("old.rs".into()),
                    added: 0,
                    deleted: 0,
                    binary: false
                },
            ]
        );
    }
}
