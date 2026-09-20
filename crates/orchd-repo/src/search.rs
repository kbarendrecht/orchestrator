//! Search one checkout's working tree: the lines that match, and the paths.
//!
//! **ripgrep's engine, linked rather than packaged.** The engine is a set of
//! libraries — `grep-searcher`, `grep-regex`, `ignore` — and linking them reaches
//! the binary's speed without shipping one. Measured on the 18,924-file monorepo
//! this daemon is developed against, one rare literal, warm cache: `git grep`
//! 549ms, `git grep --untracked` 606ms, ripgrep's own binary 25ms, this 28ms.
//! That is the whole argument. `git grep` was the first answer here and it was
//! measured on *this* checkout, 247 files, where everything is 9ms and nothing is
//! distinguishable — the mistake `CLAUDE.md` names by the name of this repo.
//!
//! **The walk honours `.gitignore` and nothing else does the honouring.** So an
//! untracked file an agent wrote ten seconds ago is found, which `git grep` needs
//! `--untracked` and 57ms for, and `target/` is not walked at all.
//!
//! **Hidden files are searched on purpose, and `.git` is skipped by hand.**
//! ripgrep skips dotfiles by default and gets `.git` for free as a consequence.
//! That default would hide `.githooks/pre-commit` and every workflow under
//! `.github/`, which in this repo is where a fair amount of the interesting text
//! lives. So `hidden(false)`, and the one directory that must not be walked is
//! named — without it the walk reads every loose object in the repository.
//!
//! **Main's tree contains every worktree, so main excludes them.** The same §2
//! rule `git::status` and the changed-files pane already hold: without the
//! prefix, a search in main answers with every sibling session's work, and the
//! same file appears once per worktree that holds a copy. A worktree excludes
//! nothing, because it contains nothing but itself.
//!
//! **There is no cancellation, deliberately.** The caps below bound the work and
//! the measurement above bounds the time, so a superseded query wastes
//! milliseconds; the page drops the answer with `AbortController`. A stop flag
//! wired through the walk would be a parameter with no caller.

use anyhow::{Context, Result};
use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// Hits taken from any one file. A file that matches on every line is a
/// generated file or a lockfile, and twenty rows of it is already more than the
/// index can usefully show.
pub const MAX_PER_FILE: usize = 20;
/// Hits returned at all. Past this the answer is "narrow the query", and saying
/// so is the footer's job.
pub const MAX_TOTAL: usize = 400;
/// Paths returned by [`paths`]. The page matches them itself, so the list is
/// fetched once per workspace rather than per keystroke.
pub const MAX_PATHS: usize = 20_000;

/// What to look for. The three flags are the overlay's three toggles.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Query {
    pub pattern: String,
    /// The pattern is a regular expression. Off means it is a literal, which is
    /// the default because most queries are a symbol and a literal cannot fail
    /// on a stray bracket.
    #[serde(default)]
    pub regex: bool,
    /// Match whole words only.
    #[serde(default)]
    pub word: bool,
    /// A path filter, as a glob: `web/`, `*.rs`, `crates/**/api.rs`. A trailing
    /// slash is completed to `/**`, because "in this directory" is what a person
    /// typing it means.
    #[serde(default)]
    pub glob: Option<String>,
    /// Match the case written, rather than smart-casing it.
    ///
    /// The overlay never sets this — smart case is what a person typing a query
    /// wants. [`crate::symbols`] does, because a definition jump moves the cursor
    /// on the strength of there being exactly one hit, and folding `run_blocking`
    /// onto `RUN_BLOCKING` is a way to land somewhere nobody asked for.
    #[serde(default)]
    pub exact_case: bool,
}

/// One matching line. `col` and `len` are **byte** offsets within the line, the
/// same units `diff::Row::words` uses and for the same reason: they come from
/// Rust and the page converts, rather than the daemon guessing at UTF-16.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Hit {
    /// Relative to the workspace root, with `/` separators.
    pub path: String,
    /// 1-based, as every editor counts.
    pub line: u32,
    pub col: u32,
    pub len: u32,
    /// The matching line, trailing newline removed. The index does not draw it —
    /// the viewer below shows the file — but a hit with no text is impossible to
    /// assert about in a test, and the page needs the length to place the mark.
    pub text: String,
}

