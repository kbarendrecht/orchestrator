use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;
use crate::model::*;
use crate::pty::pid_alive;

/// The durable half of a session.
///
/// Ring buffers are in-memory and are not persisted — session *records* are
/// (§2). On restart every previously live session becomes `Archived` and the
/// rail offers resume; their worktrees still exist, so those resume without a
/// rebuild step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub workspace: WorkspaceId,
    pub cwd: PathBuf,
    pub kind: Kind,
    pub title: Option<String>,
    /// The name you gave this session. Persisted, or a rename would last exactly
    /// as long as the daemon.
    #[serde(default)]
    pub name: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub archived_transcript: Option<PathBuf>,
    #[serde(default)]
    pub transcript_archived: bool,
    pub recovery: Option<ArchiveState>,
    pub created_at: SystemTime,
    /// Recorded next to the session so a crashed daemon's orphans can be found
    /// in the process table on the next start (§8b).
    pub pid: Option<u32>,
    /// Whether this was live when the record was last written. The daemon owns
    /// the pty, so a crash kills the process — this is the only way to know
    /// afterwards which sessions were actually going.
    #[serde(default)]
    pub was_live: bool,
    /// Survives a restart, or every fork in the archive loses the one thing that
    /// explains why two rows share a title.
    #[serde(default)]
    pub forked_from: Option<SessionId>,
    /// Whether the last turn was cut off rather than finished.
    ///
    /// The one thing a resumed session cannot work out for itself: it comes back
    /// at an empty prompt either way. Recorded so the restart can tell a
    /// conversation that was interrupted from one that was done.
    #[serde(default)]
    pub interrupted: bool,
    /// Whether a turn ever started — see [`Session::had_a_turn`]. Persisted so a
    /// restart keeps the difference between an empty pane and a real conversation
    /// without re-reading every transcript. A record written before this field
    /// existed defaults to `false` and is repaired once in [`prune_ghosts`], where
    /// the file is already being read.
    #[serde(default)]
    pub had_a_turn: bool,
    /// The branch this conversation is about. See [`crate::model::Session::branch`].
    ///
    /// Persisted because the pairing it exists to protect is exactly what a restart
    /// used to lose: auto-resume brings a session back in its recorded *directory*,
    /// and without the branch beside it nothing can tell that the directory now
    /// holds somebody else's work.
    #[serde(default)]
    pub branch: Option<String>,
    /// An undelivered [`crate::model::Session::arrival_notice`]. A conversation
    /// carried by a swap while it was not running is told when auto-resume brings
    /// it back, so the note has to outlive the daemon that wrote it.
    #[serde(default)]
    pub arrival_notice: Option<String>,
    /// Who spawned this session, and whether that spawn cut its worktree — see
    /// [`crate::model::Session::spawned_by`].
    ///
    /// Persisted so `orch kill` still works after a restart: the spawner comes back
    /// under the same id, its worktree is still on disk, and a spawn you want to be
    /// rid of is exactly the kind that outlives the daemon that made it.
    #[serde(default)]
    pub spawned_by: Option<SessionId>,
    #[serde(default)]
    pub spawn_cut_worktree: bool,
}

impl SessionRecord {
    pub fn of(s: &Session) -> Self {
        SessionRecord {
            id: s.id,
            workspace: s.workspace.clone(),
            cwd: s.cwd.clone(),
            kind: s.kind.clone(),
            title: s.title.clone(),
            name: s.name.clone(),
            transcript_path: s.transcript_path.clone(),
            archived_transcript: s.archived_transcript.clone(),
            transcript_archived: s.transcript_archived,
            recovery: s.recovery.clone(),
            created_at: s.created_at,
            pid: s.pid,
            was_live: s.state.is_live(),
            forked_from: s.forked_from,
            interrupted: s.interrupted,
            had_a_turn: s.had_a_turn,
            branch: s.branch.clone(),
            arrival_notice: s.arrival_notice.clone(),
            spawned_by: s.spawned_by,
            spawn_cut_worktree: s.spawn_cut_worktree,
        }
    }

    /// Rebuild a session from its record. It comes back `Archived`, never live:
    /// the daemon owned the pty and restarting it killed the process.
    pub fn restore(self) -> Session {
        let mut s = Session::new(self.id, self.workspace, self.cwd, self.kind);
        s.title = self.title;
        s.name = self.name;
        s.transcript_path = self.transcript_path;
        s.archived_transcript = self.archived_transcript;
        s.transcript_archived = self.transcript_archived;
        s.recovery = self.recovery;
        s.created_at = self.created_at;
        s.pid = self.pid;
        s.forked_from = self.forked_from;
        s.interrupted = self.interrupted;
        s.had_a_turn = self.had_a_turn;
        s.branch = self.branch;
        s.arrival_notice = self.arrival_notice;
        s.spawned_by = self.spawned_by;
        s.spawn_cut_worktree = self.spawn_cut_worktree;
        // After the fields, not before: `resumable` reads `recovery` and
        // `had_a_turn`, so computing it on a half-built session answers about the
        // defaults.
        s.state = State::Archived {
            resumable: s.resumable(),
        };
        s
    }
}

/// The window's last size, remembered so the app opens the way you left it.
///
/// Its own file rather than a field in `config.json`: that file is yours to edit,
/// and a number the app rewrites on every close does not belong in it.
pub fn save_window(width: u32, height: u32) {
    let Ok(dir) = Config::config_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join("window.json"),
        format!("{{\"width\":{width},\"height\":{height}}}\n"),
    );
}

/// What [`save_window`] wrote, if it is still sane. A size smaller than the
/// layout's own minimum is ignored rather than honoured.
pub fn load_window() -> Option<(u32, u32)> {
    let raw = std::fs::read_to_string(Config::config_dir().ok()?.join("window.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let w = v.get("width")?.as_u64()? as u32;
    let h = v.get("height")?.as_u64()? as u32;
    (w >= 1000 && h >= 600).then_some((w, h))
}

fn path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("sessions.json"))
}

fn automation_path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("automation.json"))
}

