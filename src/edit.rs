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
/// primitive. A repo that shares a directory across worktrees on purpose names it
/// in `shared_worktree_paths`, and only those are allowed through — an exception
/// that is configured rather than accidentally permitted. `shared` empty is the
/// tight case and the default.
pub fn resolve_in_workspace(workspace_root: &Path, rel: &str, shared: &[String]) -> Result<PathBuf> {
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

    if !inside(&real_parent, &root, shared) {
        bail!("{rel} resolves outside the workspace");
    }
    let resolved = real_parent.join(joined.file_name().context("path has no file name")?);

    /* **The leaf may be a symlink too, and canonicalising the parent says nothing
       about it.** `read` follows it, so a link committed on a PR branch —
       `notes.md -> /home/you/.ssh/id_rsa` — turned the editor into a read
       primitive for any file the daemon can open, with the containment check
       passing because the *parent* was innocent.

       `write` never had the hole: it writes a sibling temp file and renames over
       the path, which replaces a link rather than following it. The check still
       belongs here, where both callers meet, so the next caller inherits it.

       A symlink is not refused outright — a repo that shares a directory between
       worktrees does it with links, which is what `shared_worktree_paths` is for.
       Where it *points* is what decides. */
    let is_link = std::fs::symlink_metadata(&resolved)
        .map(|md| md.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        let target = std::fs::canonicalize(&resolved)
            .with_context(|| format!("resolving the symlink {rel}"))?;
        if !inside(&target, &root, shared) {
            bail!(
                "{rel} is a symlink to {}, which is outside the workspace",
                target.display()
            );
        }
    }
    Ok(resolved)
}

/// Is this resolved path within the workspace, or within a directory the repo
/// declared shared on purpose?
///
/// A declared shared directory resolves outside the worktree by design, because it
/// is a symlink back to main. Nothing else that leaves the workspace is allowed,
/// and with none declared that is every case.
fn inside(path: &Path, root: &Path, shared: &[String]) -> bool {
    if path.starts_with(root) {
        return true;
    }
    shared.iter().any(|entry| {
        std::fs::canonicalize(root.join(entry))
            .map(|real| path.starts_with(&real))
            .unwrap_or(false)
    })
}

