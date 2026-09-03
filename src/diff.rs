use anyhow::{bail, Result};
use serde::Serialize;
use std::path::Path;

use crate::git::git;

/// Eager cap: past this a file is listed but its hunks are only fetched on
/// explicit request (§5).
pub const EAGER_LINE_CAP: usize = 2000;

// ---------------------------------------------------------------------------
// Base
// ---------------------------------------------------------------------------

/// What the diff is taken against (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Base {
    /// Everything done on the branch, including uncommitted work.
    #[default]
    Upstream,
    /// Uncommitted work only.
    Head,
    /// The PR's own base branch.
    PrBase,
}

/// Resolve a base to a concrete commit.
///
/// Two-dot against the merge-base **commit**, not the ref, or develop's own
/// commits appear as your deletions (§5). This matches how `worktree-create`
/// bases branches and how `gh pr create` resolves the PR base, so the diff view
/// and the PR agree.
pub fn resolve_base(cwd: &Path, base: Base, upstream: &str, pr_base: Option<&str>) -> Result<String> {
    match base {
        Base::Upstream => Ok(git(cwd, &["merge-base", upstream, "HEAD"])?.trim().to_string()),
        Base::Head => Ok("HEAD".to_string()),
        Base::PrBase => {
            let r = pr_base.unwrap_or(upstream);
            // The PR base lives on upstream, not on the fork.
            let candidates = [format!("upstream/{r}"), r.to_string()];
            for c in &candidates {
                if let Ok(out) = git(cwd, &["merge-base", c, "HEAD"]) {
                    return Ok(out.trim().to_string());
                }
            }
            bail!("could not resolve the PR base {r}")
        }
    }
}

// ---------------------------------------------------------------------------
// File list
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub struct DiffFile {
    pub path: String,
    /// Verbatim from `--name-status`: M, A, D, R…, C…
    pub status: String,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
    /// Whether the client should fetch hunks without being asked.
    pub eager: bool,
    /// Present for renames.
    pub old_path: Option<String>,
}

