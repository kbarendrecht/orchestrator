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

/// Resolve a workspace-relative path, refusing anything that escapes the checkout.
///
/// This is the one endpoint that writes arbitrary bytes to disk, so containment
/// is checked against the canonical path rather than the requested string: a
/// symlink inside the workspace pointing out of it must not become a write
/// primitive. The threat is not the agent, which writes what it likes with no
/// check at all — it is a **crafted branch**, `notes.md -> ~/.ssh/id_rsa`
/// committed on a PR, and a reviewer who clicks the file link.
///
/// **The bound is the checkout, not the worktree** (#34). Every worktree lives
/// under `main_checkout` — `normalize_worktrees_subdir` refuses any other
/// arrangement — and a directory symlinked back to main is how a worktree gets
/// its untracked files here at all. Holding the editor to one worktree made that
/// ordinary layout unopenable, and it contradicted a decision already paid for:
/// [`crate::guard::check`]'s isolation rule holds an agent's *git* commands to its own
/// tree and deliberately lets its writes through, because main's branch is daemon
/// state while main's files are the repo's own business. This used to need
/// `shared_worktree_paths`, a list of directory names allowed out — which trusted
/// a *name* a branch can redefine, so `.plan -> /` reopened the hole the leaf
/// check had closed.
///
/// `.git` is the one exception, and it applies to every workspace, main included.
/// `hooks/pre-commit` and `config` each run a command on the next git invocation,
/// so the state `guard` exists to protect must not be reachable through the editor
/// instead.
pub fn resolve_in_workspace(workspace_root: &Path, rel: &str, checkout: &Path) -> Result<PathBuf> {
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
    let bound = Bound::new(&root, checkout);
    let joined = root.join(candidate);

    // The file itself may not exist yet, so canonicalize its parent.
    let parent = joined.parent().context("path has no parent")?.to_path_buf();
    let real_parent = std::fs::canonicalize(&parent)
        .with_context(|| format!("resolving {}", parent.display()))?;

    if !bound.holds(&real_parent) {
        bail!("{}", bound.escaped(rel, &root, &real_parent));
    }
    let resolved = real_parent.join(joined.file_name().context("path has no file name")?);
    refuse_git_dir(rel, &resolved)?;

    /* **The leaf may be a symlink too, and canonicalising the parent says nothing
    about it.** `read` follows it, so a link committed on a PR branch —
    `notes.md -> /home/you/.ssh/id_rsa` — turned the editor into a read
    primitive for any file the daemon can open, with the containment check
    passing because the *parent* was innocent.

    `write` never had the hole: it writes a sibling temp file and renames over
    the path, which replaces a link rather than following it. The check still
    belongs here, where both callers meet, so the next caller inherits it.

    A symlink is not refused outright — a repo that shares a directory between
    worktrees does it with links. Where it *points* is what decides. */
    let is_link = std::fs::symlink_metadata(&resolved)
        .map(|md| md.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        let target = std::fs::canonicalize(&resolved)
            .with_context(|| format!("resolving the symlink {rel}"))?;
        bound.admit(rel, &target)?;
    }
    Ok(resolved)
}

/// Where a resolved path is allowed to land: this checkout, and nothing else.
///
/// Carried as a value because three places ask the same question — the parent, the
/// leaf link, and `read`'s second look after `ELOOP` — and a fourth caller
/// answering it differently is the shape of the bug this replaced.
struct Bound {
    /// Canonical, because the answer is `starts_with` against a real path.
    ///
    /// The workspace stands in when the checkout cannot be canonicalised, which is
    /// a daemon whose `main_checkout` has been moved out from under it. That is
    /// tighter than intended and still opens the tree you are sitting in, where an
    /// empty bound would refuse every file in the product.
    at: PathBuf,
}

impl Bound {
    fn new(root: &Path, checkout: &Path) -> Self {
        Self {
            at: std::fs::canonicalize(checkout).unwrap_or_else(|_| root.to_path_buf()),
        }
    }

    fn holds(&self, path: &Path) -> bool {
        path.starts_with(&self.at)
    }

    /// The refusal, or `Ok` — the leaf-link form, which has a target to name.
    fn admit(&self, rel: &str, target: &Path) -> Result<()> {
        if self.holds(target) {
            return Ok(());
        }
        bail!(
            "{rel} is a symlink to {}, outside the checkout at {}",
            target.display(),
            self.at.display()
        );
    }

    /// **Name the symlink, because the user cannot see one** (#34).
    ///
    /// The parent check has canonicalised the whole directory by the time it
    /// fails, so it knows only that *something* escaped — and it used to say
    /// exactly that: `<path> resolves outside the workspace`, with no way to tell
    /// a shared `.plan` from a typo. Walking the requested components back and
    /// stat-ing each prefix finds which one is the link, which is the only fact
    /// that makes the refusal actionable.
    fn escaped(&self, rel: &str, root: &Path, real_parent: &Path) -> String {
        let at = self.at.display();
        match first_symlink(root, Path::new(rel)) {
            Some((name, target)) => format!(
                "{rel}: {name} is a symlink to {}, outside the checkout at {at}",
                target.display()
            ),
            None => format!(
                "{rel} resolves to {}, outside the checkout at {at}",
                real_parent.display()
            ),
        }
    }
}

