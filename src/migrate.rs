//! Fixing a `config.json` this build cannot read, before it is read.
//!
//! **Why a mechanism rather than a reader.** [`crate::config::Tracker`] accepts
//! the two names it once shipped, and that was not enough: a config the parser
//! refuses does not cost you a key, it costs the **whole file**.
//! `Config::existing` drops a config it cannot parse, the app reads that as first
//! run and offers a folder picker for a project configured months ago, and the
//! first-run write then merges by key and keeps the very line being refused. A
//! colleague on `"tracker": "none"` — which was the *default*, written back by
//! every save in the old settings pane — met exactly that.
//!
//! So the file is repaired on disk rather than tolerated in memory, and the next
//! setting to change shape has somewhere to go.
//!
//! **Shape-driven, and no schema version.** A version counter is state that has
//! to be maintained, migrated past and got right; a rule that recognises the
//! shape it fixes needs none, can run on every start forever, and is idempotent
//! by construction — which is also what makes it testable without a fixture of
//! old files. The cost is that a migration must be specific enough to recognise
//! its own input, which is the discipline worth having.
//!
//! **It never fails a start.** A missing file is nothing to do; a file that is not
//! JSON is left for the parser to report, because a migration guessing at broken
//! JSON is worse than the error message; an unwritable directory is a warning and
//! the reader's own tolerance carries the boot. That is why the `Tracker` reader
//! stays: this is the fix, and that is the fallback for the config we cannot write.

use std::path::Path;

use serde_json::{json, Map, Value};

/// One rule: a name for the log, and an edit to the file's top-level object.
///
/// `true` means it changed something, which is what decides whether the file is
/// written at all — a start that migrates nothing must not rewrite the file.
struct Migration {
    name: &'static str,
    apply: fn(&mut Map<String, Value>) -> bool,
}

/// Every rule, oldest first. Order matters only when two touch the same key.
const MIGRATIONS: &[Migration] = &[Migration {
    name: "tracker name to object",
    apply: tracker_name_to_object,
}];

/// The backup, written each time something is actually migrated: it is the file as
/// it stood immediately before this change, which is the copy worth having.
///
/// Its own name rather than `config.json.bak`, which the first-run page owns and
/// which would otherwise be overwritten by a boot that happened to migrate.
const BACKUP: &str = "config.json.premigrate";

/// Run every rule over `path`, writing it back only if one of them changed
/// something.
pub fn config_file(path: &Path) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return; // no config yet, or not ours to read
    };
    let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(&raw) else {
        // Not JSON, or not an object. `Config::parse` says so with a line and a
        // column; a migration cannot improve on that and must not guess.
        return;
    };

    let applied: Vec<&str> = MIGRATIONS
        .iter()
        .filter(|m| (m.apply)(&mut obj))
        .map(|m| m.name)
        .collect();
    if applied.is_empty() {
        return;
    }

    // Pretty and newline-terminated, like every other writer of this file. A
    // JSON round trip sorts the keys, which is why it only ever happens on the
    // one start that migrates: a file nothing applies to is not rewritten.
    let Ok(text) = serde_json::to_string_pretty(&Value::Object(obj)).map(|t| t + "\n") else {
        return;
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::write(dir.join(BACKUP), &raw) {
            // Said, and not fatal: a config dir we cannot write is a config we
            // cannot migrate, and the reader's own tolerance is what boots then.
            tracing::warn!("could not keep {BACKUP} beside the config: {e}");
        }
    }
    match std::fs::write(path, &text) {
        Ok(()) => tracing::info!(
            "migrated {}: {} (the previous file is {BACKUP})",
            path.display(),
            applied.join(", ")
        ),
        Err(e) => tracing::warn!("could not migrate {}: {e}", path.display()),
    }
}