impl DiffFile {
    /// A file git has never seen. `git diff` cannot report one, so the pane's
    /// list would be missing exactly the files a session just created.
    ///
    /// No line counts: counting them means reading every new file on every
    /// reconcile, and an untracked file is entirely new by definition — the
    /// number would only ever say "all of it".
    pub fn untracked(f: &crate::model::ChangedFile) -> Self {
        DiffFile {
            path: f.path.clone(),
            status: "?".to_string(),
            added: 0,
            deleted: 0,
            binary: false,
            // Nothing to diff against, so there are no hunks to fetch.
            eager: false,
            old_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffSummary {
    pub base: String,
    pub files: Vec<DiffFile>,
    pub added: u32,
    pub deleted: u32,
}

/// `--numstat` first so the file list renders immediately; hunks come later,
/// per file (§5).
pub fn summary(cwd: &Path, base: &str) -> Result<DiffSummary> {
    let numstat = git(cwd, &["diff", "--numstat", base])?;
    let namestatus = git(cwd, &["diff", "--name-status", base])?;

    let mut statuses = std::collections::HashMap::new();
    for line in namestatus.lines() {
        let mut parts = line.split('\t');
        let Some(code) = parts.next() else { continue };
        let Some(first) = parts.next() else { continue };
        // Renames and copies carry both paths; the new one is what is shown.
        let path = parts.next().unwrap_or(first);
        statuses.insert(path.to_string(), (code.to_string(), parts_old(code, first)));
    }

    let mut files = Vec::new();
    let (mut total_add, mut total_del) = (0u32, 0u32);
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let a = parts.next().unwrap_or("0");
        let d = parts.next().unwrap_or("0");
        let Some(path) = parts.next() else { continue };
        // Binary files report "-" for both counts.
        let binary = a == "-" || d == "-";
        let added: u32 = a.parse().unwrap_or(0);
        let deleted: u32 = d.parse().unwrap_or(0);
        total_add += added;
        total_del += deleted;
        let (status, old_path) = statuses
            .get(path)
            .cloned()
            .unwrap_or_else(|| ("M".to_string(), None));
        files.push(DiffFile {
            path: path.to_string(),
            status,
            added,
            deleted,
            binary,
            // Binary and generated content is collapsed rather than rendered.
            eager: !binary && (added + deleted) as usize <= EAGER_LINE_CAP,
            old_path,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(DiffSummary {
        base: base.to_string(),
        files,
        added: total_add,
        deleted: total_del,
    })
}

fn parts_old(code: &str, first: &str) -> Option<String> {
    if code.starts_with('R') || code.starts_with('C') {
        Some(first.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Hunks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    Context,
    Del,
    Add,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
    /// Byte ranges within `text` that actually differ, for word-level
    /// highlighting. Computed here so the browser's main thread never pays for
    /// it (§5). Non-overlapping and in ascending order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hunk {
    pub old_start: u32,
    pub new_start: u32,
    pub header: String,
    /// Unchanged lines skipped before this hunk, so the client can render a
    /// fold bar and expand on click.
    pub gap_before: u32,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub hunks: Vec<Hunk>,
    pub binary: bool,
    pub truncated: bool,
}

/// A file as it exists at `base`, for the read-only left pane in edit mode.
pub fn show_at(cwd: &Path, base: &str, path: &str) -> Result<String> {
    // `--` so a path that looks like a rev is still treated as a path.
    git(cwd, &["show", &format!("{base}:{path}")])
        .or_else(|_| Ok(String::new()))
}

/// Hunks for one file. `context` widens `-U`, which is how expand-on-click is
/// served without a second diff format.
pub fn file_diff(cwd: &Path, base: &str, path: &str, context: u32) -> Result<FileDiff> {
    let ctx = format!("-U{context}");
    let out = git(
        cwd,
        &["diff", &ctx, base, "--", path],
    )?;
    Ok(parse_unified(path, &out))
}

fn parse_unified(path: &str, raw: &str) -> FileDiff {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut binary = false;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut last_old_end = 1u32;

    for line in raw.lines() {
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            binary = true;
            continue;
        }
        if line.starts_with("@@") {
            let (os, ns) = parse_hunk_header(line);
            old_line = os;
            new_line = ns;
            let gap = os.saturating_sub(last_old_end);
            hunks.push(Hunk {
                old_start: os,
                new_start: ns,
                header: line.to_string(),
                gap_before: gap,
                rows: Vec::new(),
            });
            continue;
        }
        let Some(h) = hunks.last_mut() else { continue };

        // Diff metadata lines. `---`/`+++` only appear before the first hunk,
        // so anything reaching here with those prefixes is real content.
        if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("similarity ")
            || line.starts_with("rename ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
        {
            continue;
        }
        if line == "\\ No newline at end of file" {
            continue;
        }

        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (RowKind::Add, &line[1..]),
            Some(b'-') => (RowKind::Del, &line[1..]),
            Some(b' ') => (RowKind::Context, &line[1..]),
            // An empty line in the body is an unchanged empty line.
            None => (RowKind::Context, ""),
            _ => continue,
        };

        let (old, new) = match kind {
            RowKind::Context => (Some(old_line), Some(new_line)),
            RowKind::Del => (Some(old_line), None),
            RowKind::Add => (None, Some(new_line)),
        };
        h.rows.push(Row {
            kind,
            old,
            new,
            text: text.to_string(),
            words: Vec::new(),
        });
        // A side's counter moves only when the row exists on that side.
        if old.is_some() {
            old_line += 1;
        }
        if new.is_some() {
            new_line += 1;
        }
        last_old_end = old_line;
    }

    for h in &mut hunks {
        mark_words(&mut h.rows);
    }

    FileDiff {
        path: path.to_string(),
        hunks,
        binary,
        truncated: false,
    }
}

fn parse_hunk_header(line: &str) -> (u32, u32) {
    // @@ -old,count +new,count @@ optional section heading
    //
    // **Only the text between the two `@@`.** What follows the second one is git's
    // funcname heading, which is a line of the file and therefore arbitrary code —
    // and scanning the whole line read tokens out of it: git's default heading for
    // Rust is the enclosing `fn` signature, so `-> Result<()>` parsed as an old
    // range of `>` (nothing, so 0, floored to 1) and `count += 1;` as a new range
    // of `= 1;`. Every gutter number and gap fold in the hunk was then wrong, on
    // essentially every hunk inside a function with a return type. The old test
    // only used a `class Foo {` heading, which carries no `-` or `+`.
    let ranges = line
        .strip_prefix("@@")
        .and_then(|rest| rest.split_once("@@"))
        .map_or(line, |(ranges, _heading)| ranges);
    let mut old = 0;
    let mut new = 0;
    for tok in ranges.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            old = rest.split(',').next().unwrap_or("0").parse().unwrap_or(0);
        } else if let Some(rest) = tok.strip_prefix('+') {
            new = rest.split(',').next().unwrap_or("0").parse().unwrap_or(0);
        }
    }
    (old.max(1), new.max(1))
}

/// Pair each run of deletions with the following run of additions and mark the
/// words that actually differ.
///
/// This is the single biggest readability win over GitHub's line granularity
/// (§5), and pairing by position within the run is what makes a one-word edit
/// on line three of a block highlight only that word.
fn mark_words(rows: &mut [Row]) {
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind != RowKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < rows.len() && rows[i].kind == RowKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < rows.len() && rows[i].kind == RowKind::Add {
            i += 1;
        }
        let dels = del_start..add_start;
        let adds = add_start..i;

        // Only pair when the two runs line up; a 3-for-1 replacement has no
        // meaningful per-line pairing and highlighting it all adds noise.
        if dels.len() != adds.len() {
            continue;
        }
        for k in 0..dels.len() {
            let (a, b) = (del_start + k, add_start + k);
            let (dw, aw) = word_ranges(&rows[a].text, &rows[b].text);
            rows[a].words = dw;
            rows[b].words = aw;
        }
    }
}

/// Token-level diff of two lines, returned as byte ranges into each.
///
/// A single span covering everything between the common prefix and suffix is
/// cheap but wrong for a line with two separate edits: it paints the untouched
/// middle as changed. This runs a real LCS over the tokens so each edit gets
/// its own range.
fn word_ranges(a: &str, b: &str) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let ta = tokenize(a);
    let tb = tokenize(b);

    // Common prefix and suffix first. Most edits are a small change inside an
    // otherwise identical line, and trimming keeps the LCS table small.
    let mut pre = 0;
    while pre < ta.len() && pre < tb.len() && ta[pre].1 == tb[pre].1 {
        pre += 1;
    }
    let mut suf = 0;
    while suf < ta.len() - pre
        && suf < tb.len() - pre
        && ta[ta.len() - 1 - suf].1 == tb[tb.len() - 1 - suf].1
    {
        suf += 1;
    }

    let a_mid = &ta[pre..ta.len() - suf];
    let b_mid = &tb[pre..tb.len() - suf];
    if a_mid.is_empty() && b_mid.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let whole = |toks: &[(usize, &str)]| -> Vec<(usize, usize)> {
        match (toks.first(), toks.last()) {
            (Some(f), Some(l)) => vec![(f.0, l.0 + l.1.len())],
            _ => Vec::new(),
        }
    };

    // A minified bundle on one line would make the table enormous; one span is
    // a fine answer there.
    const MAX: usize = 400;
    if a_mid.len() > MAX || b_mid.len() > MAX {
        return (whole(a_mid), whole(b_mid));
    }
    // One side empty means a pure insertion or deletion.
    if a_mid.is_empty() || b_mid.is_empty() {
        return (whole(a_mid), whole(b_mid));
    }

    let (n, m) = (a_mid.len(), b_mid.len());
    let mut dp = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = if a_mid[i].1 == b_mid[j].1 {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }

    let mut a_out: Vec<(usize, usize)> = Vec::new();
    let mut b_out: Vec<(usize, usize)> = Vec::new();
    let push = |out: &mut Vec<(usize, usize)>, tok: (usize, &str)| {
        let (s, e) = (tok.0, tok.0 + tok.1.len());
        // Adjacent differing tokens read as one edit, not several.
        match out.last_mut() {
            Some(last) if last.1 == s => last.1 = e,
            _ => out.push((s, e)),
        }
    };

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a_mid[i].1 == b_mid[j].1 {
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            push(&mut a_out, a_mid[i]);
            i += 1;
        } else {
            push(&mut b_out, b_mid[j]);
            j += 1;
        }
    }
    while i < n {
        push(&mut a_out, a_mid[i]);
        i += 1;
    }
    while j < m {
        push(&mut b_out, b_mid[j]);
        j += 1;
    }

    (a_out, b_out)
}

/// Split into words, punctuation and whitespace runs, keeping byte offsets.
fn tokenize(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let c = bytes[i];
        let class = |b: u8| {
            if b.is_ascii_alphanumeric() || b == b'_' {
                0
            } else if b.is_ascii_whitespace() {
                1
            } else {
                2
            }
        };
        let k = class(c);
        // Punctuation is one token each, so `->` and `.` do not glue together.
        if k == 2 {
            i += 1;
        } else {
            while i < bytes.len() && class(bytes[i]) == k {
                i += 1;
            }
        }
        // Multi-byte UTF-8 is not split: advance to the next char boundary.
        while i < bytes.len() && !s.is_char_boundary(i) {
            i += 1;
        }
        out.push((start, &s[start..i]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_only_the_changed_word() {
        let (d, a) = word_ranges("let total = price * qty;", "let total = price * count;");
        assert_eq!(d.len(), 1);
        assert_eq!(a.len(), 1);
        let (ds, de) = d[0];
        assert_eq!(&"let total = price * qty;"[ds..de], "qty");
        let (as_, ae) = a[0];
        assert_eq!(&"let total = price * count;"[as_..ae], "count");
    }

    #[test]
    fn two_separate_edits_get_two_ranges() {
        let a = "foo(alpha, 1, beta)";
        let b = "foo(gamma, 1, delta)";
        let (d, _) = word_ranges(a, b);
        assert_eq!(d.len(), 2, "expected two ranges, got {d:?}");
        assert_eq!(&a[d[0].0..d[0].1], "alpha");
        assert_eq!(&a[d[1].0..d[1].1], "beta");
    }

    #[test]
    fn a_pure_insertion_marks_only_the_new_side() {
        let (d, a) = word_ranges("call(x)", "call(x, y)");
        assert!(d.is_empty(), "nothing was deleted, got {d:?}");
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn ranges_are_ordered_and_do_not_overlap() {
        let a = "a1 b2 c3 d4 e5";
        let b = "aX b2 cY d4 eZ";
        let (d, _) = word_ranges(a, b);
        for w in d.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlapping ranges {d:?}");
        }
        assert!(d.iter().all(|(s, e)| s < e && *e <= a.len()));
    }

    #[test]
    fn identical_lines_highlight_nothing() {
        let (d, a) = word_ranges("same", "same");
        assert!(d.is_empty() && a.is_empty());
    }

    #[test]
    fn tokenizer_keeps_multibyte_intact() {
        let toks = tokenize("héllo wörld");
        let joined: String = toks.iter().map(|(_, t)| *t).collect();
        assert_eq!(joined, "héllo wörld");
    }

    #[test]
    fn parses_line_numbers_from_a_unified_diff() {
        let raw = "\
diff --git a/x b/x
index 111..222 100644
--- a/x
+++ b/x
@@ -10,4 +10,4 @@ class Foo
 context one
-old line
+new line
 context two
";
        let d = parse_unified("x", raw);
        assert_eq!(d.hunks.len(), 1);
        let rows = &d.hunks[0].rows;
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].old, Some(10));
        assert_eq!(rows[0].new, Some(10));
        assert_eq!(rows[1].kind, RowKind::Del);
        assert_eq!(rows[1].old, Some(11));
        assert_eq!(rows[1].new, None);
        assert_eq!(rows[2].kind, RowKind::Add);
        assert_eq!(rows[2].new, Some(11));
        // The paired del/add gets word ranges.
        assert!(!rows[1].words.is_empty());
        assert!(!rows[2].words.is_empty());
        // Context after a replacement keeps both sides in step.
        assert_eq!(rows[3].old, Some(12));
        assert_eq!(rows[3].new, Some(12));
    }

    /// The funcname heading is a line of the file, so it can hold anything — and
    /// git's default heading for Rust is the enclosing `fn` signature. Scanning the
    /// whole header read `->` as an old range and `+=` as a new one, which put the
    /// wrong number on every row of the hunk. The `class Foo` case above never
    /// caught it because that heading carries neither token.
    #[test]
    fn a_funcname_heading_is_not_parsed_as_a_range() {
        assert_eq!(
            parse_hunk_header("@@ -10,6 +10,7 @@ pub fn foo() -> Result<()> {"),
            (10, 10),
            "`->` in a Rust signature is not an old range"
        );
        assert_eq!(
            parse_hunk_header("@@ -40,6 +40,7 @@ count += 1;"),
            (40, 40),
            "`+=` in the heading is not a new range"
        );
        // A single-line hunk has no count, and a heading may be absent entirely.
        assert_eq!(parse_hunk_header("@@ -3 +7 @@"), (3, 7));
        assert_eq!(parse_hunk_header("@@ -10,4 +10,4 @@ class Foo"), (10, 10));
    }

    #[test]
    fn records_the_gap_between_hunks_for_the_fold_bar() {
        let raw = "\
@@ -1,2 +1,2 @@
 a
-b
+B
@@ -40,2 +40,2 @@
 c
-d
+D
";
        let d = parse_unified("x", raw);
        assert_eq!(d.hunks.len(), 2);
        assert_eq!(d.hunks[0].gap_before, 0);
        // Lines 3..39 are unchanged and not shown.
        assert!(d.hunks[1].gap_before > 30);
    }

    #[test]
    fn unbalanced_runs_are_not_word_paired() {
        // Three lines replaced by one: per-line pairing would be noise.
        let raw = "\
@@ -1,4 +1,2 @@
-one
-two
-three
+single
 tail
";
        let d = parse_unified("x", raw);
        assert!(d.hunks[0].rows.iter().all(|r| r.words.is_empty()));
    }

    #[test]
    fn a_binary_file_is_flagged_not_parsed() {
        let d = parse_unified("img.png", "Binary files a/img.png and b/img.png differ\n");
        assert!(d.binary);
        assert!(d.hunks.is_empty());
    }
}