/// Write a store under the config dir. Write-and-rename so a crash mid-write
/// cannot leave a truncated file that would lose every record at once — one
/// discipline for every store, so a change to it lands everywhere.
fn save_json<T: Serialize + ?Sized>(p: &Path, value: &T) -> Result<()> {
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

/// Read a store, or its default when the file is missing or corrupt. A corrupt
/// store must not stop the daemon booting; what the degradation costs is each
/// caller's to say.
fn load_json<T: serde::de::DeserializeOwned + Default>(p: Result<PathBuf>) -> T {
    let Ok(p) = p else {
        return T::default();
    };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return T::default();
    };
    match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("could not parse {}: {e}", p.display());
            T::default()
        }
    }
}

/// §8 says SQLite; a JSON file with the same write-and-rename discipline holds
/// a handful of PR numbers just as safely and keeps the dependency list short.
pub fn save_automation(store: &crate::fix_pr::AutomationStore) -> Result<()> {
    save_json(&automation_path()?, store)
}

/// A restart must not resurrect a `Running` state whose session is gone (§8).
pub fn load_automation() -> crate::fix_pr::AutomationStore {
    let mut store: crate::fix_pr::AutomationStore = load_json(automation_path());
    // Orphaned Running is demoted to Exhausted: the run is not going to finish,
    // and pretending it might would block the PR forever. With no head — nobody
    // knows what the crashed run left, and the `""` this used to write matched no
    // real sha, so the next poll read it as "you moved the branch" and threw the
    // record away.
    for state in store.by_pr.values_mut() {
        match state {
            crate::fix_pr::PrAutomation::Running { .. } => {
                *state = crate::fix_pr::PrAutomation::Exhausted {
                    at_head: None,
                    at: std::time::SystemTime::now(),
                };
            }
            // The old sentinel, from a file written before `at_head` could say
            // "unknown". Read as what it meant rather than as a sha.
            crate::fix_pr::PrAutomation::Exhausted { at_head, .. } => {
                if at_head.as_deref() == Some("") {
                    *at_head = None;
                }
            }
        }
    }
    store
}

fn stories_path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("stories.json"))
}

/// Stories filed for review threads, so a retry does not file a second one.
///
/// A **cache**, not a ledger — see [`crate::story`]. The filer searches the
/// tracker for the thread's permalink before creating anything, so this only
/// saves an agent run. Which is why, unlike the two stores above, nothing here
/// tries to repair or reconcile it on load: the worst an empty file costs is one
/// redundant search.
pub fn save_stories(cache: &crate::story::Cache) -> Result<()> {
    save_json(&stories_path()?, cache)
}

pub fn load_stories() -> crate::story::Cache {
    load_json(stories_path())
}

fn manual_path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("manual.json"))
}

/// Batches that stopped for the manual phase, per PR.
///
/// Persisted because the alternative is a stranded branch: the accepted patches are
/// already committed by the time the phase opens, and losing the resume pointer to a
/// restart leaves work that can only be finished by hand in git. `fold_in` rewrites
/// shas, so nothing can re-derive which commit was ours.
pub fn save_manual(
    phases: &std::collections::HashMap<u64, crate::post::ManualPhase>,
) -> Result<()> {
    save_json(&manual_path()?, phases)
}

/// Degrading to empty costs the resume, which is bad but recoverable by hand;
/// refusing to boot would cost every session.
pub fn load_manual() -> std::collections::HashMap<u64, crate::post::ManualPhase> {
    load_json(manual_path())
}

fn resolve_runs_path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("resolve-runs.json"))
}

/// Resolve runs, per PR.
///
/// Persisted for the same reason as `manual.json` and not the same reason as
/// `sessions.json`: the run's commits are already in git by the time anything can
/// go wrong, and this record is the only thing that says which commit answers
/// which reviewer. Without it a restart left a branch of commits and no map.
pub fn save_resolve_runs(runs: &std::collections::HashMap<u64, crate::state::ResolveRun>) -> Result<()> {
    save_json(&resolve_runs_path()?, runs)
}

/// Every run the last daemon knew about, marked as over.
///
/// The session cannot have survived the restart — the daemon owns every pty and
/// takes them with it — so a restored run is an account, never something still
/// moving. Said here rather than left for a reader to infer, because a thread
/// reading `pending` in an overview otherwise looks imminent forever.
pub fn load_resolve_runs() -> std::collections::HashMap<u64, crate::state::ResolveRun> {
    let mut runs: std::collections::HashMap<u64, crate::state::ResolveRun> =
        load_json(resolve_runs_path());
    for r in runs.values_mut() {
        r.ended
            .get_or_insert_with(|| "the daemon restarted; the session did not survive it".into());
    }
    runs
}

pub fn save(records: &[SessionRecord]) -> Result<()> {
    save_json(&path()?, records)
}

/// A corrupt store only costs the resume offers.
pub fn load() -> Vec<SessionRecord> {
    load_json(path())
}

/// Drop records with nothing left to come back to.
///
/// A session whose transcript is not on disk has no conversation, and ring
/// buffers are not persisted, so its record carries no scrollback either — there
/// is literally nothing behind it. The rail knows this and draws no row for one
/// (`isArchived && !has_transcript` lands in neither group), so these accumulate
/// as records that exist, count, and cannot be seen or reached. Three had built up
/// on the author's machine and were only noticed because `Ctrl+Tab` used to step
/// onto them and show an empty terminal.
///
/// Only ever the ones with no transcript **and** no archived copy: those are the
/// two places a conversation can be, and a session that still has either is a
/// resume offer, not a ghost. The worktree is not the record's to lose —
/// `adopt_existing_worktrees` finds those on disk independently — so dropping a
/// ghost cannot orphan work.
///
/// Returns the records worth keeping, and how many went.
pub fn prune_ghosts(mut records: Vec<SessionRecord>) -> (Vec<SessionRecord>, usize) {
    let before = records.len();
    /* Pin first, exactly as `restore_sessions` does, and for the reason its own
       comment gives: a worktree session files its transcript under the checkout it
       started in, not the worktree, so `transcript_path` is usually `None` on disk
       and the cwd-derived slug finds nothing either. Only `find_transcript`'s hunt
       does.

       This is not a hypothetical. The first version of this checked the narrow
       path *before* pinning, decided all five records on a real machine had no
       transcript, and deleted every one of them — while the unit test passed,
       because it set `transcript_path` explicitly and so never exercised the case
       that actually occurs. Pinning here also repairs the record, so the path is
       written back correct rather than re-hunted on every start. */
    for r in &mut records {
        pin_transcript(r.id, &r.cwd, &mut r.transcript_path);
        // Repair a record written before `had_a_turn` existed: it defaults to
        // `false`, but the file on disk knows. Done here because this is the one
        // place already paying for a read per record, and writing it back means the
        // question is answered from the field forever after.
        if !r.had_a_turn && has_conversation(r.id, &r.cwd, r.transcript_path.as_deref()) {
            r.had_a_turn = true;
        }
    }
    let kept: Vec<SessionRecord> = records
        .into_iter()
        .filter(|r| {
            // A record is worth keeping only if there is a conversation to come back
            // to: `had_a_turn` (repaired from disk just above), or an archived copy
            // on disk. Not "a file exists" — a session opened and never typed into
            // owns a headers-only `.jsonl` that resumes into an instant exit.
            //
            // `was_live` is deliberately *not* a keeper on its own. It once was, to
            // protect records `auto_resume` relaunches — but `auto_resume` now
            // refuses a `!had_a_turn` record (it would exit instantly), so a
            // `was_live && !had_a_turn` record is kept by nothing that can bring it
            // back: it restores as an `Archived` row with `has_transcript=false`,
            // which `isConversation` hides — present and counted, but unseeable and
            // unreachable, the exact ghost this prune exists to remove.
            r.had_a_turn || r.archived_transcript.as_deref().is_some_and(Path::exists)
        })
        .collect();
    let dropped = before - kept.len();
    (kept, dropped)
}

