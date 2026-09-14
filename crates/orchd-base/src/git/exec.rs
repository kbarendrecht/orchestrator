use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// A git run this slow is worth a line of its own.
///
/// Every helper here is an exec, and the daemon does dozens of them per start
/// and per reconcile. On this machine each costs a couple of milliseconds and
/// nobody notices; the reports that produced [`crate::timing`] are from machines
/// where the same call is an order of magnitude dearer. So the threshold is set
/// where a *single* call is already the story rather than one of eighty, and the
/// count of the eighty is the boot line's job instead.
pub(super) const SLOW_GIT: std::time::Duration = std::time::Duration::from_millis(300);

/// Record what an exec cost, and say so when it was slow.
///
/// **One copy, because there are two runners.** `run` spawns git itself and
/// `git_net` goes through `proc::run_bounded_with_input`, and both have to count
/// — the boot figure is the sum of them. The block was pasted into each, so a
/// change to the threshold or the wording could land in one and not the other.
fn recorded(began: std::time::Instant, cwd: &Path, args: &[&str]) {
    let took = began.elapsed();
    crate::timing::record_exec(took);
    if took >= SLOW_GIT {
        tracing::info!(
            "slow git: {}ms for `git {}` in {}",
            took.as_millis(),
            args.join(" "),
            cwd.display()
        );
    }
}

/// Run git, and record what the exec cost.
///
/// The three helpers below are what every *read* goes through, so the count
/// covers the start and the reconcile, which is the whole of what a slow start
/// is made of. The write paths (`rebase_onto`, `push_with_lease`, the commit
/// helpers) still spawn for themselves and are deliberately left alone: they are
/// one exec each on a path a person has just asked for, so counting them would
/// only mix a deliberate wait into the boot figure.
pub(super) fn run(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    /* **"Is this on a tokio worker" cannot be asked here, and it was worth finding
    out why.** Every git read funnels through this function, so it looks like the
    one place a `debug_assert` could turn `proc::run_blocking`'s convention into
    a check. It cannot: `Handle::try_current()` succeeds on a *blocking-pool*
    thread as well as on a worker, because the runtime handle stays in scope
    across `spawn_blocking`. Asserting on it failed 36 tests, and every one was
    correctly wrapped code — `reconcile` and `worktree::preflight` inside their
    own `spawn_blocking`. Tokio exposes nothing that separates the two, so the
    rule stays a convention and the reviewer stays the enforcement. */
    let began = std::time::Instant::now();
    let out = Command::new("git").args(args).current_dir(cwd).output();
    recorded(began, cwd, args);
    out
}

/// Shell out to `git` rather than a library binding — you need fsmonitor and
/// the real worktree/remote semantics (§1).
pub fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = run(cwd, args).with_context(|| format!("running git {}", args.join(" ")))?;
    Ok(String::from_utf8_lossy(&checked(cwd, args, out)?).into_owned())
}

/// Like [`git`] but returns the raw bytes, for `-z` output that is not valid
/// UTF-8 in the general case.
pub(super) fn git_raw(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = run(cwd, args).with_context(|| format!("running git {}", args.join(" ")))?;
    checked(cwd, args, out)
}

