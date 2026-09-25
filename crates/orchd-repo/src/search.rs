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
//! `--untracked` and 57ms for, and `target/` is not walked — *unless it is small*.
//! `small_ignored_dirs` is the one exception and it is measured rather than
//! configured: an ignored directory under `IGNORED_CAP` files is searched anyway,
//! because that is where a repo keeps the notes an agent writes. A fixture's
//! one-file `target/` therefore does turn up, which is the reading the tests
//! carry.
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
    /// The matching line, trailing newline removed. The index draws it, cut to a
    /// window around the match, which is what lets two hits in one file be told
    /// apart without moving the cursor onto each of them.
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

/// How deep the scan for ignored directories looks.
///
/// **Two, and the third level is what makes it slow rather than better.**
/// Measured on the monorepo this is developed against: depth 1 offers git 30
/// directories, depth 2 offers 268 and depth 3 offers 2,343 — and `check-ignore`
/// costs 0.02s, 0.10s and 0.71s on those. What the third level adds is
/// `libraries/*/dist` and `tests/e2e/blob-report`: build output, which the cap
/// below would drop anyway. Notes live at the top or one under it.
const IGNORED_SCAN_DEPTH: usize = 2;
/// How long git gets to answer which paths are ignored.
///
/// A bound rather than a hope: this runs on a blocking thread that a search is
/// waiting on, and the failure it guards is a wedged child rather than a slow
/// repo. Generous next to the 0.10s it takes over 268 directories.
const IGNORED_GIT_TIMEOUT: u64 = 10;
/// Directories offered to git at once.
///
/// **A bound on the question, not on the tree.** `dirs_to_depth` answers with
/// whatever the first two levels hold, and that is 268 on the monorepo this was
/// measured against — but nothing in a repo's shape promises a number like that,
/// and a list long enough to matter is one this feature should decline rather
/// than spend a search on. Shallowest first, which is the order the scan already
/// produces, so what a cap drops is the deepest and least likely to be notes.
const IGNORED_CANDIDATES: usize = 2_000;
/// Files an ignored directory may hold and still be searched.
///
/// The measurement this comes from, on the same repo: `.plan` is 25 files and
/// `.idea` is 16, while `.playwright-mcp` is 524, `var` 22,656, `vendor` 30,749
/// and `node_modules` 143,465. Notes are tens of files and output starts in the
/// hundreds, so the line is drawn between them — and it is drawn low on purpose,
/// because `MAX_PATHS` is 20,000 and this repo already lists 19,043. A generous
/// cap does not merely add noise, it truncates the list and loses real files.
const IGNORED_CAP: usize = 200;

/// The directories git ignores that are small enough to search anyway, as roots
/// of their own.
///
/// **A `.gitignore` is the right default and the wrong answer for one directory.**
/// Without it the walk is 3.2 million files against 19,043 — and `MAX_PATHS` is
/// 20,000, so an unfiltered list would not merely be slow, it would push every
/// real file out. With it, the notes an agent writes into an ignored folder
/// cannot be found at all, which is how this arrived: "why can't I find `.plan`".
///
/// **Size is the only signal that separates the two, and only after the ignore
/// verdict.** A cap on its own abandons `src`, `tests` and `libraries` too —
/// measured, and the reason this is not simply a size rule. So: ask git which
/// directories are ignored, then keep the small ones.
///
/// No configuration, deliberately. A setting would have to be found and
/// understood before a file could be found, which is the flow this exists to fix.
fn small_ignored_dirs(root: &Path, exclude: Option<&str>) -> Vec<std::path::PathBuf> {
    /* **A stateless module grows one piece of state here, and the measurement is
    why.** The discovery is ~0.2s — `check-ignore` over 268 directories plus a
    capped probe of each ignored one — against a walk of 0.11s, and the content
    search would pay it per keystroke. Ignored directories change about as often
    as a `.gitignore` is edited, so a short life costs correctness nothing and
    gives the search its speed back: 0.25s to 0.13s, measured. */
    /* **Keyed on the exclude as well as the root, because the answer depends on
    both.** Main excludes its worktrees and a worktree excludes nothing, so one
    tree is asked two different questions — and keying on the path alone let
    main reuse a worktree's answer, which is how an ignored build directory
    inside a sibling session's worktree gets added back as a root of main's own
    search. That is the leak `exclude` exists to prevent, and this file's own
    test asks both ways of one tempdir microseconds apart, well inside the
    window. */
    static SEEN: std::sync::OnceLock<Mutex<Remembered>> = std::sync::OnceLock::new();
    remembered(&SEEN, root, exclude, || {
        discover_small_ignored_dirs(root, exclude)
    })
}

