//! The blast-radius guard for `git push`, run as a `PreToolUse` hook (§8).
//!
//! Registered by [`crate::hooks::write_settings`] and executed by `orch guard
//! push`, which is why the rules live in the library rather than in the binary:
//! they are testable here, and the binary is a thin `stdin -> exit code`.
//!
//! **This is a mistake-catcher, not a control.** It sees `Bash` tool calls and
//! nothing else, so `gh`, an MCP git server, or a script the agent writes and
//! then runs all go around it. What it is for is the unattended case: a fix-pr
//! run force-pushes to a PR branch with nobody watching (`commands/fix-pr.md`),
//! and a lease-less force-push there is the one mistake with real blast radius.
//! Do not describe it as something that prevents an agent from pushing.
//!
//! It replaced a Python script that shipped alongside the binary. Three reasons,
//! and each was a live defect rather than a preference:
//!
//! - The hook ran the script by its shebang, so on a machine without `python3`
//!   it exited 127 — not 2 — and the guard silently stopped existing. That is
//!   the one failure mode the script's own docstring said it was written to
//!   avoid.
//! - Its protected-ref list was four hardcoded branch names. The daemon already
//!   knows the real base from `upstream_ref`, so the list could only ever be
//!   wrong for somebody.
//! - It matched refspecs by their spelling: `git push origin main` was refused
//!   while `git push origin HEAD:refs/heads/main` — the same push — was allowed,
//!   because it compared the text after the last colon against the list.
//!
//! The two rules it dropped (`-u`/`--set-upstream`, and pushing to `upstream`)
//! were both specific to a triangular fork workflow. They are correct there and
//! wrong everywhere else, and `README.md` says the defaults assume nothing about
//! the repo — so a guard that refuses the normal `push -u` of a single-remote
//! checkout is refusing the common case.

/// Everything the guard needs from a `PreToolUse` payload.
pub struct Call<'a> {
    pub tool_name: &'a str,
    pub command: &'a str,
    /// The branch the checkout is on, when it could be read. `None` disables the
    /// bare-`git push` rule rather than guessing.
    pub current_branch: Option<&'a str>,
}

/// The base branch to protect, as a plain name (`main`).
///
/// `None` means the daemon could not resolve one — an `origin/HEAD` symref that
/// has never been fetched — and the base rule is skipped rather than applied to
/// a guessed name.
pub type Base<'a> = Option<&'a str>;

/// `Some(reason)` to refuse the tool call, `None` to let it through.
pub fn check(call: &Call, base: Base) -> Option<String> {
    if call.tool_name != "Bash" {
        return None;
    }
    for segment in segments(call.command) {
        if let Some(reason) = check_one(&segment, base, call.current_branch) {
            return Some(reason);
        }
    }
    None
}

/// Split a shell command on the operators that start a new command.
///
/// `cargo test && git push --force` is two commands, and the guard has to see
/// the second one on its own. The Python version regex-matched the whole string,
/// which got this right by accident for `--force` and wrong for refspecs — it
/// would read the remote of one command and the branch of another.
///
/// Crude on purpose: quoting and subshells are not parsed. Erring toward *more*
/// segments only ever gives the rules more places to look, and a guard that is
/// wrong here fails open, which is what it already does for anything it cannot
/// read.
fn segments(command: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        let two = matches!(c, '&' | '|') && chars.peek() == Some(&c);
        if two {
            chars.next();
        }
        if two || matches!(c, ';' | '\n' | '|') {
            out.push(String::new());
        } else {
            out.last_mut().unwrap().push(c);
        }
    }
    out
}

/// The tokens of one command, with the `git` subcommand located.
///
/// Returns the arguments *after* `push`, or `None` when this segment is not a
/// git push. `git -C /repo push` and `/usr/bin/git push` both count: the first
/// because `-C` and `-c` are the two git options that take a value, and the
/// second because a path is still git.
///
/// `git` must be the segment's *first* token, or `echo git push --force` would be
/// refused for printing a string. That does mean `time git push --force` is not
/// seen; a guard that fails open on an unusual spelling is the trade this whole
/// module already makes.
fn push_args(segment: &str) -> Option<Vec<String>> {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first()?;
    if *first != "git" && !first.ends_with("/git") {
        return None;
    }
    let mut i = 1;
    while let Some(tok) = tokens.get(i) {
        if tok.starts_with('-') {
            // The only git options that consume the next token.
            if *tok == "-C" || *tok == "-c" {
                i += 1;
            }
            i += 1;
            continue;
        }
        if *tok != "push" {
            return None;
        }
        return Some(tokens[i + 1..].iter().map(|s| s.to_string()).collect());
    }
    None
}

fn check_one(segment: &str, base: Base, current_branch: Option<&str>) -> Option<String> {
    let args = push_args(segment)?;

    // `--force-with-lease` refuses when someone else has pushed since you last
    // fetched; plain `--force` does not. Checked before the destination, because
    // it is the rule that matters when nobody is watching.
    //
    // `--force-with-lease` starts with `--force`, so the exact forms are matched
    // rather than a prefix. `-f` only as a whole token: `-fu` is not spelled here
    // and clustering it would be a way around this, but a clustered short flag is
    // also not something an agent writes.
    if args.iter().any(|a| a == "--force" || a == "-f") {
        return Some(
            "orchd: plain `--force` is denied. Use `--force-with-lease`, which refuses \
             when someone else has pushed since you last fetched."
                .into(),
        );
    }

    let base = base?;
    let dsts = destinations(&args);
    for dst in &dsts {
        if dst == base {
            return Some(format!(
                "orchd: pushing to `{base}` is denied — it is the base branch this \
                 checkout is measured against. Open a PR instead."
            ));
        }
    }

    // A bare `git push` while standing on the base branch is the same push with
    // nothing written down, and the spelling-only version of this guard let it
    // through. Only reachable when the branch could actually be read.
    if dsts.is_empty() && current_branch == Some(base) {
        return Some(format!(
            "orchd: `git push` from `{base}` is denied — it is the base branch this \
             checkout is measured against. Open a PR instead."
        ));
    }
    None
}

