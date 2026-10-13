//! What a Codex session is launched with: the daemon's hooks, the trust that lets
//! them run, and the instructions that name the daemon's skills.
//!
//! **All of it per process, as `-c` overrides, and nothing in `~/.codex`.** The
//! same rule as Claude Code's `--settings` and `--plugin-dir`: the daemon's hooks
//! are the daemon's, a session it did not start must not run them, and a user's
//! `config.toml` is not the daemon's to edit.
//!
//! # Trust is the trap
//!
//! Codex runs a hook only once its definition is trusted, and a hook it does not
//! trust is skipped **without a word in the pane**: the session works and the rail
//! never moves. Trust is a hash of the handler, recorded under a key in
//! `hooks.state`. Measured on 0.162.1:
//!
//! * The hashes are Codex's to compute. `codex app-server`'s `hooks/list` answers
//!   each hook's `key` and `currentHash` for the same `-c` overrides, which is how
//!   [`flags`] learns them, once per binary.
//! * Handed back as `-c hooks.state={"<key>"={trusted_hash="…"}}` they are trusted
//!   for that process, in `codex exec` and in the TUI alike. **It has to be one
//!   inline table**: the key holds a `.` (`config.toml:stop:0:0`), and the dotted
//!   form `-c 'hooks.state."<key>".trusted_hash=…'` is split at it and trusts
//!   nothing.
//! * `--dangerously-bypass-hook-trust` would also do it, and would also run every
//!   hook the *repo* declares, untrusted. That is the thing trust is for.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::config::Config;

/// Where Codex keeps its state: `$CODEX_HOME`, else `~/.codex`.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
}

/// The conversation id in a rollout's file name, when `path` is one.
///
/// `rollout-<timestamp>-<uuid>.jsonl`, under `sessions/YYYY/MM/DD/`. How the
/// transcript readers tell a Codex conversation from a Claude Code one without
/// being told: the recorded path is the only thing every caller has.
pub fn rollout_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    // A uuid is the last 36 characters; the timestamp before it has dashes too.
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    uuid::Uuid::parse_str(id).ok()?;
    Some(id.to_string())
}

/// The name Codex gave the conversation, which is what `codex resume` lists it
/// under.
///
/// `session_index.jsonl` holds one line per naming, appended, so **the last line
/// for an id wins**, the same rule as Claude Code's `ai-title`. Codex writes it
/// once the first turn is done, so `None` before then is ordinary.
pub fn thread_name(id: &str) -> Option<String> {
    /* **Parsed once per version of the file, not once per call.** The index only
    grows, and this is asked for every restored Codex record at boot and every
    newly listed outside conversation. Keyed by mtime and length, since an append
    can land inside one mtime tick. */
    type Index = (std::time::SystemTime, u64, HashMap<String, String>);
    static INDEX: Mutex<Option<Index>> = Mutex::new(None);
    let path = home()?.join("session_index.jsonl");
    let meta = std::fs::metadata(&path).ok()?;
    let key = (meta.modified().ok()?, meta.len());
    let mut cached = INDEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cached
        .as_ref()
        .is_none_or(|(at, len, _)| (*at, *len) != key)
    {
        let text = std::fs::read_to_string(&path).ok()?;
        *cached = Some((key.0, key.1, thread_names(&text)));
    }
    cached.as_ref()?.2.get(id).cloned()
}

/// Every id's name, the last line for an id winning.
fn thread_names(index: &str) -> HashMap<String, String> {
    index
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let id = v.get("id")?.as_str()?.to_string();
            let name = v.get("thread_name")?.as_str()?.trim();
            (!name.is_empty()).then(|| (id, name.to_string()))
        })
        .collect()
}

/// The `-c` values for the daemon's hooks, written at every start beside
/// `hooks.json`, read at every spawn.
pub fn hooks_path() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("codex-hooks.json"))
}

/// Write the hook overrides. `overrides` are complete `-c` values, each
/// `hooks.<Event>=[…]`.
pub fn write_hooks(overrides: &[String]) -> Result<PathBuf> {
    let path = hooks_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(overrides)?)?;
    Ok(path)
}

/// A TOML basic string. JSON's escapes are a subset of TOML's for everything
/// `serde_json` emits: it writes non-ASCII as itself, never as a surrogate pair.
pub fn toml_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// One matcher group of a hook: the tool it matches, if any, and each handler's
/// command line with its timeout in seconds.
pub type HookGroup<'a> = (Option<&'a str>, Vec<(String, u32)>);

