//! The login shell's PATH, adopted before anything else starts.
//!
//! Split out of `main.rs` for readability only; `main` still calls
//! [`adopt_login_path`] first, and the constraint that makes that ordering
//! matter (`set_var` before any thread exists) is documented on it.

/// Marker written around the PATH the login shell prints, so an rc file that
/// greets you does not become part of it.
const PATH_MARK: &str = "__ORCHD_PATH__";

/// Set once the PATH has been adopted, so a self-restart does not pay for it
/// again.
const ADOPTED: &str = "ORCHD_ADOPTED_LOGIN_PATH";

/// Give the daemon the PATH the user's shell would have given it.
///
/// **An app started from a launcher does not inherit your shell's environment.**
/// On macOS LaunchServices hands over `/usr/bin:/bin:/usr/sbin:/sbin`, and on
/// Linux a desktop entry gets the systemd user manager's. Neither holds Homebrew,
/// mise or `~/.local/bin`, so `gh`, `node` and `claude` are absent — and nothing
/// says "PATH" anywhere the user is looking. What they see is a PR pane reporting
/// no credential, a review queue reading unavailable, and sessions that die on
/// spawn. Measured on a colleague's Mac, from the `.app` this project now writes
/// itself, which is why it is worth doing rather than documenting.
///
/// Skipped when started from a terminal, because then the environment already is
/// the shell's. How the shell is asked is [`ask_login_path`]'s business.
///
/// Never fatal, and never destructive: the entries already present are kept, and a
/// shell that cannot be run leaves the process exactly as it was.
///
/// Called from the top of `main`, and it has to stay there: `set_var` is
/// process-global and unsound beside other threads, so this runs before the
/// runtime, the daemon and every pty exist.
pub(crate) fn adopt_login_path() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() || std::env::var_os(ADOPTED).is_some() {
        return;
    }
    /* **The cache is here because this is on the critical path of the window.**
       A `.zshrc` that activates a tool manager costs one to three seconds, and
       the window cannot open until it answers. So a remembered answer is used
       when there is one, and the shell is asked again *afterwards* only to
       rewrite the file for next time.

       Why the refresh cannot apply itself: `set_var` is process-global and
       unsound beside other threads, which is why this whole function runs before
       the runtime, the daemon and every pty exist. A background refresh that
       called it would be exactly the thing that ordering exists to prevent. So
       the refresh writes the file and nothing else, and a changed rc file takes
       effect on the *next* launch. One launch of lag on a file most people edit
       once a year, against seconds off every start. */
    if let Some(cached) = cached_login_path() {
        apply_login_path(&cached);
        tracing::info!("adopted the remembered login PATH; refreshing it for next time");
        // Not a tokio task: there is no runtime yet, and it must not become one
        // — see above. A bare thread, detached, doing file IO and no more.
        std::thread::spawn(|| {
            if let Some(fresh) = ask_login_path() {
                write_cached_login_path(&fresh);
            }
        });
        return;
    }
    // First launch on this machine, or the cache was cleared. Pay for it once.
    let Some(theirs) = ask_login_path() else {
        return;
    };
    apply_login_path(&theirs);
    write_cached_login_path(&theirs);
    tracing::info!("adopted the login shell's PATH");
}

/// Put a PATH into this process, keeping what it already had.
fn apply_login_path(theirs: &str) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let merged = merge_paths(theirs, &current);
    std::env::set_var("PATH", &merged);
    std::env::set_var(ADOPTED, "1");
}

/// Where the remembered PATH lives. Beside the config, so `ORCHD_CONFIG_DIR`
/// moves it with everything else durable.
fn login_path_cache() -> Option<std::path::PathBuf> {
    orchd::config::Config::config_dir()
        .ok()
        .map(|d| d.join("login-path"))
}

/// The remembered PATH, if it still looks like one.
///
/// Sanity-checked rather than trusted: a truncated or hand-edited file would
/// otherwise put junk in front of every lookup the daemon makes, and the failure
/// would be "`claude` not found" with no hint where it came from.
fn cached_login_path() -> Option<String> {
    let raw = std::fs::read_to_string(login_path_cache()?).ok()?;
    usable_path(&raw)
}