/// A finished git command's stdout, or its stderr as the error. The one place the
/// failure is phrased, so every runner refuses in the same words.
pub(super) fn checked(cwd: &Path, args: &[&str], out: std::process::Output) -> Result<Vec<u8>> {
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// How long a git command that touches the network may take.
///
/// Generous, because a fetch on a large repo over a slow link is legitimately
/// slow, and this is a backstop against *hanging*, not a performance budget.
pub(super) const NET_TIMEOUT_SECS: u64 = 120;

/// Run a git command that reaches the network: bounded, and unable to prompt.
///
/// **`Command::output()` nulls stdin, and that is not enough.** git and ssh ask for
/// credentials on `/dev/tty`, not on stdin, so a fetch against an https remote with
/// no credential helper — or an ssh remote whose host key is not yet known — sits
/// there forever waiting for an answer nobody can give. The desktop app only
/// escapes it by having no tty at all; `orchd` from a terminal does not, and the
/// boot path fetches before the window opens.
///
/// So three things, and each closes one door:
/// * `GIT_TERMINAL_PROMPT=0` — git itself must fail rather than ask.
/// * `GIT_SSH_COMMAND` with `BatchMode=yes` and `StrictHostKeyChecking=accept-new`
///   — ssh must not ask for a passphrase or about a new host key. `accept-new`
///   rather than `no`: an unknown host is recorded, a *changed* one still refuses.
/// * `GIT_ASKPASS` and `SSH_ASKPASS` emptied, or a configured graphical prompt
///   would be spawned in place of the terminal one and hang just as well.
///
/// And a deadline on top, because "cannot prompt" is not the same as "cannot
/// hang": a half-open TCP connection to a dead host does neither.
pub(super) fn git_net(cwd: &Path, args: &[&str], label: &str) -> Result<std::process::Output> {
    let argv: Vec<String> = std::iter::once("git".to_string())
        .chain(args.iter().map(|a| (*a).to_string()))
        .collect();
    let envs = net_env(cwd);
    let began = std::time::Instant::now();
    let out =
        crate::proc::run_bounded_with_input(cwd, NET_TIMEOUT_SECS, &argv, label, None, &envs, None);
    recorded(began, cwd, args);
    out
}

/// The environment that stops git and ssh asking a question nobody can answer.
///
/// Its own function so it can be asserted on: dropping one of these is invisible
/// until a fetch hangs on somebody's machine, which is the least reproducible bug
/// there is.
///
/// **The ssh command is yours with two options appended, not a fixed `ssh`.**
/// `GIT_SSH_COMMAND` outranks `core.sshCommand`, so a fixed value threw away a
/// multi-identity setup — `core.sshCommand = ssh -i ~/.ssh/id_work`, a 1Password
/// or YubiKey wrapper — and every daemon-side fetch and push then failed with
/// "Permission denied (publickey)" on a machine where `git fetch` at a prompt
/// worked. The base is the inherited variable, else the repo's own config, else
/// plain `ssh`; the two options go on the end either way.
pub(super) fn net_env(cwd: &Path) -> Vec<(String, String)> {
    let base = std::env::var("GIT_SSH_COMMAND")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| configured_ssh_command(cwd).clone())
        .unwrap_or_else(|| "ssh".to_string());
    vec![
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        (
            "GIT_SSH_COMMAND".to_string(),
            format!("{base} -oBatchMode=yes -oStrictHostKeyChecking=accept-new"),
        ),
        ("GIT_ASKPASS".to_string(), String::new()),
        ("SSH_ASKPASS".to_string(), String::new()),
    ]
}

/// The repo's `core.sshCommand`, read once for the life of the process.
///
/// **Cached because [`net_env`] runs before every network git call**, and reading
/// it per call put a whole extra `git` exec in front of each one: the boot fetch
/// that the window waits on, every poller tick, every `freshen_base` on a
/// daemon-cut worktree, every push. A start here was measured at 447 child
/// processes, so an exec that answers the same thing every time is exactly the
/// kind this daemon cannot afford to repeat.
///
/// Repo-static is what makes the cache honest: every worktree shares the main
/// checkout's config, so the answer does not depend on `cwd`. The inherited
/// `GIT_SSH_COMMAND` is deliberately *not* cached with it — that one is read per
/// call in [`net_env`], because it outranks this and a caller may set it.
pub(super) fn configured_ssh_command(cwd: &Path) -> &'static Option<String> {
    static SSH_COMMAND: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    SSH_COMMAND.get_or_init(|| read_ssh_command(cwd))
}

/// The uncached read, split out so a test can ask twice about two configs without
/// the process-wide cache answering for the first one.
pub(super) fn read_ssh_command(cwd: &Path) -> Option<String> {
    run(cwd, &["config", "--get", "core.sshCommand"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// [`git_net`], failing on a non-zero exit the way [`git`] does.
pub(super) fn git_net_ok(cwd: &Path, args: &[&str], label: &str) -> Result<String> {
    let out = git_net(cwd, args, label)?;
    Ok(String::from_utf8_lossy(&checked(cwd, args, out)?).into_owned())
}

/// Whether a git command succeeded, for probes where failure is a valid answer.
pub(super) fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    run(cwd, args).map(|o| o.status.success()).unwrap_or(false)
}