/// What a search answers. `truncated` is what makes the footer honest.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Matches {
    pub hits: Vec<Hit>,
    pub truncated: bool,
}

/// The workspace's files, for the name search.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Paths {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// `web/` means everything under `web`, which as a glob is `web/**`.
fn as_glob(raw: &str) -> String {
    let g = raw.trim();
    match g.strip_suffix('/') {
        Some(dir) => format!("{dir}/**"),
        None => g.to_string(),
    }
}

/// The walk both entry points share: `.gitignore` honoured, dotfiles kept,
/// `.git` refused, and main's worktrees dropped.
///
/// `exclude` is `Some(prefix)` for main only — the repo-relative worktrees subdir
/// from `Config::worktrees_subdir_str`, so a relocated layout is followed rather
/// than guessed at.
fn walker(root: &Path, exclude: Option<&str>, glob: Option<&str>) -> Result<WalkBuilder> {
    let mut w = WalkBuilder::new(root);
    let (at, skip) = (root.to_path_buf(), exclude.map(|e| e.to_string()));
    w.hidden(false).threads(0).filter_entry(move |e| {
        if e.file_name() == ".git" {
            return false;
        }
        // The prefix ends in `/`, and a directory's own relative path does not, so
        // the directory itself is matched with the slash put back.
        match &skip {
            Some(prefix) => !format!("{}/", rel(&at, e.path())).starts_with(prefix.as_str()),
            None => true,
        }
    });
    if let Some(g) = glob.map(as_glob).filter(|g| !g.is_empty()) {
        let mut ov = OverrideBuilder::new(root);
        ov.add(&g)
            .with_context(|| format!("{g} is not a path filter"))?;
        w.overrides(ov.build()?);
    }
    Ok(w)
}

/// `path` as the page names it: relative to the root, `/` separated.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every line in the tree that matches, capped and in a stable order.
///
/// **Blocking, and parallel.** Call it through `proc::run_blocking`: it owns a
/// thread pool for the duration and must not run on a tokio worker.
pub fn search(root: &Path, exclude: Option<&str>, q: &Query) -> Result<Matches> {
    if q.pattern.is_empty() {
        return Ok(Matches {
            hits: Vec::new(),
            truncated: false,
        });
    }
    // A literal is the default, so the pattern is escaped rather than trusted.
    // `case_smart` is the library's own rule — an uppercase letter anywhere makes
    // the search sensitive — rather than a second copy of that rule here.
    let pattern = if q.regex {
        q.pattern.clone()
    } else {
        regex::escape(&q.pattern)
    };
    let matcher = RegexMatcherBuilder::new()
        .case_smart(!q.exact_case)
        .word(q.word)
        .build(&pattern)
        .with_context(|| format!("{} is not a pattern", q.pattern))?;

    let found: Mutex<Vec<Hit>> = Mutex::new(Vec::new());
    let truncated = std::sync::atomic::AtomicBool::new(false);
    // Borrowed, not moved: one visitor is built per worker thread, and each needs
    // the same two.
    let (shared, stop) = (&found, &truncated);
    walker(root, exclude, q.glob.as_deref())?
        .build_parallel()
        .run(|| {
            let matcher = matcher.clone();
            let mut searcher = SearcherBuilder::new()
                // A match inside a PNG is never what was wanted.
                .binary_detection(BinaryDetection::quit(0))
                .line_number(true)
                .build();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let mut mine: Vec<Hit> = Vec::new();
                let path = rel(root, entry.path());
                // A file that cannot be read is not an error the search reports: a
                // dangling symlink or a file an agent deleted mid-walk would
                // otherwise fail the whole query.
                let _ = searcher.search_path(
                    &matcher,
                    entry.path(),
                    UTF8(|line, text| {
                        let body = text.strip_suffix('\n').unwrap_or(text);
                        let at = matcher.find(body.as_bytes()).ok().flatten();
                        mine.push(Hit {
                            path: path.clone(),
                            line: u32::try_from(line).unwrap_or(u32::MAX),
                            col: at.map_or(0, |m| m.start() as u32),
                            len: at.map_or(0, |m| (m.end() - m.start()) as u32),
                            text: body.to_string(),
                        });
                        // Stop reading this file once it has said enough. `Ok(false)`
                        // ends the search of *this* file only.
                        Ok(mine.len() < MAX_PER_FILE)
                    }),
                );
                if mine.is_empty() {
                    return WalkState::Continue;
                }
                let mut all = shared.lock().unwrap_or_else(|e| e.into_inner());
                all.append(&mut mine);
                if all.len() >= MAX_TOTAL {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    return WalkState::Quit;
                }
                WalkState::Continue
            })
        });

    let mut hits = found.into_inner().unwrap_or_else(|e| e.into_inner());
    /* **Sorted, because a parallel walk is not ordered and the index is a list a
    person moves a cursor down.** The same query run twice would otherwise
    return the same hits in a different order, and the row under the selection
    would change without anything having changed. */
    hits.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });
    let truncated = truncated.load(std::sync::atomic::Ordering::Relaxed) || hits.len() > MAX_TOTAL;
    hits.truncate(MAX_TOTAL);
    Ok(Matches { hits, truncated })
}

