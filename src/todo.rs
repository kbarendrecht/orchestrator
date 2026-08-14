use anyhow::Result;
use std::path::{Path, PathBuf};

/// Markers around the generated section.
///
/// Same shape as `worktree-link`'s managed block in the shared git exclude: the
/// block is rewritten from scratch every time so it can never drift or
/// duplicate, and everything outside it is yours.
const BEGIN: &str = "<!-- >>> orchd live findings >>> -->";
const END: &str = "<!-- <<< orchd live findings <<< -->";

/// Something the daemon noticed that wants a human decision.
///
/// Only conditions that are true *now* go in here. A findings list that
/// accumulates history stops being read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub what: String,
    pub why: String,
}

pub fn default_path() -> PathBuf {
    // The orchestrator's own checkout, which is where the file lives.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("TODO.md")
}

/// Rewrite the generated block, leaving the hand-written parts alone.
pub fn update(path: &Path, findings: &[Finding]) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let block = render(findings);

    let next = match (existing.find(BEGIN), existing.find(END)) {
        (Some(a), Some(b)) if b > a => {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..a]);
            out.push_str(&block);
            out.push_str(&existing[b + END.len()..]);
            out
        }
        // No block yet, or a half-written one: append a fresh block rather than
        // guessing where a broken one ended.
        _ => {
            let mut out = existing.trim_end().to_string();
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&block);
            out.push('\n');
            out
        }
    };

    if next != existing {
        std::fs::write(path, next)?;
    }
    Ok(())
}

fn render(findings: &[Finding]) -> String {
    let mut s = String::from(BEGIN);
    s.push_str("\n\n## Live findings\n\n");
    s.push_str("Rewritten by the daemon on every poll. Edit anything outside this block.\n\n");
    if findings.is_empty() {
        s.push_str("Nothing outstanding.\n");
    } else {
        for f in findings {
            s.push_str(&format!("- **{}** — {}\n", f.what, f.why));
        }
    }
    s.push('\n');
    s.push_str(END);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orchd-todo-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("TODO.md")
    }

    fn finding(w: &str) -> Finding {
        Finding {
            what: w.into(),
            why: "because".into(),
        }
    }

    #[test]
    fn creates_the_block_when_the_file_is_new() {
        let p = tmp("new");
        update(&p, &[finding("a")]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains(BEGIN) && s.contains(END));
        assert!(s.contains("**a**"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn hand_written_text_survives_a_rewrite() {
        let p = tmp("keep");
        std::fs::write(&p, format!("# My notes\n\nkeep me\n\n{BEGIN}\nold\n{END}\n\ntrailing note\n"))
            .unwrap();
        update(&p, &[finding("b")]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("keep me"));
        assert!(s.contains("trailing note"));
        assert!(s.contains("**b**"));
        assert!(!s.contains("old"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn rewriting_twice_does_not_duplicate_the_block() {
        let p = tmp("dup");
        update(&p, &[finding("a")]).unwrap();
        update(&p, &[finding("c")]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert_eq!(s.matches(BEGIN).count(), 1);
        assert_eq!(s.matches(END).count(), 1);
        assert!(s.contains("**c**") && !s.contains("**a**"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_empty_findings_list_says_so_rather_than_leaving_the_last_one() {
        let p = tmp("empty");
        update(&p, &[finding("a")]).unwrap();
        update(&p, &[]).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("Nothing outstanding"));
        assert!(!s.contains("**a**"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn an_unchanged_block_does_not_rewrite_the_file() {
        let p = tmp("noop");
        update(&p, &[finding("a")]).unwrap();
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        update(&p, &[finding("a")]).unwrap();
        let after = std::fs::metadata(&p).unwrap().modified().unwrap();
        // Otherwise the daemon would dirty its own repo every five minutes.
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