/// One hook entry as a `-c` value: `hooks.<event>=[{matcher=…,hooks=[…]}]`.
pub fn hook_override(event: &str, groups: &[HookGroup<'_>]) -> String {
    let groups: Vec<String> = groups
        .iter()
        .map(|(matcher, hooks)| {
            let hooks: Vec<String> = hooks
                .iter()
                .map(|(command, timeout)| {
                    format!(
                        "{{type=\"command\",command={},timeout={timeout}}}",
                        toml_str(command)
                    )
                })
                .collect();
            let matcher = matcher
                .map(|m| format!("matcher={},", toml_str(m)))
                .unwrap_or_default();
            format!("{{{matcher}hooks=[{}]}}", hooks.join(","))
        })
        .collect();
    format!("hooks.{event}=[{}]", groups.join(","))
}

/// The argv every spawned `codex` carries: the daemon's hooks, their trust, and
/// the skills.
///
/// `cwd`, `env` and `unset` are the session's own, because the `codex` that
/// answers for the hashes must be the one the pty will run. A checkout pins its
/// own through mise, and it is a node script that needs that checkout's `node`.
///
/// **Fails the spawn when the hooks cannot run**, which it used not to. A Codex
/// session without them is not a degraded session but a wrong one: no
/// `SessionEnd` and no `Stop`, so the row reads `Ready` through every turn, and
/// the exit watcher forgets a session it never saw take a turn — its row and its
/// worktree with it. Two ways to get there, both refused with what to fix: no
/// `orch` beside the daemon (the hooks are `orch hook <route>`, so none are
/// written), and a Codex that would not say the hashes it trusts, without which
/// it skips every hook in silence.
pub fn flags(cwd: &Path, env: &[(String, String)], unset: &[&str]) -> Result<Vec<String>> {
    let overrides = hook_overrides(hooks_path().and_then(|p| {
        std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))
    }))?;
    // **The directory, named, on every start.** A resume or a fork in a directory
    // other than the one the conversation last ran in stops at a picker ("use
    // session directory" or "use current directory") whose default is the *old*
    // one, and the daemon starts conversations somewhere new all the time: a fork
    // cuts a worktree, a swap and a move resume elsewhere. Measured on 0.162.1, the
    // line the daemon typed next went into the picker, and the fork would have
    // worked in its parent's tree. `-C` answers the picker before it is asked.
    let mut v: Vec<String> = vec!["-C".to_string(), cwd.to_string_lossy().into_owned()];
    v.extend(overrides.iter().flat_map(|o| ["-c".to_string(), o.clone()]));
    // The hook overrides alone, past `-C`: `app-server` refuses `-C` and exits, and
    // the hashes are of the hooks, so nothing else belongs in the question.
    let state = trust(cwd, env, unset, &v[2..]).context(
        "Codex would not say which hooks it trusts, so it would skip the daemon's \
         and the rail could not follow the session; not starting it",
    )?;
    v.push("-c".to_string());
    v.push(format!("hooks.state={state}"));
    if let Some(text) = crate::skills::codex_instructions() {
        v.push("-c".to_string());
        v.push(format!("developer_instructions={}", toml_str(&text)));
    }
    Ok(v)
}

/// The hook overrides out of the file the daemon wrote at start, or why there are
/// none. An empty list is the daemon's own way of saying it found no `orch`.
fn hook_overrides(written: Result<String>) -> Result<Vec<String>> {
    let overrides: Vec<String> = serde_json::from_str(
        &written.context("the daemon's Codex hooks were never written; not starting it")?,
    )
    .context("the daemon's Codex hooks are unreadable; not starting it")?;
    if overrides.is_empty() {
        bail!(
            "Codex cannot be followed without its hooks, and they run `orch`, which is not \
             beside this orchd; install or build `orch` next to it and restart"
        );
    }
    Ok(overrides)
}

/// The `hooks.state` inline table that trusts exactly the daemon's hooks.
///
/// Cached per binary and per set of hooks: the hash is of the definition, so it
/// changes only when one of those does, and asking costs a process start.
fn trust(cwd: &Path, env: &[(String, String)], unset: &[&str], hooks: &[String]) -> Result<String> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let exe = orchd_base::pty::which("codex", cwd, env, unset)?;
    let stamp = std::fs::metadata(&exe)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{}\0{stamp}\0{}", exe.display(), hooks.join("\0"));
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&key).cloned()) {
        return Ok(hit);
    }
    let listed = list_hooks(&exe, cwd, env, unset, hooks)?;
    let state = state_table(&listed);
    if let Ok(mut c) = cache.lock() {
        c.insert(key, state.clone());
    }
    Ok(state)
}

/// `(key, currentHash)` for each hook the overrides declare, from `hooks/list`.
///
/// The server is kept alive until it answers: it exits on end of input before an
/// asynchronous request is served, so closing stdin after writing would get the
/// `initialize` answer and nothing else.
fn list_hooks(
    exe: &Path,
    cwd: &Path,
    env: &[(String, String)],
    unset: &[&str],
    hooks: &[String],
) -> Result<Vec<(String, String)>> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("app-server")
        .args(hooks)
        .current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for k in unset {
        cmd.env_remove(k);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {} app-server", exe.display()))?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let cwd_json = serde_json::to_string(&cwd.to_string_lossy())?;
    let ask = format!(
        "{}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"orchd","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized"}"#,
        format_args!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"hooks/list","params":{{"cwds":[{cwd_json}]}}}}"#
        ),
    );
    stdin.write_all(ask.as_bytes())?;
    stdin.flush()?;

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v.get("id").and_then(|i| i.as_u64()) == Some(2) {
                let _ = tx.send(v);
                return;
            }
        }
    });
    let answer = rx.recv_timeout(Duration::from_secs(20));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let answer = answer.context("codex app-server did not answer hooks/list")?;
    if let Some(e) = answer.get("error") {
        bail!("hooks/list refused: {e}");
    }
    Ok(parse_listed(&answer))
}

