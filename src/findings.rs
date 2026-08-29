//! Live daemon findings, written to a gitignored `daemon.log` — and only when
//! the daemon is managing orchd's *own* checkout (dogfooding).
//!
//! This used to splice a block into the tracked `TODO.md`, keyed on the build
//! time source dir, so any build — including a throwaway one pointed at another
//! repo — dirtied this repo's `TODO.md`. Now it writes a gitignored log, and
//! only when the repo it is managing *is* this source tree, so a daemon pointed
//! anywhere else writes nothing and never touches a repo it was not asked to.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Something the daemon noticed that wants a human decision.
///
/// Only conditions that are true *now* go in here. A findings list that
/// accumulates history stops being read, so the file is overwritten each poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub what: String,
    pub why: String,
}

/// The findings-log path, but only when the daemon is managing orchd's own
/// checkout. Keyed on the build-time source dir: if the repo being managed is
/// *not* this source tree, there is nothing to dogfood and nothing is written.
/// `main_checkout` is already canonicalized in `Config::parse`; canonicalize the
/// source dir to match, so `/tmp` symlinks on macOS do not read as different.
pub fn dogfood_log(main_checkout: &Path) -> Option<PathBuf> {
    let src = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).ok()?;
    let here = std::fs::canonicalize(main_checkout).ok()?;
    (src == here).then(|| here.join("daemon.log"))
}

/// Overwrite the findings log with what is true now. Skips the write when the
/// content is unchanged, so a stationary daemon does not rewrite the file — and
/// bump its mtime — every poll.
pub fn write_log(path: &Path, findings: &[Finding]) -> Result<()> {
    let next = render(findings);
    if std::fs::read_to_string(path).ok().as_deref() == Some(next.as_str()) {
        return Ok(());
    }
    std::fs::write(path, next)?;
    Ok(())
}

fn render(findings: &[Finding]) -> String {
    let mut s =
        String::from("orchd live findings — rewritten every poll; only what is true now.\n\n");
    if findings.is_empty() {
        s.push_str("Nothing outstanding.\n");
    } else {
        for f in findings {
            s.push_str(&format!("- {} — {}\n", f.what, f.why));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("orchd-findings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("daemon.log")
    }

    fn finding(w: &str) -> Finding {
        Finding { what: w.into(), why: "because".into() }
    }

    #[test]
    fn writes_the_findings() {
        let p = tmp("write");
        write_log(&p, &[finding("a")]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("a — because"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_empty_list_says_so_rather_than_leaving_the_last_one() {
        let p = tmp("empty");
        write_log(&p, &[finding("a")]).unwrap();
        write_log(&p, &[]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("Nothing outstanding"));
        assert!(!s.contains("a — because"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn overwrites_rather_than_accumulating() {
        let p = tmp("over");
        write_log(&p, &[finding("a")]).unwrap();
        write_log(&p, &[finding("c")]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("c — because") && !s.contains("a — because"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_unchanged_list_does_not_rewrite_the_file() {
        let p = tmp("noop");
        write_log(&p, &[finding("a")]).unwrap();
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_log(&p, &[finding("a")]).unwrap();
        let after = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn dogfood_only_for_the_source_tree_itself() {
        // The source dir is dogfooding; any other repo is not.
        let own = dogfood_log(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(own.is_some());
        assert!(own.unwrap().ends_with("daemon.log"));
        let d = tmp("elsewhere");
        assert!(dogfood_log(d.parent().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(d.parent().unwrap());
    }
}
