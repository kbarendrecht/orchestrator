use anyhow::Result;
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
}

impl SessionRecord {
    pub fn of(s: &Session) -> Self {
        SessionRecord {
            id: s.id,
            workspace: s.workspace.clone(),
            cwd: s.cwd.clone(),
            kind: s.kind.clone(),
            title: s.title.clone(),
            transcript_path: s.transcript_path.clone(),
            archived_transcript: s.archived_transcript.clone(),
            transcript_archived: s.transcript_archived,
            recovery: s.recovery.clone(),
            created_at: s.created_at,
            pid: s.pid,
            was_live: s.state.is_live(),
        }
    }

    /// Rebuild a session from its record. It comes back `Archived`, never live:
    /// the daemon owned the pty and restarting it killed the process.
    pub fn restore(self) -> Session {
        let resumable = !matches!(self.recovery, Some(ArchiveState::TranscriptOnly));
        let mut s = Session::new(self.id, self.workspace, self.cwd, self.kind);
        s.title = self.title;
        s.transcript_path = self.transcript_path;
        s.archived_transcript = self.archived_transcript;
        s.transcript_archived = self.transcript_archived;
        s.recovery = self.recovery;
        s.created_at = self.created_at;
        s.pid = self.pid;
        s.state = State::Archived { resumable };
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

/// §8 says SQLite; a JSON file with the same write-and-rename discipline holds
/// a handful of PR numbers just as safely and keeps the dependency list short.
pub fn save_automation(store: &crate::fix_pr::AutomationStore) -> Result<()> {
    let p = automation_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// A restart must not resurrect a `Running` state whose session is gone (§8).
pub fn load_automation() -> crate::fix_pr::AutomationStore {
    let Ok(p) = automation_path() else {
        return Default::default();
    };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return Default::default();
    };
    let mut store: crate::fix_pr::AutomationStore = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("could not parse {}: {e}", p.display());
            return Default::default();
        }
    };
    // Orphaned Running is demoted to Exhausted: the run is not going to finish,
    // and pretending it might would block the PR forever.
    for state in store.by_pr.values_mut() {
        if let crate::fix_pr::PrAutomation::Running { .. } = state {
            *state = crate::fix_pr::PrAutomation::Exhausted {
                at_head: String::new(),
                at: std::time::SystemTime::now(),
            };
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
    let p = stories_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cache)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn load_stories() -> crate::story::Cache {
    let Ok(p) = stories_path() else {
        return Default::default();
    };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return Default::default();
    };
    match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("could not parse {}: {e}", p.display());
            Default::default()
        }
    }
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
    let p = manual_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(phases)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn load_manual() -> std::collections::HashMap<u64, crate::post::ManualPhase> {
    let Ok(p) = manual_path() else {
        return Default::default();
    };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return Default::default();
    };
    match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            // Degrading to empty costs the resume, which is bad but recoverable by
            // hand; refusing to boot would cost every session.
            tracing::warn!("could not parse {}: {e}", p.display());
            Default::default()
        }
    }
}

pub fn save(records: &[SessionRecord]) -> Result<()> {
    let p = path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    // Write-and-rename so a crash mid-write cannot leave a truncated file that
    // would lose every record at once.
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(records)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn load() -> Vec<SessionRecord> {
    let Ok(p) = path() else { return Vec::new() };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            // A corrupt store must not stop the daemon booting; it only costs
            // the resume offers.
            tracing::warn!("could not parse {}: {e}", p.display());
            Vec::new()
        }
    }
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

    #[test]
    fn a_restored_session_is_archived_never_live() {
        let s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        let restored = SessionRecord::of(&s).restore();
        assert!(matches!(
            restored.state,
            State::Archived { resumable: true }
        ));
        assert!(!restored.state.is_live());
    }

    #[test]
    fn a_transcript_only_session_restores_as_unresumable() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
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

/// Whether there is a transcript to resume this session from.
///
/// `claude --resume <id>` reads `~/.claude/projects/<cwd-slug>/<id>.jsonl`. A
/// session killed before its first turn never gets one, and resuming from a
/// missing file would just open an empty session under an old name.
pub fn resumable(record: &SessionRecord) -> bool {
    transcript_exists(record.id, &record.cwd, record.transcript_path.as_deref())
}

/// Whether a conversation was ever written to disk.
///
/// A session killed before its first turn has no `.jsonl` at all, and there is
/// nothing to come back to: `claude --resume` answers "no conversation found"
/// and exits. Both the startup resume and the rail's archive ask this, so they
/// ask it the same way.
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
