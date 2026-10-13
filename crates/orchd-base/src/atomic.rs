//! How a durable file is written, and what happens to one that will not read.
//!
//! **One discipline for every file the app keeps**, which is why it lives in the
//! bottom crate. It was `store::save_json`, and only the daemon's own stores went
//! through it: `config.json`, `host.json` and `recent.json` are written by
//! `orchd-repo` and the host, which cannot reach `orchd`, so each was a plain
//! `fs::write` — truncate, then fill. A crash or a power loss between the two left
//! an empty or half-written file, and a `host.json` that will not parse read as
//! "no file", so the app fell back to the legacy config and the next write of the
//! list rebuilt the host file from defaults.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `bytes` to `path` by writing a sibling and renaming it over the original,
/// so a reader sees the old file or the new one and never a torn one.
///
/// Creates the parent directory. Synchronous file IO: call it from a blocking
/// context, never from a tokio worker.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    /* **A tmp name unique to this write.** Every store shared one
    `<name>.json.tmp`, so two overlapping writers used the same scratch file:
    both wrote it, the first rename moved it away, and the second failed
    `ENOENT` — surfacing as "could not persist session records" for a write that
    had nothing wrong with it. The counter is per process and the pid is in the
    name, so neither two writers here nor two daemons can collide. */
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = sibling(
        path,
        &format!(
            "tmp.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ),
    );

    let fill = || -> Result<()> {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        // The rename is atomic in the directory, but it does not promise the
        // *contents* are on disk first: without this, a power loss can leave the
        // real name pointing at a file whose blocks were never written — which is
        // the truncated-file case the write-and-rename exists to prevent.
        f.sync_all()
            .with_context(|| format!("flushing {}", tmp.display()))?;
        Ok(())
    };
    if let Err(e) = fill() {
        // Never leave scratch behind; the name is unique, so nothing else will
        // clean it up.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))?;
    Ok(())
}

/// Move a file that will not parse out of the way, as `<name>.corrupt`, and say so.
///
/// **Before the caller falls back to a default, never instead of it.** The default
/// is what the next write puts there, so defaulting in place did not merely lose
/// the reading — it *overwrote the only copy* moments later. Renamed rather than
/// copied, so there is exactly one of it and the caller still starts. `why` is the
/// parse error, for the log line.
pub fn set_aside(path: &Path, why: &dyn std::fmt::Display) {
    let aside = sibling(path, "corrupt");
    match std::fs::rename(path, &aside) {
        Ok(()) => tracing::error!(
            "could not parse {}: {why} — kept it at {} and starting from the default",
            path.display(),
            aside.display()
        ),
        Err(move_err) => tracing::error!(
            "could not parse {}: {why} — and could not move it aside ({move_err}), \
             so the next write will overwrite it",
            path.display()
        ),
    }
}

/// `<path>.<suffix>`, beside it.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write replaces the whole file and leaves no scratch beside it, and a
    /// file set aside keeps its bytes under the `.corrupt` name.
    #[test]
    fn a_write_replaces_whole_and_a_bad_file_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("orchd-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("nested").join("host.json");

        write(&file, b"{\"a\":1}").expect("first write, creating the directory");
        write(&file, b"{}").expect("second write");
        assert_eq!(std::fs::read(&file).unwrap(), b"{}");
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["host.json"], "scratch was left behind");

        std::fs::write(&file, "{ torn").unwrap();
        set_aside(&file, &"expected value");
        assert!(
            !file.exists(),
            "the bad file is still where a default will go"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("nested").join("host.json.corrupt")).unwrap(),
            "{ torn"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
