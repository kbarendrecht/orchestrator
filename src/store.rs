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

fn path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("sessions.json"))
}

fn automation_path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("automation.json"))
}

/// §8 says SQLite; a JSON file with the same write-and-rename discipline holds
/// a handful of PR numbers just as safely and keeps the dependency list short.
pub fn save_automation(store: &crate::green::AutomationStore) -> Result<()> {
    let p = automation_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// A restart must not resurrect a `Running` state whose session is gone (§8).
pub fn load_automation() -> crate::green::AutomationStore {
    let Ok(p) = automation_path() else {
        return Default::default();
    };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return Default::default();
    };
    let mut store: crate::green::AutomationStore = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("could not parse {}: {e}", p.display());
            return Default::default();
        }
    };
    // Orphaned Running is demoted to Exhausted: the run is not going to finish,
    // and pretending it might would block the PR forever.
    for state in store.by_pr.values_mut() {
        if let crate::green::PrAutomation::Running { .. } = state {
            *state = crate::green::PrAutomation::Exhausted {
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
                command: "green".into(),
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
                command: "green".into()
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
    if let Some(p) = recorded {
        if p.exists() {
            return true;
        }
    }
    crate::config::transcript_dir_for(cwd)
        .map(|d| d.join(format!("{id}.jsonl")).exists())
        .unwrap_or(false)
}