/// Every file in the tree, for the name search. Same walk, same rules.
///
/// Blocking, for the reason [`search`] is.
pub fn paths(root: &Path, exclude: Option<&str>) -> Result<Paths> {
    let mut out: Vec<String> = Vec::new();
    for entry in walker(root, exclude, None)?.build().flatten() {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            out.push(rel(root, entry.path()));
            if out.len() > MAX_PATHS {
                break;
            }
        }
    }
    let truncated = out.len() > MAX_PATHS;
    out.truncate(MAX_PATHS);
    out.sort();
    Ok(Paths {
        paths: out,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A tree with the shapes every assertion below turns on: a nested file, a
    /// dotfile directory, an ignored directory, an untracked file, and a name
    /// that differs from its neighbour only in case.
    fn tree(tag: &str) -> std::path::PathBuf {
        let dir = orchd_base::testutil::scratch_repo(tag);
        fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\nlet needle = 1;\n").unwrap();
        fs::write(
            dir.join("src/other.rs"),
            "// needle here\n// Needle capitalised\n",
        )
        .unwrap();
        fs::write(dir.join("target/built.rs"), "let needle = 2;\n").unwrap();
        fs::write(dir.join(".githooks/pre-commit"), "# needle in a hook\n").unwrap();
        fs::write(dir.join("README.md"), "a needless word\n").unwrap();
        dir
    }

    fn find(root: &std::path::Path, pattern: &str) -> Matches {
        search(
            root,
            None,
            &Query {
                pattern: pattern.into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// The four rules the walk exists to hold, in one assertion each: an ignored
    /// directory is not read, a dotfile directory is, `.git` is never walked, and
    /// an untracked file is found without anyone asking for it.
    ///
    /// The `.git` half is the one worth keeping: `hidden(false)` is what makes
    /// `.githooks` searchable, and it is also what would send the walk through
    /// every loose object in the repository.
    #[test]
    fn the_walk_reads_dotfiles_and_refuses_git_and_gitignore() {
        let dir = tree("search-walk");
        let found = find(&dir, "needle");
        let got: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert!(
            got.contains(&".githooks/pre-commit"),
            "a dotfile directory must be searched: {got:?}"
        );
        assert!(
            got.contains(&"src/main.rs"),
            "an untracked file must be found: {got:?}"
        );
        assert!(
            !got.iter().any(|p| p.starts_with("target/")),
            "an ignored directory must not be: {got:?}"
        );
        assert!(
            !got.iter().any(|p| p.starts_with(".git/")),
            ".git must never be walked: {got:?}"
        );
    }

    /// Smart case is the library's rule and this is what it means: a lower-case
    /// query matches both spellings, a query with an upper-case letter matches
    /// only its own.
    #[test]
    fn case_is_smart_in_both_directions() {
        let dir = tree("search-case");
        let loose = find(&dir, "needle");
        assert_eq!(
            loose
                .hits
                .iter()
                .filter(|h| h.path == "src/other.rs")
                .count(),
            2
        );
        let exact = find(&dir, "Needle");
        let only: Vec<u32> = exact
            .hits
            .iter()
            .filter(|h| h.path == "src/other.rs")
            .map(|h| h.line)
            .collect();
        assert_eq!(
            only,
            vec![2],
            "an upper-case letter makes the search sensitive"
        );
    }

    /// The default is a literal, which is the half that matters: a query full of
    /// regex punctuation must find that text rather than fail to compile.
    #[test]
    fn a_literal_query_is_not_a_pattern() {
        let dir = tree("search-literal");
        fs::write(dir.join("src/re.rs"), "let v = foo(x)[0];\n").unwrap();
        let hit = find(&dir, "foo(x)[0]");
        assert_eq!(
            hit.hits.len(),
            1,
            "a literal must match punctuation verbatim"
        );
        // And the same string as a regex is a different question, not a crash.
        let as_re = search(
            &dir,
            None,
            &Query {
                pattern: "foo(x)[0]".into(),
                regex: true,
                ..Default::default()
            },
        );
        assert!(as_re.is_err() || as_re.unwrap().hits.is_empty());
    }

    #[test]
    fn whole_word_refuses_the_longer_word() {
        let dir = tree("search-word");
        let all = find(&dir, "needle");
        assert!(
            all.hits.iter().any(|h| h.path == "README.md"),
            "`needless` contains `needle`"
        );
        let word = search(
            &dir,
            None,
            &Query {
                pattern: "needle".into(),
                word: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !word.hits.iter().any(|h| h.path == "README.md"),
            "whole word must refuse `needless`"
        );
    }

    /// A trailing slash means "in this directory", because that is what a person
    /// typing it means. A bare glob is left alone.
    #[test]
    fn the_path_filter_takes_a_directory_or_a_glob() {
        let dir = tree("search-glob");
        let in_src = search(
            &dir,
            None,
            &Query {
                pattern: "needle".into(),
                glob: Some("src/".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!in_src.hits.is_empty());
        assert!(
            in_src.hits.iter().all(|h| h.path.starts_with("src/")),
            "{:?}",
            in_src.hits
        );
        let only_md = search(
            &dir,
            None,
            &Query {
                pattern: "needle".into(),
                glob: Some("*.md".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            only_md
                .hits
                .iter()
                .map(|h| h.path.as_str())
                .collect::<Vec<_>>(),
            vec!["README.md"]
        );
    }

    /// **The order is the contract, not a side effect.** The walk is parallel, so
    /// without the sort the same query returns the same hits in a different order
    /// and the row under the cursor changes while nothing has changed.
    #[test]
    fn the_same_query_answers_in_the_same_order() {
        let dir = tree("search-order");
        for i in 0..40 {
            fs::write(dir.join(format!("f{i:02}.txt")), "needle\nneedle\n").unwrap();
        }
        let first = find(&dir, "needle").hits;
        for _ in 0..6 {
            assert_eq!(
                find(&dir, "needle").hits,
                first,
                "a parallel walk must still answer in one order"
            );
        }
        let sorted = {
            let mut c = first.clone();
            c.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
            c
        };
        assert_eq!(first, sorted, "and that order is by path, then line");
    }

    /// `col` and `len` are byte offsets into the line, which is what lets the
    /// viewer mark the match without matching again. A non-ASCII prefix is the
    /// case that catches a char-vs-byte slip.
    #[test]
    fn the_offsets_are_bytes_into_the_line() {
        let dir = orchd_base::testutil::scratch_repo("search-offsets");
        fs::write(dir.join("u.txt"), "héllo needle there\n").unwrap();
        let hit = find(&dir, "needle")
            .hits
            .into_iter()
            .next()
            .expect("one hit");
        assert_eq!(hit.len, 6);
        assert_eq!(
            &hit.text.as_bytes()[hit.col as usize..(hit.col + hit.len) as usize],
            b"needle",
            "the offsets must index the line's bytes"
        );
        assert_eq!(hit.col, 7, "`héllo ` is 7 bytes, not 6 characters");
    }

    /// Both caps, and the flag that makes the footer honest about them.
    #[test]
    fn the_caps_hold_and_say_so() {
        let dir = orchd_base::testutil::scratch_repo("search-caps");
        let many = "needle\n".repeat(MAX_PER_FILE + 30);
        fs::write(dir.join("many.txt"), &many).unwrap();
        let one = find(&dir, "needle");
        assert_eq!(
            one.hits.len(),
            MAX_PER_FILE,
            "one file may not flood the index"
        );
        assert!(!one.truncated, "a per-file cap is not a truncated answer");

        for i in 0..(MAX_TOTAL / MAX_PER_FILE + 4) {
            fs::write(dir.join(format!("m{i:03}.txt")), &many).unwrap();
        }
        let lots = find(&dir, "needle");
        assert_eq!(lots.hits.len(), MAX_TOTAL);
        assert!(lots.truncated, "hitting the total cap must be admitted");
    }

    /// A binary file is skipped rather than spilling bytes into the index.
    #[test]
    fn binary_files_are_not_searched() {
        let dir = orchd_base::testutil::scratch_repo("search-binary");
        fs::write(dir.join("t.txt"), "needle\n").unwrap();
        fs::write(dir.join("b.bin"), b"needle\x00\x00needle\n").unwrap();
        let found = find(&dir, "needle");
        let got: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(got, vec!["t.txt"]);
    }

    /// The name search walks by the same rules, so what it lists and what a
    /// content search can reach are the same set of files.
    #[test]
    fn paths_lists_what_search_can_reach() {
        let dir = tree("search-paths");
        let got = paths(&dir, None).unwrap().paths;
        assert!(got.contains(&".githooks/pre-commit".to_string()), "{got:?}");
        assert!(got.contains(&"src/main.rs".to_string()), "{got:?}");
        assert!(!got.iter().any(|p| p.starts_with("target/")), "{got:?}");
        assert!(!got.iter().any(|p| p.starts_with(".git/")), "{got:?}");
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(
            got, sorted,
            "the list is matched in the page, so it arrives ordered"
        );
    }

    /// **Main's tree holds every worktree, so main must not answer for them.**
    /// The same §2 rule the changed-files pane holds: without the prefix, one
    /// search in main returns every sibling session's copy of the same file, and
    /// a worktree — which excludes nothing — still sees only itself.
    #[test]
    fn main_excludes_its_worktrees_and_a_worktree_excludes_nothing() {
        let dir = tree("search-worktrees");
        fs::create_dir_all(dir.join(".worktrees/sibling/src")).unwrap();
        fs::write(
            dir.join(".worktrees/sibling/src/main.rs"),
            "let needle = 3;\n",
        )
        .unwrap();

        let all = search(
            &dir,
            None,
            &Query {
                pattern: "needle".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            all.hits.iter().any(|h| h.path.starts_with(".worktrees/")),
            "with no prefix the walk reaches them, which is what makes the next assertion mean something"
        );

        let main = search(
            &dir,
            Some(".worktrees/"),
            &Query {
                pattern: "needle".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !main.hits.iter().any(|h| h.path.starts_with(".worktrees/")),
            "main must not answer with a sibling worktree's work: {:?}",
            main.hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(
            main.hits.iter().any(|h| h.path == "src/main.rs"),
            "and must still answer for itself"
        );

        let listed = paths(&dir, Some(".worktrees/")).unwrap().paths;
        assert!(
            !listed.iter().any(|p| p.starts_with(".worktrees/")),
            "the name search follows the same rule"
        );
    }

    /// An empty query is not an error and not every line in the repository.
    #[test]
    fn an_empty_query_finds_nothing() {
        let dir = tree("search-empty");
        assert!(find(&dir, "").hits.is_empty());
    }
}