/// One tree's answer to one question, for [`IGNORED_TTL`].
type Key = (std::path::PathBuf, Option<String>);
type Remembered = std::collections::HashMap<Key, (std::time::Instant, Vec<std::path::PathBuf>)>;

/// The memo both ignore lists share: ask once per tree, trust it for a while.
///
/// Written once because it is asked twice — the small ignored directories and the
/// loose ignored files are two git questions with the same cost, the same key and
/// the same reason to be cached. Each caller brings its own `static`, so the two
/// answers never share a slot.
///
/// The key carries `exclude` as well as the root for the reason
/// [`small_ignored_dirs`] gives: main excludes its worktrees and a worktree
/// excludes nothing, so one tree is asked two different questions.
fn remembered(
    store: &'static std::sync::OnceLock<Mutex<Remembered>>,
    root: &Path,
    exclude: Option<&str>,
    fresh: impl FnOnce() -> Vec<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let seen = store.get_or_init(|| Mutex::new(Remembered::new()));
    let key: Key = (root.to_path_buf(), exclude.map(str::to_string));
    if let Ok(map) = seen.lock() {
        if let Some((at, found)) = map.get(&key) {
            if at.elapsed() < IGNORED_TTL {
                return found.clone();
            }
        }
    }
    let found = fresh();
    if let Ok(mut map) = seen.lock() {
        // Nothing else evicts a workspace that is gone, so the answers that have
        // expired are dropped on the way past rather than kept for the life of
        // the daemon.
        map.retain(|_, (at, _)| at.elapsed() < IGNORED_TTL);
        map.insert(key, (std::time::Instant::now(), found.clone()));
    }
    found
}

/// The files git ignores that sit outside any ignored directory.
///
/// **A `.gitignore` names two different things and only one of them is volume.**
/// An ignored *directory* can hold a million files, which is what
/// [`small_ignored_dirs`] measures before letting one in. An ignored *file* costs
/// one row — and it is often the file you edit most: `.env`, `mise.local.toml`,
/// `compose.override.yaml`, `config/environments/local.yml`. Shift-Shift could
/// not find any of them, which is how this arrived.
///
/// Measured before it was written, because the cost is the whole argument: on the
/// 18,924-file monorepo this is developed against it is **15 files**, and in
/// `orchd`'s own tree **one**. Directories are still refused, so `node_modules`
/// and `target` are untouched by this.
///
/// **`--directory` is what makes it 15 rather than 3 million.** git collapses a
/// wholly ignored directory to one entry ending in `/`, and lists a file
/// individually only when its parent is *not* wholly ignored — which is exactly
/// the set this wants. Everything ending in `/` is dropped here; the directories
/// are [`small_ignored_dirs`]'s question, asked with its own cap.
fn loose_ignored_files(root: &Path, exclude: Option<&str>) -> Vec<std::path::PathBuf> {
    static SEEN: std::sync::OnceLock<Mutex<Remembered>> = std::sync::OnceLock::new();
    remembered(&SEEN, root, exclude, || {
        let argv: Vec<String> = [
            "git",
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let out = match orchd_base::proc::run_bounded_with_input(
            root,
            IGNORED_GIT_TIMEOUT,
            &argv,
            "asking git which files are ignored",
            None,
            &[],
            None,
        ) {
            Ok(out) => out,
            Err(e) => {
                // Not fatal, for the reason `ask_git_which_are_ignored` gives: the
                // search still answers, it just cannot see these files.
                tracing::debug!("could not list the ignored files: {e:#}");
                return Vec::new();
            }
        };
        let mut found: Vec<std::path::PathBuf> = String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|l| !l.is_empty())
            // A directory is the other question, and git marks one with a slash.
            .filter(|l| !l.ends_with('/'))
            .filter(|l| match exclude {
                Some(prefix) => !l.starts_with(prefix),
                None => true,
            })
            .map(|l| root.join(l))
            .collect();
        /* **A cap, because 15 is this monorepo's number and not a contract.** The
        repo in front of you is the only live test there is, and a tree that
        ignores generated sources file by file could name thousands — which would
        push real files out of `MAX_PATHS` the same way an unfiltered walk does.
        Well above both measurements, so it is a refusal nobody meets by accident. */
        if found.len() > LOOSE_IGNORED_CAP {
            tracing::debug!(
                "{} loose ignored files; searching the first {LOOSE_IGNORED_CAP}",
                found.len(),
            );
            found.sort();
            found.truncate(LOOSE_IGNORED_CAP);
        }
        found
    })
}

