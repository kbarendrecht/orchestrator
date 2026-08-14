use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Largest file the editor will open. Past this the diff is still viewable, but
/// loading it into a browser buffer helps nobody.
const MAX_EDIT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct FileContents {
    pub path: String,
    pub content: String,
    /// Content hash, not mtime. A checkout or a `git stash` can restore an
    /// identical file with a new mtime, and that is not a conflict.
    pub version: String,
    pub bytes: u64,
}

/// Resolve a workspace-relative path, refusing anything that escapes.
///
/// This is the one endpoint that writes arbitrary bytes to disk, so containment
/// is checked against the canonical path rather than the requested string: a
/// symlink inside the workspace pointing out of it must not become a write
/// primitive. `.plan/` is deliberately allowed through that rule — it is a
/// symlink to main's `.plan/` by design (§4) — so it is special-cased rather
/// than accidentally permitted.
pub fn resolve_in_workspace(workspace_root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        bail!("no path given");
    }
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        bail!("path must be relative to the workspace");
    }
    if candidate
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("path may not contain ..");
    }

    let root = std::fs::canonicalize(workspace_root)
        .with_context(|| format!("resolving {}", workspace_root.display()))?;
    let joined = root.join(candidate);

    // The file itself may not exist yet, so canonicalize its parent.
    let parent = joined
        .parent()
        .context("path has no parent")?
        .to_path_buf();
    let real_parent = std::fs::canonicalize(&parent)
        .with_context(|| format!("resolving {}", parent.display()))?;

    if real_parent.starts_with(&root) {
        return Ok(real_parent.join(joined.file_name().context("path has no file name")?));
    }

    // `.plan/` resolves outside the worktree on purpose: it is a symlink to
    // main's `.plan/` (§4). Nothing else that leaves the workspace is allowed.
    let plan = root.join(".plan");
    if let Ok(real_plan) = std::fs::canonicalize(&plan) {
        if real_parent.starts_with(&real_plan) {
            return Ok(real_parent.join(joined.file_name().context("path has no file name")?));
        }
    }

    bail!("{rel} resolves outside the workspace")
}

pub fn version_of(bytes: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn read(workspace_root: &Path, rel: &str) -> Result<FileContents> {
    let path = resolve_in_workspace(workspace_root, rel)?;
    let md = std::fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    if md.len() > MAX_EDIT_BYTES {
        bail!(
            "{rel} is {} bytes, past the {MAX_EDIT_BYTES} byte edit limit",
            md.len()
        );
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let content = String::from_utf8(bytes.clone())
        .map_err(|_| anyhow::anyhow!("{rel} is not UTF-8, so it is not editable here"))?;
    Ok(FileContents {
        path: rel.to_string(),
        version: version_of(&bytes),
        bytes: md.len(),
        content,
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum WriteOutcome {
    Written { version: String },
    /// Someone else changed the file since it was loaded. Almost always an
    /// agent editing underneath you (§5), so the write is refused rather than
    /// silently clobbering their work.
    Conflict { on_disk: String, expected: String },
}

pub fn write(workspace_root: &Path, rel: &str, content: &str, expected: &str) -> Result<WriteOutcome> {
    let path = resolve_in_workspace(workspace_root, rel)?;
    let current = std::fs::read(&path).unwrap_or_default();
    let on_disk = version_of(&current);
    if on_disk != expected {
        return Ok(WriteOutcome::Conflict {
            on_disk,
            expected: expected.to_string(),
        });
    }

    // Write-and-rename, so a crash mid-write cannot truncate a source file.
    let tmp = path.with_extension(format!(
        "{}.orchd-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, content.as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    // Preserve the original mode; the temp file is created with a default one.
    if let Ok(md) = std::fs::metadata(&path) {
        let _ = std::fs::set_permissions(&tmp, md.permissions());
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;

    Ok(WriteOutcome::Written {
        version: version_of(content.as_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orchd-edit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        d
    }

    #[test]
    fn writes_and_bumps_the_version() {
        let d = scratch("write");
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        let f = read(&d, "src/a.txt").unwrap();
        assert_eq!(f.content, "one\n");

        let out = write(&d, "src/a.txt", "two\n", &f.version).unwrap();
        match out {
            WriteOutcome::Written { version } => assert_ne!(version, f.version),
            other => panic!("expected Written, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(d.join("src/a.txt")).unwrap(), "two\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_to_clobber_a_file_that_moved_underneath() {
        let d = scratch("conflict");
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        let f = read(&d, "src/a.txt").unwrap();

        // An agent edits the same file while the buffer is open.
        std::fs::write(d.join("src/a.txt"), "agent wrote this\n").unwrap();

        let out = write(&d, "src/a.txt", "mine\n", &f.version).unwrap();
        assert!(matches!(out, WriteOutcome::Conflict { .. }));
        // The agent's work survives.
        assert_eq!(
            std::fs::read_to_string(d.join("src/a.txt")).unwrap(),
            "agent wrote this\n"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_identical_rewrite_is_not_a_conflict() {
        // Content hash rather than mtime: a checkout that restores identical
        // bytes must not read as someone else's edit.
        let d = scratch("same");
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        let f = read(&d, "src/a.txt").unwrap();
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        assert!(matches!(
            write(&d, "src/a.txt", "two\n", &f.version).unwrap(),
            WriteOutcome::Written { .. }
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_paths_that_escape_the_workspace() {
        let d = scratch("escape");
        assert!(resolve_in_workspace(&d, "../outside.txt").is_err());
        assert!(resolve_in_workspace(&d, "/etc/passwd").is_err());
        assert!(resolve_in_workspace(&d, "src/../../x").is_err());
        assert!(resolve_in_workspace(&d, "").is_err());
        assert!(resolve_in_workspace(&d, "src/a.txt").is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_symlink_out_of_the_workspace_is_not_a_write_primitive() {
        let d = scratch("symlink");
        let outside = std::env::temp_dir().join(format!("orchd-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("escape")).unwrap();
        assert!(resolve_in_workspace(&d, "escape/evil.txt").is_err());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn plan_is_allowed_out_because_it_is_meant_to_be() {
        // `.plan/` is a symlink to main's `.plan/` by design (§4).
        let d = scratch("plan");
        let main_plan = std::env::temp_dir().join(format!("orchd-plan-{}", std::process::id()));
        std::fs::create_dir_all(&main_plan).unwrap();
        std::os::unix::fs::symlink(&main_plan, d.join(".plan")).unwrap();
        assert!(resolve_in_workspace(&d, ".plan/notes.md").is_ok());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&main_plan);
    }

    #[test]
    fn binary_files_are_refused_rather_than_mangled() {
        let d = scratch("binary");
        std::fs::write(d.join("src/x.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert!(read(&d, "src/x.bin").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
