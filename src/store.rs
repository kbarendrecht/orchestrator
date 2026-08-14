use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
    fn a_restored_session_is_archived_never_live() {
        let s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        let restored = SessionRecord::of(&s).restore();
        assert!(matches!(restored.state, State::Archived { resumable: true }));
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
        assert!(matches!(restored.state, State::Archived { resumable: false }));
    }

    #[test]
    fn a_record_survives_a_round_trip() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "invoice".into(),
            Path::new("/repo/.claude/worktrees/invoice").to_path_buf(),
            Kind::Automation { pr: 4812, skill: "green".into() },
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
        assert_eq!(r.kind, Kind::Automation { pr: 4812, skill: "green".into() });
        assert!(matches!(r.recovery, Some(ArchiveState::Recoverable { .. })));
    }
}