/// How many loose ignored files may be searched. See [`loose_ignored_files`].
const LOOSE_IGNORED_CAP: usize = 500;

/// How long the answer above is trusted. Long enough that a burst of searches
/// pays for it once, short enough that a `.gitignore` edit is picked up while you
/// are still wondering why.
const IGNORED_TTL: std::time::Duration = std::time::Duration::from_secs(30);

fn discover_small_ignored_dirs(root: &Path, exclude: Option<&str>) -> Vec<std::path::PathBuf> {
    let mut candidates = dirs_to_depth(root, exclude, IGNORED_SCAN_DEPTH);
    if candidates.is_empty() {
        return Vec::new();
    }
    if candidates.len() > IGNORED_CANDIDATES {
        tracing::debug!(
            "{} directories in the first {IGNORED_SCAN_DEPTH} levels; asking git about the first {IGNORED_CANDIDATES}",
            candidates.len(),
        );
        candidates.truncate(IGNORED_CANDIDATES);
    }
    let mut ignored = ask_git_which_are_ignored(root, &candidates);
    ignored.sort();
    /* **The cap is applied before the outermost reduction, and the order was
    wrong first.** Reducing first throws away a small ignored directory that
    sits inside a big one — `build/` over the cap holding `build/notes/`, which
    is this feature's own case one level deeper, and it failed in silence. So a
    directory that fits is kept whatever its parent does, and one that does not
    is skipped *without* taking its children with it. What the reduction is
    still for is the pair where both fit: `.plan` and `.plan/old` are one root
    rather than two walks of the same files. */
    /* **Two passes, because the second decision needs the whole of the first.**
    Sorted, so a parent is seen before its children: each ignored directory
    either fits the cap or does not, and one already covered by a kept ancestor
    is not a separate walk. */
    let mut fits: Vec<std::path::PathBuf> = Vec::new();
    let mut refused: Vec<std::path::PathBuf> = Vec::new();
    for at in ignored {
        if fits.iter().any(|kept| at.starts_with(kept)) {
            continue;
        }
        if subtree_fits(&at, IGNORED_CAP) {
            fits.push(at);
        } else {
            refused.push(at);
        }
    }
    /* **A refused directory with many fitting children is a package tree, and
    none of them are notes.** Measured on the monorepo this is developed
    against: taking every child that fits filled `MAX_PATHS`, because
    `node_modules` is refused whole and then a thousand of its packages each
    fit on their own — the same failure a generous cap caused, by another
    route. The `build/notes` shape this exists for is one or two directories.
    Counted over the candidates rather than over what has been accepted so
    far, or the first `IGNORED_SIBLINGS` of a thousand get in. */
    fits.iter()
        .filter(|at| {
            refused
                .iter()
                .find(|big| at.starts_with(big))
                .is_none_or(|big| {
                    fits.iter().filter(|x| x.starts_with(big)).count() <= IGNORED_SIBLINGS
                })
        })
        .cloned()
        .collect()
}