/// The refs a push would write to, normalised to plain branch names.
///
/// The destination of `<src>:<dst>` is `dst`; a lone `<ref>` is its own
/// destination. `refs/heads/main` and `main` are the same ref and must compare
/// equal — reading only the text after the last colon is what let
/// `HEAD:refs/heads/main` past the old guard.
fn destinations(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen_remote = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        i += 1;
        if arg.starts_with('-') {
            // `--repo <name>` is the one push option taking a separate value that
            // could otherwise be read as a refspec.
            if arg == "--repo" {
                i += 1;
            }
            continue;
        }
        // The first bare word is the remote, not a ref.
        if !seen_remote {
            seen_remote = true;
            continue;
        }
        let spec = arg.trim_matches(|c| c == '\'' || c == '"');
        // A leading `+` is force-for-this-refspec.
        let spec = spec.strip_prefix('+').unwrap_or(spec);
        let dst = spec.split_once(':').map(|(_, d)| d).unwrap_or(spec);
        let dst = dst.trim_matches(|c| c == '\'' || c == '"');
        let dst = dst.strip_prefix("refs/heads/").unwrap_or(dst);
        if !dst.is_empty() {
            out.push(dst.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash<'a>(command: &'a str, branch: Option<&'a str>) -> Call<'a> {
        Call { tool_name: "Bash", command, current_branch: branch }
    }

    #[test]
    fn plain_force_is_refused_and_a_lease_is_not() {
        let denied = |c: &str| check(&bash(c, None), Some("main")).is_some();
        assert!(denied("git push --force"));
        assert!(denied("git push -f origin topic"));
        assert!(denied("git push origin topic --force"));
        // The rule this guard exists for: the lease is the allowed spelling, and
        // `--force-with-lease` starts with `--force`, so a prefix match would
        // refuse the very thing it is steering people toward.
        assert!(!denied("git push --force-with-lease"));
        assert!(!denied("git push --force-with-lease=topic:abc123"));
    }

    #[test]
    fn a_push_to_the_base_branch_is_refused_however_it_is_spelled() {
        let denied = |c: &str| check(&bash(c, None), Some("main")).is_some();
        assert!(denied("git push origin main"));
        // The bypass in the script this replaced: it compared the text after the
        // last colon, so the fully-qualified form named `refs/heads/main` and
        // matched nothing.
        assert!(denied("git push origin HEAD:refs/heads/main"));
        assert!(denied("git push origin HEAD:main"));
        assert!(denied("git push origin +main"));
        assert!(denied("git push origin 'main'"));
        assert!(!denied("git push origin topic"));
        assert!(!denied("git push origin HEAD:topic"));
    }

    #[test]
    fn the_base_branch_comes_from_config_and_no_name_is_privileged() {
        // `develop`, `master` and `release` were hardcoded alongside `main` in the
        // script this replaced. Only the configured base is protected now, so a
        // repo whose base is `trunk` is guarded and one whose base is `main` does
        // not also refuse pushes to an ordinary branch called `develop`.
        assert!(check(&bash("git push origin trunk", None), Some("trunk")).is_some());
        assert!(check(&bash("git push origin develop", None), Some("main")).is_none());
        // No resolvable base: the force rule still applies, the ref rule cannot.
        assert!(check(&bash("git push origin main", None), None).is_none());
        assert!(check(&bash("git push --force", None), None).is_some());
    }

    #[test]
    fn a_bare_push_is_judged_by_the_branch_it_stands_on() {
        assert!(check(&bash("git push", Some("main")), Some("main")).is_some());
        assert!(check(&bash("git push", Some("topic")), Some("main")).is_none());
        // Unknown branch is not a guess: the rule is skipped.
        assert!(check(&bash("git push", None), Some("main")).is_none());
    }

    #[test]
    fn a_push_hidden_behind_another_command_is_still_seen() {
        let denied = |c: &str| check(&bash(c, None), Some("main")).is_some();
        assert!(denied("cargo test && git push --force"));
        assert!(denied("cd /repo; git push origin main"));
        assert!(denied("git status | grep x || git push -f"));
        // And the reverse: the segments must not be read as one command, or the
        // remote of the first would pair with the ref of the second.
        assert!(!denied("git push origin topic && echo main"));
    }

    #[test]
    fn git_reached_by_path_or_with_leading_options_still_counts() {
        let denied = |c: &str| check(&bash(c, None), Some("main")).is_some();
        assert!(denied("/usr/bin/git push --force"));
        assert!(denied("git -C /repo push origin main"));
        assert!(denied("git -c user.name=x push --force"));
        // `-C` consumes its value, so the path must not be read as the subcommand.
        assert!(!denied("git -C /repo status"));
    }

    #[test]
    fn everything_that_is_not_a_push_is_left_alone() {
        let denied = |c: &str| check(&bash(c, None), Some("main")).is_some();
        assert!(!denied("git commit -m 'main'"));
        assert!(!denied("git fetch origin main"));
        assert!(!denied("echo git push --force"));
        assert!(!denied("git pull --force"));
        // Another tool entirely is not this guard's business.
        let call = Call { tool_name: "Edit", command: "git push --force", current_branch: None };
        assert!(check(&call, Some("main")).is_none());
    }
}