/// The daemon's own hooks out of a `hooks/list` answer. Its keys start
/// `/<session-flags>/`; a repo's or a user's do not, and are not the daemon's to
/// trust.
fn parse_listed(answer: &serde_json::Value) -> Vec<(String, String)> {
    let data = answer
        .pointer("/result/data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    data.iter()
        .flat_map(|d| {
            d.get("hooks")
                .and_then(|h| h.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|h| {
            let key = h.get("key")?.as_str()?;
            let hash = h.get("currentHash")?.as_str()?;
            key.starts_with("/<session-flags>/")
                .then(|| (key.to_string(), hash.to_string()))
        })
        .collect()
}

fn state_table(listed: &[(String, String)]) -> String {
    let entries: Vec<String> = listed
        .iter()
        .map(|(k, h)| format!("{}={{trusted_hash={}}}", toml_str(k), toml_str(h)))
        .collect();
    format!("{{{}}}", entries.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_is_one_inline_value_codex_parses() {
        assert_eq!(
            hook_override("Stop", &[(None, vec![("orch hook stop".into(), 5)])]),
            r#"hooks.Stop=[{hooks=[{type="command",command="orch hook stop",timeout=5}]}]"#
        );
        assert_eq!(
            hook_override(
                "PreToolUse",
                &[
                    (None, vec![("a".into(), 5)]),
                    (Some("Bash"), vec![("'/x y/orch' guard push".into(), 5)]),
                ]
            ),
            r#"hooks.PreToolUse=[{hooks=[{type="command",command="a",timeout=5}]},{matcher="Bash",hooks=[{type="command",command="'/x y/orch' guard push",timeout=5}]}]"#
        );
    }

    #[test]
    fn only_the_daemons_hooks_are_trusted() {
        let answer = serde_json::json!({"id": 2, "result": {"data": [{"hooks": [
            {"key": "/<session-flags>/config.toml:stop:0:0", "currentHash": "sha256:aa"},
            {"key": "/repo/.codex/config.toml:stop:0:0", "currentHash": "sha256:bb"},
        ]}]}});
        let listed = parse_listed(&answer);
        assert_eq!(
            listed,
            [(
                "/<session-flags>/config.toml:stop:0:0".to_string(),
                "sha256:aa".to_string()
            )]
        );
        // The inline-table form, the one that works: the key's `.` stays quoted.
        assert_eq!(
            state_table(&listed),
            r#"{"/<session-flags>/config.toml:stop:0:0"={trusted_hash="sha256:aa"}}"#
        );
    }

    #[test]
    fn a_rollout_names_its_conversation() {
        let p = Path::new(
            "/h/.codex/sessions/2026/10/12/rollout-2026-10-12T15-06-29-01a13038-c5d8-7cd0-b9b8-7c877e5c58ff.jsonl",
        );
        assert_eq!(
            rollout_id(p).as_deref(),
            Some("01a13038-c5d8-7cd0-b9b8-7c877e5c58ff")
        );
        // A Claude Code transcript is `<uuid>.jsonl`, and is not one.
        assert_eq!(
            rollout_id(Path::new(
                "/h/.claude/projects/x/01a13038-c5d8-7cd0-b9b8-7c877e5c58ff.jsonl"
            )),
            None
        );
    }

    #[test]
    fn the_last_naming_wins() {
        let index = concat!(
            r#"{"id":"a","thread_name":"first","updated_at":"1"}"#,
            "\n",
            r#"{"id":"b","thread_name":"other","updated_at":"2"}"#,
            "\n",
            r#"{"id":"a","thread_name":"renamed","updated_at":"3"}"#,
            "\n",
        );
        assert_eq!(
            thread_names(index).get("a").map(String::as_str),
            Some("renamed")
        );
        assert_eq!(thread_names(index).get("c"), None);
    }

    /// No `orch` means the daemon wrote no hooks, and a Codex session with none
    /// is refused rather than started blind.
    #[test]
    fn a_codex_session_without_hooks_is_refused() {
        let said = |r: Result<Vec<String>>| format!("{:#}", r.unwrap_err());
        assert!(said(hook_overrides(Ok("[]".into()))).contains("`orch`"));
        assert!(said(hook_overrides(Err(anyhow::anyhow!("gone")))).contains("never written"));
        assert!(said(hook_overrides(Ok("{".into()))).contains("unreadable"));
        assert_eq!(
            hook_overrides(Ok(r#"["hooks.Stop=[]"]"#.into())).unwrap(),
            ["hooks.Stop=[]"]
        );
    }

    #[test]
    fn a_toml_string_survives_quotes_and_newlines() {
        assert_eq!(toml_str("a \"b\"\nc"), r#""a \"b\"\nc""#);
    }
}