/// `"tracker": "<name>"` becomes the object the name meant.
///
/// Three names ever existed, and the daemon wrote two of them itself: `"none"` was
/// the **default** and the old settings pane wrote every field back, so it is on
/// most machines. It is the one that still fails, because a tracker is three
/// fields now and no name can supply a per-site host.
///
/// A name that never shipped is left exactly as it is. `Tracker`'s own refusal
/// names the object to write, and that message is right for somebody guessing
/// today — a migration inventing a host for `"jira"` would be a wrong answer
/// written to disk.
fn tracker_name_to_object(obj: &mut Map<String, Value>) -> bool {
    let Some(Value::String(name)) = obj.get("tracker") else {
        return false;
    };
    match name.as_str() {
        // No tracker is the absence of the key, which is what `Option` reads.
        "none" => {
            obj.remove("tracker");
            true
        }
        // The fixture's stub differed from the real thing in one field.
        name @ ("shortcut" | "stub") => {
            let stub = name == "stub";
            let mut t = json!({
                "mcp_server": "shortcut",
                "host": "app.shortcut.com",
                "token_env": "SHORTCUT_API_TOKEN",
            });
            if stub {
                t["stub"] = json!(true);
            }
            obj.insert("tracker".to_string(), t);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file is rewritten only when a rule applies, the rest of it survives, and
    /// running twice changes nothing the first run did not.
    #[test]
    fn a_tracker_name_becomes_the_object_and_the_rest_of_the_file_is_kept() {
        let dir = crate::testutil::scratch("migrate");
        let path = dir.join("config.json");
        let write = |s: &str| std::fs::write(&path, s).unwrap();
        let read = || std::fs::read_to_string(&path).unwrap();
        let obj = || {
            serde_json::from_str::<Value>(&read())
                .unwrap()
                .as_object()
                .unwrap()
                .clone()
        };

        // The one that still fails today, and the one most machines have: written
        // by every save in the old settings pane, because it was the default.
        write(r#"{"main_checkout":"/repo","port":9001,"tracker":"none"}"#);
        config_file(&path);
        assert!(
            !obj().contains_key("tracker"),
            "no tracker is the absence of the key"
        );
        assert_eq!(obj()["port"], 9001, "the rest of the file has to survive");
        assert!(dir.join(BACKUP).exists(), "the previous file is kept");

        // Idempotent: nothing applies now, so nothing is written. Compared by
        // *content*, since a second rewrite would be invisible in a timestamp.
        let after = read();
        config_file(&path);
        assert_eq!(
            read(),
            after,
            "a start that migrates nothing rewrites nothing"
        );

        write(r#"{"main_checkout":"/repo","tracker":"shortcut"}"#);
        config_file(&path);
        let t = obj()["tracker"].clone();
        assert_eq!(t["mcp_server"], "shortcut");
        assert_eq!(t["host"], "app.shortcut.com");
        assert_eq!(t["token_env"], "SHORTCUT_API_TOKEN");
        assert!(t.get("stub").is_none(), "the real server is not the stub");
        // And the result is what the parser wants, which is the whole point.
        assert!(
            crate::config::Config::parse(&read()).is_ok(),
            "a migrated file has to parse: {}",
            read()
        );

        write(r#"{"main_checkout":"/repo","tracker":"stub"}"#);
        config_file(&path);
        assert_eq!(obj()["tracker"]["stub"], true);

        // A name that never shipped is left alone: `Tracker`'s refusal names the
        // object to write, and inventing a host here would write a wrong answer
        // to disk.
        write(r#"{"main_checkout":"/repo","tracker":"jira"}"#);
        let before = read();
        config_file(&path);
        assert_eq!(read(), before);

        // An object is already right, so it is not touched either.
        write(r#"{"main_checkout":"/repo","tracker":{"mcp_server":"linear","host":"linear.app"}}"#);
        let before = read();
        config_file(&path);
        assert_eq!(read(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Everything that is not a config to migrate, and none of it is fatal.
    #[test]
    fn a_missing_or_broken_config_is_left_for_the_parser_to_report() {
        let dir = crate::testutil::scratch("migrate-bad");
        let path = dir.join("config.json");

        // Missing: nothing to do, and nothing created — a migration must not be
        // how a config file comes into existence.
        config_file(&path);
        assert!(!path.exists());

        // Not JSON. `Config::parse` reports a line and a column; a rule guessing
        // at that is worse than the message.
        std::fs::write(&path, "{ not json").unwrap();
        config_file(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        assert!(
            !dir.join(BACKUP).exists(),
            "nothing changed, so nothing was backed up"
        );

        // JSON, but not an object.
        std::fs::write(&path, "[1,2,3]").unwrap();
        config_file(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1,2,3]");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