/// The first component of `rel` that is a symlink, with where it points.
///
/// Stops at the first one: that is the component that moved the path out, and
/// anything under it is a consequence rather than a cause.
fn first_symlink(root: &Path, rel: &Path) -> Option<(String, PathBuf)> {
    let mut at = root.to_path_buf();
    // The leaf is excluded: a leaf link is the other refusal, which names its own
    // target and says so in those words.
    let mut components: Vec<_> = rel.components().collect();
    components.pop();
    let mut shown = PathBuf::new();
    for c in components {
        at.push(c);
        shown.push(c);
        let is_link = std::fs::symlink_metadata(&at)
            .map(|md| md.file_type().is_symlink())
            .unwrap_or(false);
        if is_link {
            // Canonical rather than the literal target: `../../../.plan` says
            // nothing to somebody who cannot see the worktree's depth.
            let target = std::fs::canonicalize(&at)
                .unwrap_or_else(|_| std::fs::read_link(&at).unwrap_or_else(|_| at.clone()));
            return Some((shown.display().to_string(), target));
        }
    }
    None
}

/// `.git` is off limits, in every workspace including main.
///
/// A write to `hooks/pre-commit` or `config` runs on the next git command, so it
/// is an execution primitive rather than an edit — and the daemon runs git
/// constantly. Main's own `.git` was already reachable this way before the bound
/// moved, so this closes a hole rather than paying for one.
fn refuse_git_dir(rel: &str, resolved: &Path) -> Result<()> {
    if resolved
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
    {
        bail!("{rel} is inside a .git directory, which this editor will not touch");
    }
    Ok(())
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

pub fn read(workspace_root: &Path, rel: &str, checkout: &Path) -> Result<FileContents> {
    use std::io::Read as _;
    let path = resolve_in_workspace(workspace_root, rel, checkout)?;
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
            Bound::new(&root, checkout).admit(rel, &target)?;
            refuse_git_dir(rel, &target)?;
            open_no_follow(&target).with_context(|| format!("opening {}", target.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let md = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?;
    if md.len() > MAX_EDIT_BYTES {
        bail!(
            "{rel} is {} bytes, past the {MAX_EDIT_BYTES} byte edit limit",
            md.len()
        );
    }
    let mut bytes = Vec::with_capacity(md.len() as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
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
    Written {
        version: String,
    },
    /// Someone else changed the file since it was loaded. Almost always an
    /// agent editing underneath you (§5), so the write is refused rather than
    /// silently clobbering their work.
    Conflict {
        on_disk: String,
        expected: String,
    },
}

pub fn write(
    workspace_root: &Path,
    rel: &str,
    content: &str,
    expected: &str,
    checkout: &Path,
) -> Result<WriteOutcome> {
    let path = resolve_in_workspace(workspace_root, rel, checkout)?;
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
        let outside = d
            .parent()
            .unwrap()
            .join(format!("orchd-edit-secret-{}", std::process::id()));
        std::fs::write(&outside, "PRIVATE KEY\n").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("src/leak.txt")).unwrap();

        let err = read(&d, "src/leak.txt", &d)
            .expect_err("a symlink out of the workspace must not be readable")
            .to_string();
        assert!(err.contains("outside the checkout"), "unhelpful: {err}");
        // And the same gate refuses the write, so neither is a way in.
        assert!(write(&d, "src/leak.txt", "x", "", &d).is_err());

        // A link that stays inside is still fine — repos do use them, and the
        // check is about where it points, not that it is a link.
        std::fs::write(d.join("src/real.txt"), "in tree\n").unwrap();
        std::os::unix::fs::symlink(d.join("src/real.txt"), d.join("src/alias.txt")).unwrap();
        assert_eq!(read(&d, "src/alias.txt", &d).unwrap().content, "in tree\n");

        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn writes_and_bumps_the_version() {
        let d = scratch("write");
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        let f = read(&d, "src/a.txt", &d).unwrap();
        assert_eq!(f.content, "one\n");

        let out = write(&d, "src/a.txt", "two\n", &f.version, &d).unwrap();
        match out {
            WriteOutcome::Written { version } => assert_ne!(version, f.version),
            other => panic!("expected Written, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(d.join("src/a.txt")).unwrap(),
            "two\n"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_to_clobber_a_file_that_moved_underneath() {
        let d = scratch("conflict");
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        let f = read(&d, "src/a.txt", &d).unwrap();

        // An agent edits the same file while the buffer is open.
        std::fs::write(d.join("src/a.txt"), "agent wrote this\n").unwrap();

        let out = write(&d, "src/a.txt", "mine\n", &f.version, &d).unwrap();
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
        let f = read(&d, "src/a.txt", &d).unwrap();
        std::fs::write(d.join("src/a.txt"), "one\n").unwrap();
        assert!(matches!(
            write(&d, "src/a.txt", "two\n", &f.version, &d).unwrap(),
            WriteOutcome::Written { .. }
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refuses_paths_that_escape_the_workspace() {
        let d = scratch("escape");
        assert!(resolve_in_workspace(&d, "../outside.txt", &d).is_err());
        assert!(resolve_in_workspace(&d, "/etc/passwd", &d).is_err());
        assert!(resolve_in_workspace(&d, "src/../../x", &d).is_err());
        assert!(resolve_in_workspace(&d, "", &d).is_err());
        assert!(resolve_in_workspace(&d, "src/a.txt", &d).is_ok());
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
        let seen = read(&d, "src/alias.txt", &d).expect("an in-tree link reads");
        assert_eq!(seen.content, "real\n");
        let err = write(&d, "src/alias.txt", "other\n", &seen.version, &d)
            .expect_err("a write through a link must be refused")
            .to_string();
        assert!(err.contains("symlink"), "{err}");
        assert!(
            std::fs::symlink_metadata(d.join("src/alias.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("src/real.txt")).unwrap(),
            "real\n"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_symlink_out_of_the_workspace_is_not_a_write_primitive() {
        let d = scratch("symlink");
        let outside = std::env::temp_dir().join(format!("orchd-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("escape")).unwrap();
        assert!(resolve_in_workspace(&d, "escape/evil.txt", &d).is_err());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// **A worktree sharing a directory back to main is the ordinary layout** (#34).
    ///
    /// The bound used to be the worktree, so `.plan -> ../../../.plan` — which is
    /// how a worktree is given its untracked files here — made every file under it
    /// unopenable, and the refusal named neither the link nor the setting that
    /// allowed it. The checkout is the bound now, and that link needs no setting at
    /// all. A link leaving the checkout is still refused, which is the case the
    /// check exists for.
    #[cfg(unix)]
    #[test]
    fn a_directory_shared_back_to_main_resolves_and_one_leaving_the_checkout_does_not() {
        // A checkout shaped like the real one: main, and a worktree under it.
        let checkout = crate::testutil::scratch("edit-checkout");
        let wt = checkout.join(".claude/worktrees/feature");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(checkout.join(".plan")).unwrap();
        std::fs::write(checkout.join(".plan/notes.md"), "shared\n").unwrap();
        std::os::unix::fs::symlink("../../../.plan", wt.join(".plan")).unwrap();

        assert_eq!(
            read(&wt, ".plan/notes.md", &checkout).unwrap().content,
            "shared\n",
            "a directory shared back to main is inside the checkout"
        );

        // And out of the checkout is out. Its own name: another test in this file
        // uses `orchd-outside-<pid>` and deletes it, so sharing the name makes both
        // flaky under a parallel run.
        let outside =
            std::env::temp_dir().join(format!("orchd-off-checkout-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("escape")).unwrap();
        let err = resolve_in_workspace(&wt, "escape/evil.txt", &checkout)
            .map(|_| ())
            .expect_err("a link out of the checkout must be refused")
            .to_string();
        // **The refusal names the link** — the whole of #34. Before this it said
        // only "resolves outside the workspace", so a shared directory and a typo
        // produced the same sentence.
        assert!(
            err.contains("escape is a symlink to"),
            "the refusal must name the symlink: {err}"
        );
        assert!(
            err.contains(
                &std::fs::canonicalize(&outside)
                    .unwrap()
                    .display()
                    .to_string()
            ),
            "and where it points: {err}"
        );

        let _ = std::fs::remove_dir_all(&checkout);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// **`.git` is an execution primitive, not a file** (#34).
    ///
    /// A write to `hooks/pre-commit` or to `config` runs on the next git command,
    /// and the daemon runs git constantly. Main's own `.git` was reachable through
    /// the editor before the bound moved to the checkout, so this closes a hole
    /// rather than paying for one.
    #[cfg(unix)]
    #[test]
    fn nothing_inside_a_git_directory_is_editable() {
        let d = scratch("gitdir");
        std::fs::create_dir_all(d.join(".git/hooks")).unwrap();
        std::fs::write(d.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();

        let err = resolve_in_workspace(&d, ".git/hooks/pre-commit", &d)
            .map(|_| ())
            .expect_err("a hook is not an editable file")
            .to_string();
        assert!(err.contains(".git"), "{err}");
        assert!(read(&d, ".git/config", &d).is_err());

        // And not through a link either: the leaf check asks the same question.
        std::os::unix::fs::symlink(d.join(".git/hooks/pre-commit"), d.join("src/hook")).unwrap();
        assert!(
            read(&d, "src/hook", &d).is_err(),
            "a link into .git is still .git"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn binary_files_are_refused_rather_than_mangled() {
        let d = scratch("binary");
        std::fs::write(d.join("src/x.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert!(read(&d, "src/x.bin", &d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