/// Every directory under `root` down to `depth`, as repo-relative paths.
///
/// Its own scan rather than the walk below, because this has to see what the walk
/// is about to refuse: the `ignore` crate applies its matchers before
/// `filter_entry`, so a directory dropped by a `.gitignore` never reaches a
/// callback at all. Symlinked directories are left alone — following one is how a
/// scan leaves the repo.
fn dirs_to_depth(root: &Path, exclude: Option<&str>, depth: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut level = vec![root.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for at in &level {
            let Ok(entries) = std::fs::read_dir(at) else {
                continue;
            };
            for e in entries.flatten() {
                if !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                if e.file_name() == ".git" {
                    continue;
                }
                let rel_path = rel(root, &e.path());
                if exclude.is_some_and(|prefix| format!("{rel_path}/").starts_with(prefix)) {
                    continue;
                }
                out.push(rel_path);
                next.push(e.path());
            }
        }
        level = next;
    }
    out
}

/// Which of `candidates` git ignores, in one call.
///
/// `check-ignore --stdin -z` answers for a whole list at once, which is the
/// difference between one process and one per directory. Exit 1 means "none of
/// them", not a failure; a repo git cannot answer for (no git, not a work tree)
/// gives an empty list and the search behaves exactly as it did before.
///
/// **Through `run_bounded_with_input`, and the first version deadlocked without
/// it.** Writing the list to a child's stdin and only then reading its stdout
/// hangs as soon as both pipes are full: measured against a repo whose
/// `.gitignore` is `*`, so every candidate comes back — 3,000 paths completed and
/// 3,500 never returned. That runner writes stdin on a thread, drains both output
/// pipes while it does, and carries a deadline, which is why its own doc names
/// this exact failure.
fn ask_git_which_are_ignored(root: &Path, candidates: &[String]) -> Vec<std::path::PathBuf> {
    let argv: Vec<String> = ["git", "check-ignore", "--stdin", "-z"]
        .iter()
        .map(|a| (*a).to_string())
        .collect();
    let input = candidates.join("\0").into_bytes();
    let out = match orchd_base::proc::run_bounded_with_input(
        root,
        IGNORED_GIT_TIMEOUT,
        &argv,
        "asking git which paths are ignored",
        Some(input),
        &[],
        None,
    ) {
        Ok(out) => out,
        Err(e) => {
            // Not an error worth a refusal: the search works without this, it
            // just cannot see into an ignored directory.
            tracing::debug!("could not ask git about ignored paths: {e:#}");
            return Vec::new();
        }
    };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|l| !l.is_empty())
        .map(|l| root.join(l))
        .collect()
}

/// Whether git ignores this one workspace-relative path.
///
/// `false` when git cannot answer, the same degradation the search makes: a tree
/// that is not a git work tree has nothing ignored in it.
pub fn is_ignored(root: &Path, rel: &str) -> bool {
    !ask_git_which_are_ignored(root, &[rel.to_string()]).is_empty()
}

/// How many of one refused directory's children may be searched on their own.
///
/// **Measured, and the number is doing real work.** Without a limit the monorepo
/// this is developed against filled `MAX_PATHS`: `node_modules` is refused whole,
/// and then a thousand of its packages each came in under `IGNORED_CAP`. A
/// notes folder under an ignored build root — the case this allows — is one or
/// two directories, never a thousand.
const IGNORED_SIBLINGS: usize = 4;

/// Whether a subtree holds no more than `cap` files.
///
/// Bounded by the cap rather than by the directory: a probe of `node_modules`
/// stops after `cap` entries, so the cost of refusing a huge directory is the
/// same as the cost of accepting a small one.
fn subtree_fits(at: &Path, cap: usize) -> bool {
    let mut n = 0;
    for e in WalkBuilder::new(at)
        .hidden(false)
        .git_ignore(false)
        .ignore(false)
        .parents(false)
        .build()
        .flatten()
    {
        if e.file_type().is_some_and(|t| t.is_file()) {
            n += 1;
            if n > cap {
                return false;
            }
        }
    }
    true
}