fn version_of(bytes: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Open `path` for reading without following a final symlink. `ELOOP` is the
/// kernel saying "that is a link", which is the one answer a stat-then-open
/// cannot give without a window between the two.
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub fn read(workspace_root: &Path, rel: &str, shared: &[String]) -> Result<FileContents> {
    use std::io::Read as _;
    let path = resolve_in_workspace(workspace_root, rel, shared)?;
    // `resolve_in_workspace` checked where a link points, but a stat followed by
    // an open is two steps, and a path can be made a link between them. So the
    // open itself refuses to follow: a plain file opens; a link comes back `ELOOP`,
    // its target is checked again and opened the same way, and a target that is
    // itself a link is refused rather than followed anywhere.
    let mut file = match open_no_follow(&path) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            let root = std::fs::canonicalize(workspace_root)
                .with_context(|| format!("resolving {}", workspace_root.display()))?;
            let target = std::fs::canonicalize(&path)
                .with_context(|| format!("resolving the symlink {rel}"))?;
            if !inside(&target, &root, shared) {
                bail!(
                    "{rel} is a symlink to {}, which is outside the workspace",
                    target.display()
                );
            }
            open_no_follow(&target).with_context(|| format!("opening {}", target.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let md = file.metadata().with_context(|| format!("stat {}", path.display()))?;
    if md.len() > MAX_EDIT_BYTES {
        bail!(
            "{rel} is {} bytes, past the {MAX_EDIT_BYTES} byte edit limit",
            md.len()
        );
    }
    let mut bytes = Vec::with_capacity(md.len() as usize);
    file.read_to_end(&mut bytes).with_context(|| format!("reading {}", path.display()))?;
    let content = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("{rel} is not UTF-8, so it is not editable here"))?;
    Ok(FileContents {
        path: rel.to_string(),
        version: version_of(content.as_bytes()),
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

pub fn write(
    workspace_root: &Path,
    rel: &str,
    content: &str,
    expected: &str,
    shared: &[String],
) -> Result<WriteOutcome> {
    let path = resolve_in_workspace(workspace_root, rel, shared)?;
    // Never through a link. The rename below replaces whatever is at `path`, so a
    // write onto an in-tree symlink turned the link into a regular file: a
    // `typechange` in git, and the file it pointed at untouched. Links are readable
    // here on purpose; they are not a thing this editor rewrites.
    let is_link = std::fs::symlink_metadata(&path)
        .map(|md| md.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        bail!("{rel} is a symlink; edit the file it points at instead");
    }
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
    /// No shared directories declared — the default, and the tight case.
    const NONE: &[String] = &[];

    use super::*;

    /// A workspace root with a `src/` in it, which is what every case below edits
    /// through.
    fn scratch(name: &str) -> PathBuf {
        let d = crate::testutil::scratch(&format!("edit-{name}"));
        std::fs::create_dir_all(d.join("src")).unwrap();
        d
    }

    /// **A leaf symlink is the hole canonicalising the parent leaves open.** A link
    /// committed on a PR branch — and a PR branch is somebody else's content — made
    /// the editor read any file the daemon can open, because the containment check
    /// only ever looked at the directory the link sat in.
    /// Distinct from `..._is_not_a_write_primitive` below, which links a *directory*
    /// and is caught by the parent check. This is the leaf, which was not.
    #[cfg(unix)]
    #[test]
    fn a_leaf_symlink_out_of_the_workspace_is_refused() {
        // Its own scratch name: `scratch` wipes the directory, so sharing one with
        // another test makes both flaky under a parallel run.
        let d = scratch("leaf-symlink");
        // The secret lives outside the workspace, as `~/.ssh/id_rsa` would.
        let outside = d.parent().unwrap().join(format!(
            "orchd-edit-secret-{}",
            std::process::id()
        ));
        std::fs::write(&outside, "PRIVATE KEY\n").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("src/leak.txt")).unwrap();

        let err = read(&d, "src/leak.txt", NONE)
            .expect_err("a symlink out of the workspace must not be readable")
            .to_string();
        assert!(err.contains("outside the workspace"), "unhelpful: {err}");
        // And the same gate refuses the write, so neither is a way in.
        assert!(write(&d, "src/leak.txt", "x", "", NONE).is_err());

        // A link that stays inside is still fine — repos do use them, and the
        // check is about where it points, not that it is a link.
        std::fs::write(d.join("src/real.txt"), "in tree\n").unwrap();
        std::os::unix::fs::symlink(d.join("src/real.txt"), d.join("src/alias.txt")).unwrap();
        assert_eq!(read(&d, "src/alias.txt", NONE).unwrap().content, "in tree\n");

        // A declared shared directory is the configured exception, and a link into
        // it resolves.
        let shared_dir = d.parent().unwrap().join(format!("orchd-edit-shared-{}", std::process::id()));
        std::fs::create_dir_all(&shared_dir).unwrap();
        std::fs::write(shared_dir.join("vendored.txt"), "shared\n").unwrap();
        std::os::unix::fs::symlink(&shared_dir, d.join("vendor")).unwrap();
        let shared = vec!["vendor".to_string()];
        assert_eq!(
            read(&d, "vendor/vendored.txt", &shared).unwrap().content,
            "shared\n"
        );

        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&shared_dir);
    }

    #[test]
    fn writes_and_bumps_the_version() {
        let d = scratch("write");
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        let f = read(&d, "src/a.txt", NONE).unwrap();
        assert_eq!(f.content, "one\n");

        let out = write(&d, "src/a.txt", "two\n", &f.version, NONE).unwrap();
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
        let f = read(&d, "src/a.txt", NONE).unwrap();

        // An agent edits the same file while the buffer is open.
        std::fs::write(d.join("src/a.txt"), "agent wrote this\n").unwrap();

        let out = write(&d, "src/a.txt", "mine\n", &f.version, NONE).unwrap();
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
        let f = read(&d, "src/a.txt", NONE).unwrap();
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        assert!(matches!(
            write(&d, "src/a.txt", "two\n", &f.version, NONE).unwrap(),
            WriteOutcome::Written { .. }
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_paths_that_escape_the_workspace() {
        let d = scratch("escape");
        assert!(resolve_in_workspace(&d, "../outside.txt", NONE).is_err());
        assert!(resolve_in_workspace(&d, "/etc/passwd", NONE).is_err());
        assert!(resolve_in_workspace(&d, "src/../../x", NONE).is_err());
        assert!(resolve_in_workspace(&d, "", NONE).is_err());
        assert!(resolve_in_workspace(&d, "src/a.txt", NONE).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A write lands by rename, which replaces a link rather than following it.
    /// So an in-tree link, which `read` allows, is refused by `write`: the
    /// alternative was a `typechange` commit and the real file left as it was.
    #[test]
    fn a_write_through_an_in_tree_symlink_is_refused() {
        let d = scratch("write-link");
        std::fs::write(d.join("src/real.txt"), "real\n").unwrap();
        std::os::unix::fs::symlink(d.join("src/real.txt"), d.join("src/alias.txt")).unwrap();
        let seen = read(&d, "src/alias.txt", NONE).expect("an in-tree link reads");
        assert_eq!(seen.content, "real\n");
        let err = write(&d, "src/alias.txt", "other\n", &seen.version, NONE)
            .expect_err("a write through a link must be refused")
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(
            std::fs::symlink_metadata(d.join("src/alias.txt")).unwrap().file_type().is_symlink(),
            "the link was replaced"
        );
        assert_eq!(std::fs::read_to_string(d.join("src/real.txt")).unwrap(), "real\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_symlink_out_of_the_workspace_is_not_a_write_primitive() {
        let d = scratch("symlink");
        let outside = std::env::temp_dir().join(format!("orchd-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("escape")).unwrap();
        assert!(resolve_in_workspace(&d, "escape/evil.txt", NONE).is_err());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_declared_shared_directory_is_allowed_out_and_only_when_declared() {
        // The case this exists for: a directory symlinked back to main on purpose,
        // shared across every worktree. It used to be hardcoded as `.plan/`, which
        // gave every other repo a carve-out for a convention it does not have.
        let d = scratch("plan");
        let shared_real = std::env::temp_dir().join(format!("orchd-plan-{}", std::process::id()));
        std::fs::create_dir_all(&shared_real).unwrap();
        std::os::unix::fs::symlink(&shared_real, d.join(".plan")).unwrap();

        // A second escape hatch that is *not* declared, so the last assertion
        // fails on the rule rather than on the path not existing.
        let other_real = std::env::temp_dir().join(format!("orchd-other-{}", std::process::id()));
        std::fs::create_dir_all(&other_real).unwrap();
        std::os::unix::fs::symlink(&other_real, d.join("escape")).unwrap();

        let declared = [".plan".to_string()];
        assert!(resolve_in_workspace(&d, ".plan/notes.md", &declared).is_ok());
        // And undeclared it is just another symlink out of the workspace, which is
        // the whole point of the containment rule.
        assert!(resolve_in_workspace(&d, ".plan/notes.md", NONE).is_err());
        // Declaring one does not open the others — this one resolves fine and is
        // still refused.
        assert!(resolve_in_workspace(&d, "escape/evil.txt", &declared).is_err());
        let _ = std::fs::remove_dir_all(&other_real);

        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&shared_real);
    }

    #[test]
    fn binary_files_are_refused_rather_than_mangled() {
        let d = scratch("binary");
        std::fs::write(d.join("src/x.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert!(read(&d, "src/x.bin", NONE).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