/// Kill processes left behind by a crashed daemon.
///
/// Anything still alive from a previous run is an orphan: the daemon spawns
/// every session and nothing else adopts them (§8b).
pub fn reap_orphans(records: &[SessionRecord]) -> usize {
    let mut killed = 0;
    for r in records {
        let Some(pid) = r.pid else { continue };
        if !pid_alive(pid) {
            continue;
        }
        // SIGTERM only. These are Claude sessions mid-turn, and a signal they
        // can handle beats one they cannot.
        let ok = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            tracing::warn!(pid, session = %r.id, "killed an orphan from a crashed daemon");
            killed += 1;
        }
    }
    killed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// This one deletes durable records, so the line between "nothing to return
    /// to" and "a resume offer" has to be exact in both directions.
    #[test]
    fn pruning_drops_only_the_records_with_nothing_behind_them() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-prune-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let record = |name: &str, transcript: Option<PathBuf>, archived: Option<PathBuf>| {
            SessionRecord {
                id: uuid::Uuid::new_v4(),
                workspace: name.to_string(),
                // A cwd with no transcript directory of its own, so only the paths
                // set explicitly below can make a record worth keeping.
                cwd: dir.clone(),
                kind: crate::model::Kind::Interactive,
                title: None,
                name: None,
                transcript_path: transcript,
                archived_transcript: archived,
                transcript_archived: false,
                recovery: None,
                created_at: SystemTime::now(),
                pid: None,
                was_live: false,
                forked_from: None,
                interrupted: false,
                had_a_turn: false,
                branch: None,
                arrival_notice: None,
                spawned_by: None,
                spawn_cut_worktree: false,
            }
        };

        // A real conversation: the file holds a user turn, so `prune_ghosts` repairs
        // `had_a_turn` from disk and keeps it even though the record predates the bit.
        let real = dir.join("real.jsonl");
        std::fs::write(&real, "{\"type\":\"user\",\"message\":\"hi\"}\n").unwrap();
        // A headers-only file: a session opened and never typed into. This is the
        // ghost the `had_a_turn` sweep exists to drop — the old file-exists check
        // kept it, and it resumed into an instant exit.
        let headers = dir.join("headers.jsonl");
        std::fs::write(&headers, "{\"type\":\"summary\"}\n").unwrap();
        let archived_copy = dir.join("archived.jsonl");
        std::fs::write(&archived_copy, "{}\n").unwrap();

        let records = vec![
            record("real-conversation", Some(real.clone()), None),
            // The bit already set, no file needed: a record written since the field
            // existed carries its own answer.
            SessionRecord { had_a_turn: true, ..record("had-a-turn", None, None) },
            // A live session that had a turn survives — `was_live` alone is not why,
            // its conversation is.
            SessionRecord { was_live: true, had_a_turn: true, ..record("was-live-real", None, None) },
            // The archive is the *other* place a conversation can be: teardown
            // copies it out and the original may be pruned by Claude Code.
            record("has-archived-copy", None, Some(archived_copy.clone())),
            // A file, but no turn in it — dropped now, kept before.
            record("headers-only", Some(headers.clone()), None),
            // Recorded paths that no longer resolve are the same as none.
            record("dangling-paths", Some(dir.join("gone.jsonl")), Some(dir.join("gone2.jsonl"))),
            record("ghost", None, None),
            // Live at the crash but never a turn: `auto_resume` refuses it (a resume
            // would exit instantly), so keeping it only makes an `Archived,
            // has_transcript=false` row that `isConversation` hides — an invisible
            // ghost. Dropped, precisely because `was_live` no longer outranks the
            // turn check.
            SessionRecord { was_live: true, ..record("was-live-turnless", None, None) },
        ];

        let (kept, dropped) = prune_ghosts(records);
        let names: Vec<&str> = kept.iter().map(|r| r.workspace.as_str()).collect();
        assert_eq!(
            names,
            vec!["real-conversation", "had-a-turn", "was-live-real", "has-archived-copy"]
        );
        assert_eq!(dropped, 4, "headers-only, dangling, the pure ghost, and the turnless was_live ghost");
        // The repair is written back, so the survivor stops being re-derived from
        // the file on every start.
        assert!(kept.iter().find(|r| r.workspace == "real-conversation").unwrap().had_a_turn);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manual_phase_survives_a_round_trip() {
        // The file exists so a restart does not strand a batch whose patches are
        // already committed, which means the digest has to mean the same thing in the
        // next process too.
        let phase = crate::post::ManualPhase {
            committed: "4c1e9a27f3b8d1e5a9c2f7b4e8d3a6c1f5b9e2d7".into(),
            files: vec![crate::patch::FileStat {
                path: "renovate.json5".into(),
                added: 3,
                deleted: 1,
            }],
            amend: Some("folded into 9b21f04".into()),
            threads: vec![crate::post::ManualThread {
                thread_id: "PRRT_1".into(),
                label: "a.ts:12 · alice".into(),
                comment: "belongs in the repository".into(),
                draft: String::new(),
            }],
            decisions: "0badc0de0badc0de".into(),
        };
        let map: std::collections::HashMap<u64, crate::post::ManualPhase> = [(10001, phase)].into();
        let back: std::collections::HashMap<u64, crate::post::ManualPhase> =
            serde_json::from_str(&serde_json::to_string(&map).unwrap()).unwrap();

        let got = back.get(&10001).expect("the phase");
        assert_eq!(got.committed, map[&10001].committed);
        assert_eq!(got.decisions, "0badc0de0badc0de", "the digest must survive");
        assert_eq!(got.files[0].path, "renovate.json5");
        assert_eq!(got.threads[0].thread_id, "PRRT_1");
    }

    #[test]
    fn the_last_ai_title_in_the_transcript_wins() {
        // Claude Code re-writes the entry on every append, so the file holds the
        // whole history of what it has called this conversation. The newest one
        // is the one that describes what is in the pane now.
        let dir = std::env::temp_dir().join(format!("orchd-title-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.jsonl");
        std::fs::write(
            &file,
            concat!(
                r#"{"type":"user","message":"hi"}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"First guess"}"#,
                "\n",
                r#"{"type":"assistant","message":"ok"}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"Fix Ctrl+P not showing all filled pages"}"#,
                "\n",
            ),
        )
        .unwrap();

        let got = ai_title(uuid::Uuid::new_v4(), Path::new("/nonexistent"), Some(&file));
        assert_eq!(got.as_deref(), Some("Fix Ctrl+P not showing all filled pages"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The line the swap's carry closes a live session on, so it has to hold in
    /// both directions.
    #[test]
    fn a_transcript_of_headers_is_not_yet_a_conversation() {
        let dir = std::env::temp_dir().join(format!("orchd-conv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = uuid::Uuid::new_v4();
        let nowhere = Path::new("/nonexistent");

        // A session that spawned and was never typed into: the file exists, and
        // `--resume` on it still answers "no conversation found".
        let headers = dir.join("headers.jsonl");
        std::fs::write(
            &headers,
            concat!(
                r#"{"type":"mode","mode":"default"}"#,
                "\n",
                r#"{"type":"permission-mode","permissionMode":"default"}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"worktree-x"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert!(transcript_exists(id, nowhere, Some(&headers)), "the file is there");
        assert!(
            !has_conversation(id, nowhere, Some(&headers)),
            "but nothing has been said in it"
        );

        // One turn is enough, and stays enough however much tool output buries it.
        // This is the case a tail read gets wrong: 128KB of the end of this file
        // holds no `user` line, so tailing would refuse a real conversation.
        let buried = dir.join("buried.jsonl");
        let mut text = String::from("{\"type\":\"user\",\"message\":\"hi\"}\n");
        while text.len() < 300 * 1024 {
            text.push_str(r#"{"type":"assistant","message":"tool output"}"#);
            text.push('\n');
        }
        std::fs::write(&buried, &text).unwrap();
        assert!(has_conversation(id, nowhere, Some(&buried)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `--worktree` case: the record names a directory Claude Code never
    /// wrote to, and the file is under the checkout the session started in.
    #[test]
    fn a_transcript_is_found_by_id_when_both_recorded_paths_are_wrong() {
        let home = std::env::temp_dir().join(format!("orchd-find-{}", std::process::id()));
        let real = home.join(".claude/projects/-home-dev-repo");
        std::fs::create_dir_all(&real).unwrap();
        let id = uuid::Uuid::new_v4();
        std::fs::write(real.join(format!("{id}.jsonl")), "{}\n").unwrap();
        std::env::set_var("HOME", &home);

        // Neither cheap answer can see it: the recorded path names a worktree that
        // was never written to, and so does the cwd.
        let wrong = home.join(".claude/projects/-home-dev-repo--claude-worktrees-x");
        assert!(transcript_file(
            id,
            Path::new("/home/dev/repo/.claude/worktrees/x"),
            Some(&wrong.join(format!("{id}.jsonl")))
        )
        .is_none());

        let found = find_transcript(id).expect("found by id");
        assert!(found.ends_with(format!("-home-dev-repo/{id}.jsonl")), "{found:?}");

        // And that is what `pin_transcript` records, so the session stops
        // reporting no transcript and stays in the archive. Asserted here rather
        // than in a test of its own: `find_transcript` reads `$HOME`, which is
        // process-wide, and a second test setting it races this one.
        let cwd = Path::new("/home/dev/repo/.claude/worktrees/x");
        let mut recorded = Some(wrong.join(format!("{id}.jsonl")));
        pin_transcript(id, cwd, &mut recorded);
        assert!(transcript_exists(id, cwd, recorded.as_deref()));

        // A path that is already right is left alone, so the scan is not paid for
        // on every call.
        let pinned = recorded.clone();
        pin_transcript(id, cwd, &mut recorded);
        assert_eq!(recorded, pinned);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_transcript_without_a_title_is_not_an_error() {
        // The entry only appears after the first exchange, and the format is
        // Claude Code's own. No title means the rail keeps the workspace name.
        let dir = std::env::temp_dir().join(format!("orchd-notitle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.jsonl");
        std::fs::write(&file, "{\"type\":\"user\",\"message\":\"hi\"}\n").unwrap();

        assert!(ai_title(uuid::Uuid::new_v4(), Path::new("/nonexistent"), Some(&file)).is_none());
        assert!(ai_title(uuid::Uuid::new_v4(), Path::new("/nonexistent"), None).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relocated session's transcript is re-filed under the tree it now runs in.
    ///
    /// A move rather than a copy, deliberately: two files under one id is the state
    /// where `find_transcript` can answer with the stale one, and the swap would
    /// leave a conversation appearing to live in both trees at once.
    /// The shapes are copied from real transcripts, because the whole value of
    /// this reader is that it agrees with what Claude Code actually writes.
    #[test]
    fn the_last_worktree_state_record_is_the_pin() {
        let dir = std::env::temp_dir().join(format!("orchd-pin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pin.jsonl");
        let id = uuid::Uuid::new_v4();

        // Nothing to say: not every conversation was ever in a worktree.
        std::fs::write(&f, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(worktree_pin(&f), None);

        // Isolated, re-appended every turn — the many-records case.
        let pinned = format!(
            "{{\"type\":\"worktree-state\",\"worktreeSession\":{{\"originalCwd\":\"/repo\",\
             \"preEnterOriginalCwd\":\"/repo\",\"worktreePath\":\"/repo/.claude/worktrees/wt\",\
             \"worktreeName\":\"wt\",\"sessionId\":\"{id}\",\"hookBased\":true}},\
             \"sessionId\":\"{id}\"}}\n"
        );
        std::fs::write(&f, format!("{{\"type\":\"user\"}}\n{pinned}{pinned}")).unwrap();
        assert_eq!(
            worktree_pin(&f).as_deref(),
            Some(std::path::Path::new("/repo/.claude/worktrees/wt"))
        );

        // And let go again. The last record wins, which is the point: an earlier
        // pin followed by a cleared one means "not isolated", and reading only the
        // first would have this exactly backwards.
        clear_worktree_pin(id, std::path::Path::new("/repo"), &f).unwrap();
        assert_eq!(worktree_pin(&f), None, "a cleared pin must read as no pin");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What we append has to be what Claude Code reads, so the record is checked
    /// field by field rather than by "it round-trips through our own reader".
    #[test]
    fn clearing_writes_the_two_records_claude_code_writes() {
        let dir = std::env::temp_dir().join(format!("orchd-clear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("clear.jsonl");
        let id = uuid::Uuid::new_v4();
        std::fs::write(&f, "").unwrap();

        clear_worktree_pin(id, std::path::Path::new("/repo"), &f).unwrap();

        let text = std::fs::read_to_string(&f).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "two records, one line each");

        let a: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(a["type"], "relocated");
        assert_eq!(a["relocatedCwd"], "/repo");
        assert_eq!(a["sessionId"], id.to_string());

        let b: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(b["type"], "worktree-state");
        assert!(b["worktreeSession"].is_null(), "null is what releases it");
        assert_eq!(b["sessionId"], id.to_string());

        // Appended, never rewritten: the file is a conversation's own history and
        // the daemon has no business editing what is already in it.
        let before = text.clone();
        clear_worktree_pin(id, std::path::Path::new("/repo"), &f).unwrap();
        assert!(
            std::fs::read_to_string(&f).unwrap().starts_with(&before),
            "clearing twice must append, not rewrite"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_relocated_transcript_moves_rather_than_being_copied() {
        let root = std::env::temp_dir().join(format!("orchd-move-{}", std::process::id()));
        let (from, to) = (root.join("slug-worktree"), root.join("slug-main"));
        std::fs::create_dir_all(&from).unwrap();
        let id = uuid::Uuid::new_v4();
        let src = from.join(format!("{id}.jsonl"));
        std::fs::write(&src, "{\"type\":\"user\",\"message\":\"hi\"}\n").unwrap();

        // The destination slug does not exist yet: main's own directory is only
        // created when something is filed under it.
        let dest = to.join(format!("{id}.jsonl"));
        assert_eq!(relocate_file(&src, &dest).unwrap(), dest);
        assert!(dest.exists(), "the transcript arrived");
        assert!(!src.exists(), "and did not stay behind");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "{\"type\":\"user\",\"message\":\"hi\"}\n");

        // Idempotent, because a session resumed in the tree it was already in asks
        // for a move to where it already is.
        assert_eq!(relocate_file(&dest, &dest).unwrap(), dest);
        assert!(dest.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session that never had a turn has no file, and that is not a failure —
    /// it is the ordinary case for a pane opened and swapped away from.
    #[test]
    fn moving_a_transcript_that_does_not_exist_is_not_an_error() {
        let id = uuid::Uuid::new_v4();
        let moved = move_transcript(id, Path::new("/nonexistent/a"), Path::new("/nonexistent/b"));
        assert!(matches!(moved, Ok(None)), "{moved:?}");
    }

    /// A rename is durable or it is decoration: the field goes through
    /// `sessions.json` on every `notify`, and a `restore` that forgot it would put
    /// Claude Code's own ai-title back on the row at the next launch with nothing
    /// failing. Asserted here because the round-trip test beside it checks fields
    /// one by one, so a new one is only covered once it is named.
    #[test]
    fn a_name_you_gave_a_session_survives_the_record() {
        let mut s = crate::model::Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            std::path::Path::new("/tmp").to_path_buf(),
            crate::model::Kind::Interactive,
        );
        s.title = Some("Generated conversation name".into());
        s.name = Some("the swap bug".into());

        let back = SessionRecord::of(&s).restore();
        assert_eq!(back.name.as_deref(), Some("the swap bug"));
        // Both are kept: the ai-title is what the row goes back to when the name
        // is cleared, so overwriting it with the name would be a one-way door.
        assert_eq!(back.title.as_deref(), Some("Generated conversation name"));
        assert_eq!(back.label(), Some("the swap bug"));

        // A record written before the field existed loads unnamed, and the row
        // reads as whatever Claude Code called it.
        let old: SessionRecord = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "workspace": "wt",
            "cwd": "/tmp",
            "kind": { "kind": "interactive" },
            "title": "Generated conversation name",
            "transcript_path": null,
            "archived_transcript": null,
            "recovery": null,
            "created_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "pid": null,
        }))
        .expect("a record from before the rename still loads");
        let back = old.restore();
        assert_eq!(back.name, None);
        assert_eq!(back.label(), Some("Generated conversation name"));
    }

    /// A restart brings a session back in its recorded *directory*, so the branch
    /// has to come back beside it or nothing can tell that the directory now holds
    /// somebody else's work. The notice rides along for the same reason: a
    /// conversation moved while it was not running is told when auto-resume starts
    /// it again, which may be days later.
    #[test]
    fn the_branch_and_an_undelivered_notice_survive_the_record() {
        let mut s = crate::model::Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            std::path::Path::new("/tmp").to_path_buf(),
            crate::model::Kind::Interactive,
        );
        s.branch = Some("feature/a".into());
        s.arrival_notice = Some("you were moved".into());

        let back = SessionRecord::of(&s).restore();
        assert_eq!(back.branch.as_deref(), Some("feature/a"));
        assert_eq!(back.arrival_notice.as_deref(), Some("you were moved"));

        // A record written before either field existed loads as "nothing known",
        // and an unknown branch travels nowhere.
        let old: SessionRecord = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "workspace": "wt",
            "cwd": "/tmp",
            "kind": { "kind": "interactive" },
            "title": null,
            "transcript_path": null,
            "archived_transcript": null,
            "recovery": null,
            "created_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "pid": null,
        }))
        .unwrap();
        assert_eq!(old.branch, None);
        assert_eq!(old.arrival_notice, None);
    }

    /// Without this the flag dies with the daemon, and every session comes back
    /// looking equally unfinished — which is the bug it exists to fix.
    #[test]
    fn an_interrupted_turn_survives_the_record() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        s.set_state(State::Working);
        assert!(SessionRecord::of(&s).restore().interrupted);

        s.set_state(State::YourTurn {
            since: std::time::SystemTime::now(),
            reason: TurnReason::TurnComplete,
        });
        assert!(!SessionRecord::of(&s).restore().interrupted);
    }

    /// The bit that separates an empty pane from a conversation: set on the first
    /// `Working`, never cleared, and carried across a restart so the archive does
    /// not have to re-read every transcript to tell them apart.
    #[test]
    fn had_a_turn_latches_on_the_first_working_and_survives_the_record() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        assert!(!s.had_a_turn, "a fresh session has had none");

        s.set_state(State::Working);
        assert!(s.had_a_turn, "a turn started");
        assert!(SessionRecord::of(&s).restore().had_a_turn, "and survives the record");

        // Finishing the turn does not un-set it: the question is whether one ever
        // happened, not whether one is happening now.
        s.set_state(State::YourTurn {
            since: std::time::SystemTime::now(),
            reason: TurnReason::TurnComplete,
        });
        assert!(s.had_a_turn);
    }

    #[test]
    fn a_restored_session_is_archived_never_live() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        s.had_a_turn = true;
        let restored = SessionRecord::of(&s).restore();
        assert!(matches!(
            restored.state,
            State::Archived { resumable: true }
        ));
        assert!(!restored.state.is_live());
    }

    /// The archive used to promise a resume for every session it touched, so a pane
    /// opened and never typed into came back offering to continue a conversation
    /// that does not exist — `--resume` finds a headers-only transcript and exits.
    #[test]
    fn a_turnless_session_restores_as_unresumable() {
        let s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        assert!(!s.had_a_turn);
        let restored = SessionRecord::of(&s).restore();
        assert!(matches!(
            restored.state,
            State::Archived { resumable: false }
        ));
    }

    #[test]
    fn a_transcript_only_session_restores_as_unresumable() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        s.had_a_turn = true;
        s.recovery = Some(ArchiveState::TranscriptOnly);
        let restored = SessionRecord::of(&s).restore();
        assert!(matches!(
            restored.state,
            State::Archived { resumable: false }
        ));
    }

    #[test]
    fn a_record_survives_a_round_trip() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "invoice".into(),
            Path::new("/repo/.claude/worktrees/invoice").to_path_buf(),
            Kind::Automation {
                pr: 4812,
                command: "fix-pr".into(),
            },
        );
        s.transcript_archived = true;
        s.recovery = Some(ArchiveState::Recoverable {
            name: "invoice".into(),
            branch: "worktree-invoice".into(),
            head_sha: "abc123".into(),
        });
        let json = serde_json::to_string(&[SessionRecord::of(&s)]).unwrap();
        let back: Vec<SessionRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        let r = back.into_iter().next().unwrap();
        assert_eq!(r.id, s.id);
        assert!(r.transcript_archived);
        assert_eq!(
            r.kind,
            Kind::Automation {
                pr: 4812,
                command: "fix-pr".into()
            }
        );
        assert!(matches!(r.recovery, Some(ArchiveState::Recoverable { .. })));
    }

    /// A spawn you want to be rid of is exactly the kind that outlives the daemon
    /// that made it, so both halves of `orch kill`'s authorisation are persisted —
    /// and a record that never had them reads as "nobody spawned this", which
    /// refuses rather than allowing a restart to widen what the token reaches.
    #[test]
    fn who_spawned_a_session_and_whether_it_cut_the_tree_survive_a_restart() {
        let parent = uuid::Uuid::new_v4();
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "fixer-a".into(),
            Path::new("/repo/.claude/worktrees/fixer-a").to_path_buf(),
            Kind::Interactive,
        );
        s.spawned_by = Some(parent);
        s.spawn_cut_worktree = true;
        let back = SessionRecord::of(&s).restore();
        assert_eq!(back.spawned_by, Some(parent));
        assert!(back.spawn_cut_worktree);

        let old: SessionRecord = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "workspace": "main",
            "cwd": "/repo",
            "kind": { "kind": "interactive" },
            "title": null,
            "transcript_path": null,
            "archived_transcript": null,
            "recovery": null,
            "created_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "pid": null,
        }))
        .expect("a record written before these fields still loads");
        assert_eq!(old.spawned_by, None, "nobody spawned it, so nobody may kill it");
        assert!(!old.spawn_cut_worktree, "and no tree of ours to remove");
    }

    #[test]
    fn a_record_written_before_the_rename_still_loads() {
        // `Automation.skill` became `command` when the prompts were vendored
        // into `commands/`. Sessions already on disk say `skill`, and a daemon
        // that cannot read its own state file loses every archived session.
        let old = r#"{"kind":"automation","pr":4812,"skill":"green"}"#;
        let k: Kind = serde_json::from_str(old).unwrap();
        assert_eq!(
            k,
            Kind::Automation {
                pr: 4812,
                command: "green".into()
            }
        );
    }
}

/// Whether there is a conversation here, not merely a file.
///
/// [`transcript_exists`] is the weaker question and it is not enough before a
/// When this conversation was last worked in, as evidence rather than bookkeeping.
///
/// The transcript's own mtime. Claude Code appends a line per turn, so the file's
/// last write *is* the conversation's last activity, and the daemon only ever reads
/// it — `ai_title` tails it, `has_conversation` reads its head, neither touches the
/// mtime. Measured across 102 records here: readable for 87 of them, and it puts a
/// session that ran for 66 hours 66 hours later than its start.
///
/// Retention counts from this rather than from `created_at`, which is the flaw in
/// dating a conversation by its beginning: a session you worked in for weeks would
/// read as ancient the day after you stopped, and its tree would go while you still
/// remembered it.
///
/// The ladder ends at `created_at` on purpose. Claude Code prunes transcripts, so a
/// missing file is a normal state, and the archived copy is then the best remaining
/// evidence — its mtime is when the daemon copied it, which is close to when the
/// session ended. Falling back to the start is the conservative end: it can only
/// make a tree look *older* than it is, and the six-check preflight is what stands
/// between that and losing anything.
pub fn last_used(s: &crate::model::Session) -> std::time::SystemTime {
    let mtime = |p: Option<&Path>| {
        p.and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
    };
    mtime(s.transcript_path.as_deref())
        .or_else(|| mtime(s.archived_transcript.as_deref()))
        .unwrap_or(s.created_at)
}

/// `--resume`: a session that has started but never had a turn *does* have a
/// `.jsonl`, holding only its `mode` / `permission-mode` / `ai-title` headers —
/// 266 bytes of it. Resuming that answers "no conversation found" and the process
/// exits instantly, which cost a live session when the swap's carry trusted the
/// file's existence and then closed the original.
///
/// A `user` entry is the cheapest proof that somebody said something, and the
/// first one sits just past those headers and never moves — so this reads the
/// *head*, unlike [`ai_title`], whose answer is rewritten on every append and so
/// is only ever current at the tail. Tailing here would also answer *wrongly*: a
/// conversation whose last 128KB is one long run of tool output holds no `user`
/// line in it at all.
pub fn has_conversation(id: uuid::Uuid, cwd: &Path, recorded: Option<&Path>) -> bool {
    let Some(path) = transcript_file(id, cwd, recorded) else {
        return false;
    };
    let Ok(head) = read_head(&path, FIRST_TURN_BYTES) else {
        return false;
    };
    // The pattern holds no newline, so searching the buffer whole is the same
    // search as scanning it by line, without the split.
    String::from_utf8_lossy(&head).contains(r#""type":"user""#)
}

/// Whether a conversation was ever written to disk.
///
/// A session killed before its first turn has no `.jsonl` at all, and there is
/// nothing to come back to: `claude --resume` answers "no conversation found"
/// and exits. Both the startup resume and the rail's archive ask this, so they
/// ask it the same way.
///
/// Says nothing about whether the file holds a *turn* — see [`has_conversation`]
/// for that, which is what anything about to `--resume` should ask.
pub fn transcript_exists(id: uuid::Uuid, cwd: &Path, recorded: Option<&Path>) -> bool {
    transcript_file(id, cwd, recorded).is_some()
}

/// The transcript on disk, if there is one.
///
/// The path the hook reported is preferred and the slug is the fallback, because
/// a session adopted into a worktree changes cwd after it starts and the recorded
/// path is the one that was actually written.
pub fn transcript_file(id: uuid::Uuid, cwd: &Path, recorded: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = recorded {
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }
    let candidate = crate::config::transcript_dir_for(cwd)
        .ok()?
        .join(format!("{id}.jsonl"));
    candidate.exists().then_some(candidate)
}

/// Hunt for a session's transcript anywhere Claude Code keeps them.
///
/// The last resort, and it exists because of `claude --worktree`: that session
/// starts in the main checkout, so Claude Code files its transcript under
/// *main's* slug, and then the daemon adopts the session into the worktree it
/// just cut. From then on both of the cheap answers are wrong — the recorded path
/// names the worktree and so does the cwd — while the file sits under a third
/// name. The id is a uuid, so finding it by name is unambiguous.
///
/// Deliberately not part of [`transcript_file`]: that is asked once per session
/// per snapshot, and a directory scan for every session that legitimately has no
/// transcript would be a scan a second, forever. Callers use this once and record
/// what it found.
pub fn find_transcript(id: uuid::Uuid) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let projects = PathBuf::from(home).join(".claude/projects");
    let name = format!("{id}.jsonl");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let candidate = entry.path().join(&name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Point a session at the transcript it actually has, when the cheap answers are
/// wrong.
///
/// Wraps [`find_transcript`] with the check that says whether the scan is needed
/// at all, so the three callers cannot disagree about when to pay for it: at
/// startup, at the first `Stop`, and when the process ends. Those are the moments
/// the answer can change, and each of them records what it found.
pub fn pin_transcript(id: uuid::Uuid, cwd: &Path, recorded: &mut Option<PathBuf>) {
    if transcript_file(id, cwd, recorded.as_deref()).is_some() {
        return;
    }
    if let Some(found) = find_transcript(id) {
        *recorded = Some(found);
    }
}

/// Re-file a session's transcript under the slug of the directory it now runs in.
///
/// For a relocated session — one killed in one tree and resumed in another under
/// the same id, which is how the swap moves a conversation. `--resume` does not
/// need this: it was measured against `claude` 2.1.240, and it resolves a
/// conversation by **id wherever the file sits**, appending to whatever location it
/// finds. Resuming from a second directory with the file left behind continues the
/// conversation and keeps writing to the *original* slug.
///
/// So this is about filing, not about survival, and that is why its failure is not
/// fatal to a move. What it buys is the invariant every slug-based lookup here
/// assumes — a session's transcript lives under its own cwd's slug. Left unmoved,
/// [`transcript_file`]'s cheap path is wrong for the rest of the session's life and
/// only the recorded path or a [`find_transcript`] scan saves it.
///
/// `Ok(None)` means there was nothing to move, which is the ordinary case for a
/// session that never had a turn.
pub fn move_transcript(id: uuid::Uuid, from: &Path, to: &Path) -> Result<Option<PathBuf>> {
    let Some(src) = transcript_file(id, from, None) else {
        return Ok(None);
    };
    let dir = crate::config::transcript_dir_for(to)?;
    relocate_file(&src, &dir.join(format!("{id}.jsonl"))).map(Some)
}

/// Where Claude Code believes this conversation is isolated, if anywhere.
///
/// The transcript carries a `worktree-state` record and **the last one wins**: a
/// session that leaves a worktree appends one whose `worktreeSession` is `null`,
/// and 128 transcripts on this machine end that way. So the pin is a running
/// value, not a header — read to the end, keep the last answer.
///
/// `None` covers both "never isolated" and "isolated and then let go", which are
/// the same thing to every caller here.
pub fn worktree_pin(transcript: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(transcript).ok()?;
    let mut pin = None;
    for line in text.lines() {
        // Cheap reject first: this runs over a file that is megabytes of turns and
        // holds a handful of these records.
        if !line.contains("\"worktree-state\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("worktree-state") {
            continue;
        }
        pin = v
            .get("worktreeSession")
            .and_then(|w| w.get("worktreePath"))
            .and_then(|p| p.as_str())
            .map(PathBuf::from);
    }
    pin
}

/// Tell a moved conversation it has been moved, in the file it reads it from.
///
/// The claim this replaces was that the daemon *cannot* clear Claude Code's
/// worktree isolation from outside, because `ExitWorktree` is the agent's own
/// tool. Measured against 128 transcripts, that is wrong: the state is two
/// appended records, and both are one line with no timestamp and no uuid —
///
/// ```text
/// {"type":"relocated","sessionId":"…","relocatedCwd":"/new/cwd"}
/// {"type":"worktree-state","worktreeSession":null,"sessionId":"…"}
/// ```
///
/// which is exactly what Claude Code writes for itself when a session relocates.
/// Asking the agent instead was one instruction, delivered once, that an agent is
/// free to ignore — and one conversation took the bare "isolated in the worktree"
/// refusal sixteen times over two days while editing a tree that had since been
/// cut again for somebody else's branch.
///
/// Undocumented format, the same bet as [`ai_title`], and it degrades the same
/// way: an ignored record leaves things exactly as they were, and the arrival
/// notice still says the words. **Only safe while the session is not running** —
/// call it before the pty, never beside a live agent appending to the same file.
pub fn clear_worktree_pin(id: uuid::Uuid, cwd: &Path, transcript: &Path) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(transcript)?;
    // `relocated` first, then the pin, in the order Claude Code writes them.
    writeln!(
        f,
        "{}",
        serde_json::json!({
            "type": "relocated",
            "sessionId": id.to_string(),
            "relocatedCwd": cwd.to_string_lossy(),
        })
    )?;
    writeln!(
        f,
        "{}",
        serde_json::json!({
            "type": "worktree-state",
            "worktreeSession": serde_json::Value::Null,
            "sessionId": id.to_string(),
        })
    )?;
    Ok(())
}

/// Delete a session's transcript file, wherever it sits.
///
/// For a turnless session being dropped rather than archived: the headers-only
/// file has nothing in it worth keeping, and leaving it lets a later
/// [`find_transcript`] scan hand it back. Best effort — a missing file is the goal,
/// not an error — and it reports whether it removed anything, only for the log.
///
/// The recorded path is preferred, then the cwd slug, then the id scan, so it
/// finds the file even when a `--worktree` session filed it under main's slug.
pub fn delete_transcript(id: uuid::Uuid, cwd: &Path, recorded: Option<&Path>) -> bool {
    let Some(path) = transcript_file(id, cwd, recorded).or_else(|| find_transcript(id)) else {
        return false;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("could not remove transcript {}: {e}", path.display());
            false
        }
    }
}

/// The filesystem half of [`move_transcript`], split out for the same reason
/// [`crate::config::transcript_slug`] is: both ends of the real call are slugs
/// under `$HOME`, and a test that set `HOME` to reach them would change it under
/// every other test in the process.
fn relocate_file(src: &Path, dest: &Path) -> Result<PathBuf> {
    // Caught before the rename, which would otherwise be a rename onto itself: a
    // session resumed in the tree it was already in moves nowhere.
    if src == dest {
        return Ok(dest.to_path_buf());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Both slugs live under `~/.claude/projects`, so this is normally one inode
    // moving inside a directory. The copy is for the case where it is not — a `HOME`
    // that spans a mount gives `EXDEV`, which no amount of retrying fixes.
    if std::fs::rename(src, dest).is_err() {
        std::fs::copy(src, dest)
            .with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
        // Only once the copy is real. Removing the source first would put the one
        // failure that loses the conversation into the path that exists to keep it.
        std::fs::remove_file(src)
            .with_context(|| format!("removing {} after copying it", src.display()))?;
    }
    Ok(dest.to_path_buf())
}

/// How much of the transcript's head to read looking for a first turn. The
/// headers ahead of it are ~266 bytes, so this is slack for a longer preamble
/// rather than a window that has to grow with the conversation.
const FIRST_TURN_BYTES: u64 = 8 * 1024;

/// How much of the transcript's tail to read looking for a title.
const TITLE_TAIL_BYTES: u64 = 128 * 1024;

/// The longest title worth putting in a rail row.
const TITLE_MAX: usize = 120;

/// The name Claude Code gave this conversation.
///
/// Claude Code writes `{"type":"ai-title","aiTitle":"…"}` into the transcript and
/// re-writes it on every append, so the last one in the file is the current
/// answer and the tail is enough to find it. That beats anything the daemon
/// could invent: it is the same sentence `claude --resume` lists the
/// conversation under.
///
/// None is a normal answer, not a failure — the entry only appears after the
/// first exchange, and the format is Claude Code's own and undocumented. The
/// caller falls back to the workspace name, which is what the rail showed before
/// this existed.
pub fn ai_title(id: uuid::Uuid, cwd: &Path, recorded: Option<&Path>) -> Option<String> {
    let path = transcript_file(id, cwd, recorded)?;
    let tail = read_tail(&path, TITLE_TAIL_BYTES).ok()?;
    let text = String::from_utf8_lossy(&tail);
    // Skip the first line: a tail read almost always lands mid-record.
    let mut lines = text.split('\n');
    lines.next();
    let mut found = None;
    for line in lines {
        // Most of a tail is tool results, tens of KB each; parsing those to learn
        // they are not the title was the cost of every `Stop`.
        if !line.contains("\"ai-title\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("ai-title") {
            continue;
        }
        if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
            let t = t.trim();
            if !t.is_empty() {
                found = Some(t.chars().take(TITLE_MAX).collect::<String>());
            }
        }
    }
    found
}

/// The first `n` bytes of a file, or the whole thing if it is shorter.
fn read_head(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(n).read_to_end(&mut buf)?;
    Ok(buf)
}

/// The last `n` bytes of a file, or the whole thing if it is shorter.
fn read_tail(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len > n {
        f.seek(SeekFrom::Start(len - n))?;
    }
    let mut buf = Vec::with_capacity(n.min(len) as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}