/// A walk of its own for each small ignored directory, with the ignore machinery
/// off.
///
/// **Adding them as roots of the main walk is not enough, and that was the first
/// version.** A root the `ignore` crate is given is not itself filtered, but its
/// *children* are still matched against the ignore files above it — so
/// `build/notes` handed to `WalkBuilder::add` walked and then dropped
/// `build/notes/plan.md`, because `build/` in the repo's `.gitignore` matches it.
/// It worked for a directory named directly (`.plan`) and silently did not for one
/// a level down, which is the shape a review caught.
///
/// So each one gets a builder with `git_ignore`, `ignore` and `parents` all off:
/// the decision "is this directory worth searching" has already been made, and
/// asking the ignore files again can only unmake it. The glob still applies,
/// because a path filter is the caller's question rather than the repo's.
fn ignored_walkers(root: &Path, exclude: Option<&str>, glob: Option<&str>) -> Vec<WalkBuilder> {
    let mut out = Vec::new();
    /* **A file has to be matched against the glob here, where a directory does
    not.** The doc above says a root the crate is given is not itself filtered —
    for a directory that is harmless, because the files it yields are its
    children and those are matched. A file root *is* the whole yield, so
    handing it over unasked would return `.env` for a search filtered to
    `src/`. */
    let filter = glob.map(as_glob).filter(|g| !g.is_empty()).and_then(|g| {
        let mut ov = OverrideBuilder::new(root);
        ov.add(&g).ok()?;
        ov.build().ok()
    });
    for at in loose_ignored_files(root, exclude) {
        if filter
            .as_ref()
            .is_some_and(|ov| ov.matched(&at, false).is_ignore())
        {
            continue;
        }
        let mut w = WalkBuilder::new(&at);
        w.hidden(false)
            .threads(0)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false);
        out.push(w);
    }
    for at in small_ignored_dirs(root, exclude) {
        let mut w = WalkBuilder::new(&at);
        w.hidden(false)
            .threads(0)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false)
            .filter_entry(|e| e.file_name() != ".git");
        if let Some(g) = glob.map(as_glob).filter(|g| !g.is_empty()) {
            // Relative to the *repo* root, because that is what a person typing a
            // path filter means and what the main walk matches against.
            let mut ov = OverrideBuilder::new(root);
            if ov.add(&g).is_ok() {
                if let Ok(built) = ov.build() {
                    w.overrides(built);
                }
            }
        }
        out.push(w);
    }
    out
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
    let mut walks = vec![walker(root, exclude, q.glob.as_deref())?];
    walks.extend(ignored_walkers(root, exclude, q.glob.as_deref()));
    for w in walks {
        w.build_parallel().run(|| {
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
        // One `Quit` is the whole answer: the cap is global, so an extra walk after
        // it would only read files nobody will see.
        if truncated.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
    }

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
    let mut walks = vec![walker(root, exclude, None)?];
    walks.extend(ignored_walkers(root, exclude, None));
    'walks: for w in walks {
        for entry in w.build().flatten() {
            if entry.file_type().is_some_and(|t| t.is_file()) {
                out.push(rel(root, entry.path()));
                if out.len() > MAX_PATHS {
                    break 'walks;
                }
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
        /* **A small ignored directory *is* searched now**, and this fixture's
        `target/` is one file — the rule is the cap, not the ignore verdict
        alone, and `a_small_ignored_directory_is_searched_and_a_big_one_is_not`
        is where both halves of it are asserted. */
        assert!(
            got.contains(&"target/built.rs"),
            "a small ignored directory is searched: {got:?}"
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

    /// A small ignored directory is searched; a big one is not.
    ///
    /// **This is the one exception to "the walk honours `.gitignore`", and it is
    /// measured rather than configured.** The default is right — without it the
    /// walk is 3.2M files against 19,043 — and it hides exactly the directory an
    /// agent writes its notes into. Size is what separates the two, but only
    /// after the ignore verdict: a cap on its own abandons `src` and `tests` too.
    /// Asserted from both sides, because a rule only the content search honoured
    /// is one the name search would quietly disagree with.
    #[test]
    fn a_small_ignored_directory_is_searched_and_a_big_one_is_not() {
        let dir = orchd_base::testutil::scratch_repo("search-ignored");
        fs::write(dir.join(".gitignore"), ".plan\nheap/\n").unwrap();
        fs::create_dir_all(dir.join(".plan")).unwrap();
        fs::create_dir_all(dir.join("heap")).unwrap();
        fs::write(dir.join(".plan/notes.md"), "the needle is here\n").unwrap();
        // Over the cap, so it stays out however loudly it matches.
        for i in 0..(IGNORED_CAP + 5) {
            fs::write(
                dir.join(format!("heap/f{i:05}.txt")),
                "the needle is here too\n",
            )
            .unwrap();
        }

        let found = find(&dir, "needle");
        let got: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            got,
            vec![".plan/notes.md"],
            "the small one is searched and the big one is not",
        );

        let listed = paths(&dir, None).unwrap().paths;
        assert!(listed.contains(&".plan/notes.md".to_string()), "{listed:?}");
        assert!(
            !listed.iter().any(|p| p.starts_with("heap/")),
            "the name search agrees with the content search",
        );
    }

    /// A small ignored directory inside a big one is still reachable.
    ///
    /// **The reduction to outermost roots used to run before the cap**, which
    /// threw away exactly the case the feature exists for, one level deeper: a
    /// notes folder under an ignored build root. Found by review, reproduced
    /// here, and it failed in silence — no truncation flag, the file simply gone.
    /// The report this arrived as: Shift-Shift could not find `.env`.
    ///
    /// **The file is the case, not the directory.** `small_ignored_dirs` lets an
    /// ignored *directory* in when it is small enough; an ignored *file* had no
    /// route at all, so the one file a developer edits most was the one the
    /// finder could not name. The heap is here to hold the other half: a
    /// directory git ignores stays out however many loose files are let in.
    #[test]
    fn an_ignored_file_is_found_and_an_ignored_directory_is_still_not() {
        let dir = orchd_base::testutil::scratch_repo("search-ignored-file");
        fs::write(dir.join(".gitignore"), ".env\nheap/\n").unwrap();
        fs::write(dir.join(".env"), "TOKEN=the needle is here\n").unwrap();
        fs::create_dir_all(dir.join("heap")).unwrap();
        for i in 0..(IGNORED_CAP + 5) {
            fs::write(dir.join(format!("heap/f{i:05}.txt")), "the needle too\n").unwrap();
        }

        let listed = paths(&dir, None).unwrap().paths;
        assert!(listed.contains(&".env".to_string()), "{listed:?}");
        assert!(
            !listed.iter().any(|p| p.starts_with("heap/")),
            "an ignored directory over the cap is still refused",
        );

        // The content search answers about the same files, which is the whole
        // reason this is one walk rather than a rule per mode.
        let found = find(&dir, "needle");
        let got: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(got, vec![".env"], "the two modes agree");
    }

    /// A path filter is the caller's question, and it has to reach these too.
    ///
    /// **The one thing a file root does not get for free.** The `ignore` crate
    /// never filters a root it is handed, and a file root *is* the whole yield —
    /// so without the check in `ignored_walkers` a search narrowed to `src/`
    /// answered with `.env` anyway.
    #[test]
    fn a_path_filter_still_refuses_an_ignored_file() {
        let dir = orchd_base::testutil::scratch_repo("search-ignored-file-glob");
        fs::write(dir.join(".gitignore"), ".env\n").unwrap();
        fs::write(dir.join(".env"), "TOKEN=the needle is here\n").unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), "the needle is here\n").unwrap();

        let q = Query {
            pattern: "needle".into(),
            // With the slash, which is what makes it a directory — see `as_glob`.
            glob: Some("src/".into()),
            ..Query::default()
        };
        let got: Vec<String> = search(&dir, None, &q)
            .unwrap()
            .hits
            .iter()
            .map(|h| h.path.clone())
            .collect();
        assert_eq!(got, vec!["src/a.rs".to_string()], "the filter holds");
    }

    #[test]
    fn a_small_ignored_directory_inside_a_big_one_is_still_searched() {
        let dir = orchd_base::testutil::scratch_repo("search-nested-ignored");
        fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        fs::create_dir_all(dir.join("build/notes")).unwrap();
        for i in 0..(IGNORED_CAP + 1) {
            fs::write(dir.join(format!("build/o{i:05}.txt")), "output\n").unwrap();
        }
        fs::write(dir.join("build/notes/plan.md"), "the needle is here\n").unwrap();

        let found = find(&dir, "needle");
        let got: Vec<&str> = found.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            got,
            vec!["build/notes/plan.md"],
            "the small child is searched and the big parent is not",
        );
        let listed = paths(&dir, None).unwrap().paths;
        assert!(
            listed.contains(&"build/notes/plan.md".to_string()),
            "{listed:?}"
        );
        assert!(
            !listed.iter().any(|p| p.starts_with("build/o")),
            "and the parent's own files stay out: {listed:?}",
        );
    }

    /// A refused directory with many small children contributes none of them.
    ///
    /// **This is the other half of the fix above, and it was measured rather than
    /// reasoned.** Letting every child of an over-cap directory in filled
    /// `MAX_PATHS` on a real monorepo: `node_modules` is refused whole, then a
    /// thousand of its packages each fit on their own. So the shape is the test —
    /// one notes folder under a build root comes in, a package tree does not.
    #[test]
    fn a_package_tree_contributes_nothing_however_small_its_packages_are() {
        let dir = orchd_base::testutil::scratch_repo("search-packages");
        fs::write(dir.join(".gitignore"), "deps/\n").unwrap();
        for i in 0..(IGNORED_SIBLINGS + 3) {
            fs::create_dir_all(dir.join(format!("deps/p{i}"))).unwrap();
            fs::write(dir.join(format!("deps/p{i}/index.js")), "the needle\n").unwrap();
        }
        // The parent itself is over the cap, so only its children could qualify.
        for i in 0..(IGNORED_CAP + 1) {
            fs::write(dir.join(format!("deps/o{i:05}.txt")), "output\n").unwrap();
        }
        let found = find(&dir, "needle");
        assert!(
            found.hits.is_empty(),
            "a dependency tree stays out: {:?}",
            found.hits.iter().map(|h| &h.path).collect::<Vec<_>>(),
        );
    }

    /// The list handed to git is bounded, and a long one does not hang.
    ///
    /// **The first version deadlocked**: it wrote every candidate to git's stdin
    /// and only drained stdout afterwards, so both pipes filling meant neither
    /// side moved again. Measured at the time: 3,000 paths through, 3,500 wedged
    /// forever. This drives the shape that broke it — a `.gitignore` of `*`, so
    /// every path git is asked about comes straight back down the other pipe.
    #[test]
    fn a_long_list_of_ignored_directories_answers_rather_than_hanging() {
        let dir = orchd_base::testutil::scratch_repo("search-ignored-many");
        fs::write(dir.join(".gitignore"), "*\n").unwrap();
        // Two levels, so the scan offers both the parents and the children.
        for i in 0..70 {
            for j in 0..30 {
                fs::create_dir_all(dir.join(format!("d{i:03}/n{j:03}"))).unwrap();
            }
        }
        fs::write(dir.join("d000/n000/x.txt"), "needle\n").unwrap();
        let began = std::time::Instant::now();
        let found = find(&dir, "needle");
        assert!(
            began.elapsed() < std::time::Duration::from_secs(20),
            "the walk answered in {:?}",
            began.elapsed(),
        );
        // What it answers with is not the point; that it answers is.
        assert!(found.hits.len() <= 1, "{found:?}");
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
        // Small and ignored is reachable; `.git` never is. The cap that keeps a
        // big one out has its own test.
        assert!(got.contains(&"target/built.rs".to_string()), "{got:?}");
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