/// Does this file's contents look like a PATH worth adopting?
///
/// Its own function so the rule can be tested without a config dir. A truncated
/// or hand-edited cache would otherwise go in front of every lookup the daemon
/// makes, and the symptom would be "`claude` not found" with nothing pointing at
/// a file nobody remembers writing.
fn usable_path(raw: &str) -> Option<String> {
    let path = raw.trim();
    (!path.is_empty() && path.contains('/')).then(|| path.to_string())
}

fn write_cached_login_path(path: &str) {
    let Some(file) = login_path_cache() else {
        return;
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&file, format!("{path}\n")) {
        tracing::debug!("could not remember the login PATH: {e}");
    }
}

/// Ask the user's shell what PATH it would have given us.
///
/// `-lic` rather than `-lc`: a login shell reads `.zprofile`, but most people set
/// PATH in `.zshrc`, which only an *interactive* shell reads.
fn ask_login_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let argv = [
        shell,
        "-lic".into(),
        format!("printf '{PATH_MARK}%s{PATH_MARK}' \"$PATH\""),
    ];
    // Bounded, because an rc file that waits for something would otherwise hold
    // the window shut. Five seconds is a slow shell, not a hung one.
    let out = match orchd::proc::run_bounded(std::path::Path::new(&home), 5, &argv, "login shell") {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("could not ask the login shell for PATH: {e:#}");
            return None;
        }
    };
    let said = String::from_utf8_lossy(&out.stdout);
    match path_between_marks(&said) {
        Some(p) => Some(p.to_string()),
        None => {
            tracing::warn!("the login shell printed no PATH; keeping the one we were given");
            None
        }
    }
}

/// The PATH between the two markers, if the shell printed both.
fn path_between_marks(said: &str) -> Option<&str> {
    let (_, rest) = said.split_once(PATH_MARK)?;
    let (path, _) = rest.split_once(PATH_MARK)?;
    (!path.trim().is_empty()).then_some(path)
}

/// The shell's PATH first, then anything we already had that it did not mention.
///
/// Order matters and theirs wins: a tool manager puts its shims in front on
/// purpose, and taking the version the user gets in a terminal is the whole point.
/// Nothing is dropped, because the inherited PATH is what the packaged install
/// relies on to find its own `orch`.
fn merge_paths(theirs: &str, ours: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut merged: Vec<std::path::PathBuf> = std::env::split_paths(theirs).collect();
    for p in std::env::split_paths(ours) {
        if !merged.contains(&p) {
            merged.push(p);
        }
    }
    std::env::join_paths(merged).unwrap_or_else(|_| ours.to_os_string())
}


#[cfg(test)]
mod tests {
    use super::*;

    /// A remembered PATH goes in front of every binary the daemon looks up, so
    /// junk in that file has to answer "no" rather than "maybe".
    #[test]
    fn a_remembered_path_is_checked_before_it_is_trusted() {
        assert_eq!(
            usable_path("/opt/homebrew/bin:/usr/bin\n").as_deref(),
            Some("/opt/homebrew/bin:/usr/bin"),
            "the trailing newline the writer adds is not part of the value"
        );
        // The shapes a half-written or hand-edited file actually takes.
        assert!(usable_path("").is_none());
        assert!(usable_path("   \n").is_none(), "whitespace is empty");
        assert!(usable_path("no-slashes-here").is_none(), "that is not a path list");
    }

    #[test]
    fn the_shell_path_is_read_between_the_marks_and_greetings_are_not() {
        let said = format!("Welcome back!\n{PATH_MARK}/opt/homebrew/bin:/usr/bin{PATH_MARK}");
        assert_eq!(path_between_marks(&said), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(path_between_marks("no markers here"), None, "a shell that failed says nothing");
        assert_eq!(
            path_between_marks(&format!("{PATH_MARK}{PATH_MARK}")),
            None,
            "an empty PATH is a shell that did not answer, not an answer"
        );
    }

    /// The shell's order is the answer, and the inherited entries still have to
    /// survive: a packaged install finds its own `orch` through one of them.
    #[test]
    fn the_shell_path_wins_and_nothing_inherited_is_dropped() {
        let merged = merge_paths(
            "/opt/homebrew/bin:/usr/bin",
            std::ffi::OsStr::new("/usr/bin:/Applications/Orchestrator.app/Contents/MacOS"),
        );
        assert_eq!(
            merged.to_string_lossy(),
            "/opt/homebrew/bin:/usr/bin:/Applications/Orchestrator.app/Contents/MacOS"
        );
    }
}
