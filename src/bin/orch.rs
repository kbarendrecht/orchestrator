//! `orch` — what a running session can ask the daemon for.
//!
//! Shipped in the same tarball as the app and installed by the same mise tool, so
//! an agent finds it on `PATH` and the monorepo it is working in stays clean: no
//! script, no `.claude/` entry, nothing to keep in step with this repo.
//!
//! It needs no configuration. A session's environment already says where the
//! daemon is (`ORCH_URL`), who the session is (`ORCH_SESSION_ID`) and what it is
//! allowed to do (`ORCH_ASK_TOKEN`). That token opens asking, spawning, starting a
//! declared process, undoing your own spawns and tearing down a clean worktree,
//! and nothing else: this is not a remote control for the daemon, it is what an
//! agent legitimately needs.
//!
//! It replaces a page of `curl | jq` in `commands/resolve-run.md`, where the
//! long-poll loop was written out by hand and easy to get wrong.
// A command-line binary: printing *is* its output, and `print_stdout` is denied
// across the workspace so the daemon library cannot quietly grow a `println!`
// that no log ever sees.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use serde_json::{json, Value};
use std::process::ExitCode;

const USAGE: &str = "\
orch — talk to the orchestrator you are running inside

  orch new [--worktree [<name>] | --workspace <name>] [--prompt <text>]
      Start another session. Prints its id, workspace and path.

  orch kill <id>
      Undo one of your own spawns: end it, drop its row, and remove the
      worktree if that spawn cut one.

  orch teardown <workspace>
      Remove a worktree nobody is working in, through the same checks as the
      app's button. Refuses one holding a live session or unpushed work.

  orch ls [--state <state>] [--all]
      The sessions the daemon knows about, one per line. Archived ones are
      hidden unless you ask.

  orch ask --question <text> --option <value>:<label> ...
      Ask the human something and block until they answer.

  orch run <name>
      Start one of the processes this workspace declares.

  orch outside <path>
      Ask to run git in that folder. Remembered for it, for this session.

  orch guard push [--base <branch>] [--main <path>]
      Not for you to call — the daemon registers this as a PreToolUse hook.

`orch <command> --help` for the flags each one takes.

Environment (set for you): ORCH_URL, ORCH_SESSION_ID, ORCH_ASK_TOKEN
";

const HELP_NEW: &str = "\
orch new [--worktree [<name>] | --workspace <name>] [--prompt <text>]

Start another session, and print `<id>  <workspace>  <path>` — where it landed as
well as what it is, so confirming the spawn went where you meant takes no second
command.

  --worktree [<name>]  Cut a fresh worktree, on its own branch, and run there.
                       Without a name the daemon picks one. This is the shape you
                       want for work that must not touch yours: its own tree, its
                       own git index, its own branch, one PR.

  --workspace <name>   An *existing* checkout the daemon manages: `main`, or a
                       worktree named after the branch in it. It must already
                       exist — an unknown name is refused with the list of the
                       ones that do. `orch ls` prints them too.

                       Note what this means: a session already in that workspace
                       keeps working in it. Two agents in one tree share one git
                       index. That is what you want for a hand with the thing you
                       are doing, and not what you want for two parallel jobs.

  --prompt <text>      Typed into the new session once it is up. Without one it
                       sits at an empty prompt waiting for somebody.

With neither flag it lands somewhere it can actually run: beside you in your own
worktree, or in a worktree cut for it when you are in main — main holds one
session and you are it.

Refused when the machine is low on memory, so this cannot be how the desktop
dies. `orch kill` is the way back out.
";

const HELP_KILL: &str = "\
orch kill <id>

Undo a spawn. Ends the session, drops its row, and — when that same `orch new`
cut a worktree for it — removes the worktree too.

**Only sessions you spawned yourself.** Anything else is refused: closing a
conversation somebody is sitting in is a button in the app, not a command here.

The worktree goes through the ordinary teardown checks, so one holding
uncommitted or unpushed work is kept and says why. The session is gone either
way.
";

const HELP_TEARDOWN: &str = "\
orch teardown <workspace>

Remove a worktree, the way the app's own button does. The transcripts of the
sessions that ran there are archived first, and each of them stays in the rail as
a resumable conversation — a resume rebuilds the tree at the same path.

**Every check the button runs, runs here.** A live session, uncommitted changes,
unpushed commits or an attached process refuse, and the refusal names which. It
is never forced. Use it when the person asks for a cleanup; `git worktree remove`
by hand skips the archive and leaves a row the daemon has to skip forever.

Your own workspace is refused too: the live session in it is you. Ask for it from
a session in main, or let the person remove it once you are done. `main` is not a
worktree and is never torn down.
";

const HELP_LS: &str = "\
orch ls [--state <state>] [--all]

One line per session: `id  workspace  state  branch  path`.

The last two answer the question the first three could not — whether two sessions
can safely work in parallel. A shared path means one git index; the same branch
in two rows means one branch.

  --state <state>  Only sessions in this state. One of: starting, working,
                   your_turn, build_failing, error, exited, archived.

  --all            Include archived sessions. They are hidden by default,
                   because the archive is a long tail that buries the live rows.
";

const HELP_ASK: &str = "\
orch ask --question <text> [--detail <text>] [--thread <id>]
         --option <value>:<label>[:<sub>] ...  [--free <value>:<label>]

Ask the human something and block until they answer. Prints the chosen value, and
their words on a second line when they wrote any.

  --question <text>    What you are asking. Required.

  --option <value>:<label>[:<sub>]
                       One answer they may pick. Repeatable, and at least one is
                       required — an open question has no box to type into.

  --free <value>:<label>
                       An option that opens a text box instead of answering, for
                       when none of the above fit.

  --detail <text>      Context under the question: a diff, a file, the
                       reviewer's words. Rendered as-is.

  --thread <id>        The review thread this is about, so the card can say so
                       without you repeating it in the question.

Each poll blocks up to a minute and loops, so a human taking ten minutes is safe.
";

const HELP_RUN: &str = "\
orch run <name>

Start one of the processes this workspace declares — the tabs in the app's
drawer, `docker` or a watch build.

A *name*, not a command: the daemon resolves it against the config and refuses
anything else, so this cannot run arbitrary things. Refused when it is already
up, which is the answer you wanted anyway.
";

const HELP_OUTSIDE: &str = "\
orch outside <path>

Ask to run git outside this session's own worktree. The push guard refuses that
by default; this puts the question to the user and remembers a yes for that folder
for the rest of the session. Another folder is another question. Blocks until they
answer, then prints `allowed` or `refused`.

  <path>  What you were refused, named in the question so the answer is informed.
          A yes covers this folder and what is under it, and nothing else.
";

const HELP_GUARD: &str = "\
orch guard push [--base <branch>] [--main <path>]

Not for you to call. The daemon registers this as a PreToolUse hook; it reads the
payload on stdin and exits 2 to refuse a dangerous push, or a git command aimed
out of the worktree the session works in.

  --base <branch>  The branch that must never be pushed to. Defaults to the
                   daemon's configured upstream ref.
  --main <path>    The main checkout. Without it the worktree rule is skipped,
                   since there is nothing to measure \"another checkout\" against.
";

/// The states `--state` will accept, which are the ones `model::State` has.
///
/// Checked rather than passed through, because an unknown state filters everything
/// out and an empty list reads as "no sessions" rather than "you typo'd".
const STATES: [&str; 7] = [
    "starting",
    "working",
    "your_turn",
    "build_failing",
    "error",
    "exited",
    "archived",
];

fn help_for(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "new" => HELP_NEW,
        "kill" => HELP_KILL,
        "teardown" => HELP_TEARDOWN,
        "ls" => HELP_LS,
        "ask" => HELP_ASK,
        "run" => HELP_RUN,
        "outside" => HELP_OUTSIDE,
        "guard" => HELP_GUARD,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// The argv guard
// ---------------------------------------------------------------------------

/// Whether a flag takes a value, and whether that value may be left off.
#[derive(Clone, Copy, PartialEq)]
enum Arity {
    /// Presence is the whole meaning — `--all`.
    Flag,
    /// The next word is the value, and there must be one.
    Value,
    /// A value when the next word is not itself a flag. `--worktree` alone means
    /// "cut one, you name it"; `--worktree fixer-a` names it.
    Maybe,
}

/// What each command accepts. **This table is the whole of the argv guard.**
///
/// `orch new --help` used to *spawn a session*: an unrecognised argument was
/// ignored, so the probe every CLI in the world answers for free became a side
/// effect with an empty prompt and, at the time, no way to undo it. A command whose
/// job is a side effect must never act on an argv it only partly understood, so
/// anything missing from here is refused with a usage line and a non-zero exit.
fn spec(cmd: &str) -> Option<&'static [(&'static str, Arity)]> {
    Some(match cmd {
        "new" => &[
            ("--workspace", Arity::Value),
            ("--worktree", Arity::Maybe),
            ("--prompt", Arity::Value),
        ],
        "kill" => &[],
        "teardown" => &[],
        "ls" => &[("--state", Arity::Value), ("--all", Arity::Flag)],
        "ask" => &[
            ("--question", Arity::Value),
            ("--detail", Arity::Value),
            ("--thread", Arity::Value),
            ("--option", Arity::Value),
            ("--free", Arity::Value),
        ],
        "run" => &[],
        "outside" => &[],
        "guard" => &[("--base", Arity::Value), ("--main", Arity::Value)],
        _ => return None,
    })
}

/// How many bare words a command takes, and what to say when one is missing.
fn words_wanted(cmd: &str) -> (usize, &'static str) {
    match cmd {
        "kill" => (1, "kill needs the id of a session you spawned"),
        "teardown" => (1, "teardown needs the name of a worktree — `orch ls` prints them"),
        "run" => (1, "run needs the name of a process"),
        "outside" => (1, "outside needs the path you were refused"),
        // `guard push`: the sub-verb is a word, and `guard` checks which one.
        "guard" => (1, "the only guard is `orch guard push`"),
        _ => (0, ""),
    }
}

#[derive(Default, Debug)]
struct Parsed {
    /// Flag to value, in the order given. A `Flag`, and a `Maybe` with nothing
    /// after it, store an empty string — so `has` answers on presence and `value`
    /// on content, and the two questions stay separate.
    given: Vec<(String, String)>,
    words: Vec<String>,
}

impl Parsed {
    fn has(&self, name: &str) -> bool {
        self.given.iter().any(|(n, _)| n == name)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.given
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every occurrence, in order — `--option` is repeatable.
    fn every(&self, name: &str) -> Vec<&str> {
        self.given
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

fn parse(cmd: &str, args: &[String]) -> Result<Parsed, String> {
    let spec = spec(cmd).ok_or_else(|| format!("unknown command `{cmd}`"))?;
    let known = |w: &str| spec.iter().any(|(n, _)| *n == w);

    let mut out = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if !a.starts_with('-') {
            out.words.push(a.to_string());
            i += 1;
            continue;
        }
        let Some(&(_, arity)) = spec.iter().find(|(n, _)| *n == a) else {
            return Err(format!("unknown flag {a} for `orch {cmd}`"));
        };
        match arity {
            Arity::Flag => {
                out.given.push((a.to_string(), String::new()));
                i += 1;
            }
            // A value that is itself one of this command's flags is a *missing*
            // value: `orch new --prompt --workspace x` left a word out. Anything
            // else is taken verbatim, so a prompt that happens to start with a dash
            // still works — refusing prose for its first character would be its own
            // bug.
            Arity::Value => match args.get(i + 1) {
                Some(v) if !known(v) => {
                    out.given.push((a.to_string(), v.clone()));
                    i += 2;
                }
                _ => return Err(format!("{a} needs a value")),
            },
            // Stricter, because here there is nothing to tell an omitted optional
            // value from a value: anything dash-led ends the flag.
            Arity::Maybe => match args.get(i + 1) {
                Some(v) if !v.starts_with('-') => {
                    out.given.push((a.to_string(), v.clone()));
                    i += 2;
                }
                _ => {
                    out.given.push((a.to_string(), String::new()));
                    i += 1;
                }
            },
        }
    }

    let (wanted, missing) = words_wanted(cmd);
    if out.words.len() < wanted {
        return Err(missing.to_string());
    }
    if out.words.len() > wanted {
        return Err(format!("`orch {cmd}` does not take `{}`", out.words[wanted]));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Talking to the daemon
// ---------------------------------------------------------------------------

/// The three variables, and a refusal that names the ones that are absent.
///
/// The old message said "only runs inside a session the daemon started" for every
/// subset, which sends the reader to look at *where they are* — and the session
/// that produced this complaint was in exactly the right place, missing two
/// variables because `spawn_worktree_session` built its environment by hand.
///
/// It does not say whose fault a subset is, because from here that is unknowable:
/// `ORCH_SESSION_ID` is set for every session the daemon starts, since the hooks
/// correlate on it, while `ORCH_URL` and the ask token are the *ask channel* — and
/// a run with nothing to ask legitimately has none. Naming the variables is the
/// part that saves the time either way.
fn session_env() -> Result<(String, String, String), String> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let (url, me, token) = (
        get("ORCH_URL"),
        get("ORCH_SESSION_ID"),
        get("ORCH_ASK_TOKEN"),
    );
    let missing: Vec<&str> = [
        ("ORCH_URL", url.is_none()),
        ("ORCH_SESSION_ID", me.is_none()),
        ("ORCH_ASK_TOKEN", token.is_none()),
    ]
    .into_iter()
    .filter(|(_, absent)| *absent)
    .map(|(k, _)| k)
    .collect();

    if let (Some(url), Some(me), Some(token)) = (url, me, token) {
        return Ok((url, me, token));
    }

    match missing.len() {
        3 => Err("this only runs inside a session the daemon started — \
                  ORCH_URL, ORCH_SESSION_ID and ORCH_ASK_TOKEN are all unset"
            .into()),
        _ => Err(format!(
            "{} not set, so there is no channel to the daemon from here — \
             an interactive session is given all three",
            missing.join(" and ")
        )),
    }
}

/// One blocking HTTP call, via `curl`.
///
/// `curl` rather than an HTTP crate on purpose: this binary rides in the same
/// tarball as the app, and the daemon already shells to `curl` for its own
/// GitHub calls. A second TLS stack for loopback JSON would be all cost.
fn http(method: &str, url: &str, token: &str, body: Option<&str>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "-X", method, "-H", &format!("x-orch-ask: {token}")]);
    if let Some(b) = body {
        cmd.args(["-H", "content-type: application/json", "-d", b]);
    }
    cmd.arg(url);
    let out = cmd.output().map_err(|e| format!("running curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The daemon's reply, or its `{"error": …}` as an `Err`.
///
/// This used to be a hand-rolled field scan, on the grounds that the replies are
/// flat and a real parser bought nothing. `orch ls` ended that: the snapshot is
/// nested, its session objects hold objects of their own, and the scan read the
/// wrong `"state"` and the wrong `"id"` often enough to need two comments saying
/// where to start looking. `serde_json` is already linked in for the guard's
/// payload, so there is nothing left to save.
/// Block until an ask is answered: the option value, and the words if any came.
///
/// Each poll blocks up to a minute and comes back "not yet"; looping is what makes
/// a human taking ten minutes safe. Shared by `ask` and `outside`, which differ
/// only in what they do with the answer — the loop was `ask`'s alone and the second
/// caller is exactly when a copy would start drifting.
fn await_answer(base: &str, me: &str, token: &str, ask: &str) -> Result<(String, String), String> {
    loop {
        let r = http(
            "GET",
            &format!("{base}/api/session/{me}/ask/{ask}/wait"),
            token,
            None,
        )?;
        let v = reply(&r)?;
        if v.get("answered").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        return Ok((
            v.get("answer").and_then(Value::as_str).unwrap_or("").to_string(),
            v.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
        ));
    }
}

fn reply(out: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(out.trim()).map_err(|e| {
        format!(
            "the daemon answered something that is not JSON ({e}): {}",
            out.trim()
        )
    })?;
    if let Some(err) = v.get("error").and_then(Value::as_str) {
        return Err(err.to_string());
    }
    Ok(v)
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    };

    // **Before everything, including the guard and the session environment.** No
    // ordering further down can make `orch new --help` not have spawned a session,
    // so help is answered here and nothing else runs at all. A `--prompt "--help"`
    // is caught by this too; erring towards printing help is the harmless direction.
    if matches!(cmd, "-h" | "--help" | "help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args[1..].iter().any(|a| a == "-h" || a == "--help") {
        return match help_for(cmd) {
            Some(h) => {
                print!("{h}");
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("orch: unknown command `{cmd}`\n\n{USAGE}");
                ExitCode::FAILURE
            }
        };
    }

    let parsed = match parse(cmd, &args[1..]) {
        Ok(p) => p,
        Err(e) => {
            // The usage with the refusal, so a wrong flag does not cost a second
            // command to find out what the right ones were.
            let usage = help_for(cmd).unwrap_or(USAGE);
            eprintln!("orch: {e}\n\n{usage}");
            return ExitCode::FAILURE;
        }
    };

    // The guard is spawned by Claude Code, not by the daemon, so `ORCH_URL` and the
    // ask token are not guaranteed to be there. It asks the daemon one thing when
    // they are (the worktree grant) and refuses as usual when they are not, so it
    // is the one command that must not be gated on a session environment.
    if cmd == "guard" {
        return guard(&parsed);
    }
    match run(cmd, &parsed) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("orch: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `orch guard push` — the `PreToolUse` hook body.
///
/// Exit 2 refuses the tool call and shows stderr to the model; everything else
/// lets it through. **Every unreadable thing here exits 0 on purpose**: a guard
/// that blocks a turn because a payload changed shape would be worse than the
/// mistake it is watching for, and the rules it enforces are a safety net rather
/// than a boundary (see `orchd::guard`).
fn guard(a: &Parsed) -> ExitCode {
    if a.words[0] != "push" {
        eprintln!("orch: the only guard is `orch guard push`");
        return ExitCode::FAILURE;
    }
    let mut payload = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut payload).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(v) = serde_json::from_str::<Value>(&payload) else {
        return ExitCode::SUCCESS;
    };
    let tool_name = v.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let command = v
        .get("tool_input")
        .and_then(|i| i.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // The branch only matters for a bare `git push`, and it is read from the
    // payload's own cwd rather than this process's — a hook's working directory
    // is not promised to be the checkout the command runs in.
    let cwd = v.get("cwd").and_then(Value::as_str);
    let branch = cwd.and_then(current_branch);

    /* The worktree this session may reach, asked of git rather than derived from
       the path: one `rev-parse` answers both halves, and the git dir is the
       exemption the rule cannot do without — a worktree's real one sits under the
       *main* checkout. Only when there is a git command to judge, so the ordinary
       Bash call pays nothing, and only outside main, which is not isolated from
       anything. */
    let (worktree, git_dir) = match (a.value("--main"), cwd.filter(|_| mentions_git(command))) {
        (Some(main), Some(cwd)) => match worktree_of(cwd) {
            Some((top, dir)) if top != std::path::Path::new(main) => (Some(top), Some(dir)),
            _ => (None, None),
        },
        _ => (None, None),
    };
    /* **The grants are handed to the rule rather than turning it off.** The daemon
       holds them per session (`api::allow_outside`), one folder per yes, and
       [`orchd::guard::isolation`] reads them exactly like the worktree — so the rule
       stays a pure function of the command and can still say no to the checkout you
       never approved. This used to drop the worktree from the `Call` on a blanket
       yes, which no list of folders can be expressed as.
       Only asked when there *is* a worktree to be let out of, so an ordinary session
       in main pays nothing, and an empty answer leaves the rule on: a daemon that
       does not answer must not silently widen what an agent may reach. */
    let granted = match &worktree {
        Some(_) => outside_grants(),
        None => Vec::new(),
    };

    let call = orchd::guard::Call {
        tool_name,
        command,
        current_branch: branch.as_deref(),
        cwd: cwd.map(std::path::Path::new),
        worktree: worktree.as_deref(),
        git_dir: git_dir.as_deref(),
        granted: &granted,
    };
    match orchd::guard::check(&call, a.value("--base")) {
        Some(reason) => {
            eprintln!("{reason}");
            // Claude Code's "block and tell the model why" code.
            ExitCode::from(2)
        }
        None => ExitCode::SUCCESS,
    }
}

/// Which folders has the user let this session out to?
///
/// Empty on anything unreadable, which is the direction that keeps the guard
/// honest: the environment may be missing (this runs as Claude Code's child, not
/// the daemon's), the daemon may be gone, and neither is a reason to allow what it
/// would otherwise refuse. The agent's own `orch outside <path>` is what fills it,
/// one folder per answer.
fn outside_grants() -> Vec<std::path::PathBuf> {
    let Ok((base, me, token)) = session_env() else {
        return Vec::new();
    };
    let Ok(out) = http("GET", &format!("{base}/api/session/{me}/outside"), &token, None) else {
        return Vec::new();
    };
    let Ok(v) = reply(&out) else {
        return Vec::new();
    };
    v.get("paths")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(std::path::PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Is there a `git` anywhere in this command? The cheap gate in front of the two
/// subprocesses the worktree rule needs, since most Bash calls are not git at all.
fn mentions_git(command: &str) -> bool {
    command.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| w == "git")
}

/// The worktree root and the real git dir of `cwd`, in one `rev-parse`.
///
/// `--absolute-git-dir` rather than `--git-dir`, which answers a bare `.git` from
/// inside the checkout and would then be resolved against the wrong directory.
fn worktree_of(cwd: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let top = lines.next()?.trim();
    let dir = lines.next()?.trim();
    (!top.is_empty() && !dir.is_empty())
        .then(|| (std::path::PathBuf::from(top), std::path::PathBuf::from(dir)))
}

fn current_branch(cwd: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn run(cmd: &str, a: &Parsed) -> Result<String, String> {
    let (base, me, token) = session_env()?;

    match cmd {
        "new" => {
            // Refused here as well as by the daemon, because this is where the words
            // are: two names for one place is a request nobody can honour.
            if a.has("--workspace") && a.has("--worktree") {
                return Err("--workspace names an existing checkout and --worktree cuts a \
                            new one; pick one"
                    .into());
            }
            let mut body = json!({ "prompt": a.value("--prompt").unwrap_or("") });
            if let Some(w) = a.value("--workspace") {
                body["workspace"] = json!(w);
            }
            if a.has("--worktree") {
                body["worktree"] = json!(true);
                match a.value("--worktree").unwrap_or("") {
                    "" => {}
                    name => body["name"] = json!(name),
                }
            }
            let out = http(
                "POST",
                &format!("{base}/api/session/{me}/spawn"),
                &token,
                Some(&body.to_string()),
            )?;
            let v = reply(&out)?;
            // Where as well as what: a bare uuid left the caller with no way to
            // confirm the spawn landed where it was aimed.
            Ok(format!(
                "{}  {}  {}",
                str_at(&v, "session"),
                str_at(&v, "workspace"),
                str_at(&v, "path")
            ))
        }
        "kill" => {
            let child = &a.words[0];
            let out = http(
                "POST",
                &format!("{base}/api/session/{me}/spawned/{child}/discard"),
                &token,
                None,
            )?;
            let v = reply(&out)?;
            let mut said = format!("killed {}", str_at(&v, "killed"));
            // The worktree is a separate outcome from the session, and a tree that
            // was kept has to say why — the session is gone regardless, so silence
            // here would read as "removed".
            if let Some(ws) = v.get("removed").and_then(Value::as_str) {
                said.push_str(&format!("  (worktree {ws} removed)"));
            } else if let Some(why) = v.get("kept").and_then(Value::as_str) {
                said.push_str(&format!(
                    "  (worktree {} kept: {why})",
                    str_at(&v, "workspace")
                ));
            }
            Ok(said)
        }
        "teardown" => {
            let workspace = &a.words[0];
            let out = http(
                "POST",
                &format!("{base}/api/session/{me}/teardown"),
                &token,
                Some(&json!({ "workspace": workspace }).to_string()),
            )?;
            // A refusal comes back as the error, with the failed checks in its
            // text; success is the preflight that passed, which needs no reading.
            let v = reply(&out)?;
            Ok(format!("removed worktree {}", str_at(&v, "workspace")))
        }
        "ls" => {
            if let Some(want) = a.value("--state") {
                if !STATES.contains(&want) {
                    return Err(format!(
                        "unknown state {want} — one of: {}",
                        STATES.join(", ")
                    ));
                }
            }
            let out = http("GET", &format!("{base}/api/state"), &token, None)?;
            let snap = reply(&out)?;
            let sessions = snap
                .get("sessions")
                .and_then(Value::as_array)
                .ok_or("the daemon's state has no sessions")?;

            let mut rows: Vec<[String; 5]> = Vec::new();
            for s in sessions {
                let st = s
                    .get("state")
                    .and_then(|v| v.get("state"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                match a.value("--state") {
                    Some(want) if st != want => continue,
                    // The archive is a long tail and it buried every live row, so it
                    // is opt-in. `--state archived` asks for it too.
                    None if st == "archived" && !a.has("--all") => continue,
                    _ => {}
                }
                rows.push([
                    str_at(s, "id"),
                    str_at(s, "workspace"),
                    st.to_string(),
                    str_at(s, "branch"),
                    str_at(s, "cwd"),
                ]);
            }
            // Padded rather than tab-separated: the columns stay splittable on
            // whitespace for a shell loop, and readable when a human runs it.
            let width = |col: usize| rows.iter().map(|r| r[col].len()).max().unwrap_or(0);
            let (w1, w2, w3) = (width(1), width(2), width(3));
            Ok(rows
                .iter()
                .map(|r| format!("{}  {:w1$}  {:w2$}  {:w3$}  {}", r[0], r[1], r[2], r[3], r[4]))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "run" => {
            let body = json!({ "name": a.words[0] });
            let out = http(
                "POST",
                &format!("{base}/api/session/{me}/process"),
                &token,
                Some(&body.to_string()),
            )?;
            Ok(str_at(&reply(&out)?, "process"))
        }
        "outside" => {
            let path = &a.words[0];
            let out = http(
                "POST",
                &format!("{base}/api/session/{me}/outside"),
                &token,
                Some(&json!({ "path": path }).to_string()),
            )?;
            let v = reply(&out)?;
            // Answered once already, and still in force: no second question.
            if v.get("allowed").and_then(Value::as_bool) == Some(true) {
                return Ok("allowed".into());
            }
            let ask = v
                .get("ask")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("the daemon did not take the question: {}", out.trim()))?
                .to_string();
            let (answer, _) = await_answer(&base, &me, &token, &ask)?;
            Ok(if answer == orchd::api::OUTSIDE_ALLOW {
                "allowed".into()
            } else {
                "refused — keep git inside this worktree".into()
            })
        }
        "ask" => {
            let question = a.value("--question").ok_or("ask needs --question")?;
            let mut opts: Vec<Value> = Vec::new();
            for o in a.every("--option") {
                let mut parts = o.splitn(3, ':');
                let value = parts.next().unwrap_or_default();
                let label = parts.next().unwrap_or(value);
                let sub = parts.next().unwrap_or("");
                opts.push(json!({ "value": value, "label": label, "sub": sub }));
            }
            // The way out when none of the offered answers fit: the overlay opens
            // a box instead of answering, and what they type comes back too.
            if let Some(f) = a.value("--free") {
                let mut parts = f.splitn(2, ':');
                let value = parts.next().filter(|v| !v.is_empty()).unwrap_or("mine");
                let label = parts.next().unwrap_or("Let me write it…");
                opts.push(json!({ "value": value, "label": label, "free": true }));
            }
            if opts.is_empty() {
                return Err("ask needs at least one --option".into());
            }
            let mut body = json!({ "question": question, "options": opts });
            if let Some(d) = a.value("--detail") {
                body["detail"] = json!(d);
            }
            if let Some(t) = a.value("--thread") {
                body["thread_id"] = json!(t);
            }

            let out = http(
                "POST",
                &format!("{base}/api/session/{me}/ask"),
                &token,
                Some(&body.to_string()),
            )?;
            let ask = reply(&out)?
                .get("ask")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("the daemon did not take the question: {}", out.trim()))?
                .to_string();

            let (answer, text) = await_answer(&base, &me, &token, &ask)?;
            Ok(if text.is_empty() {
                answer
            } else {
                format!("{answer}\n{text}")
            })
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    /// The fault this whole guard exists for. `orch new --help` spawned a session
    /// with an empty prompt, because an unrecognised argument was ignored and the
    /// command went ahead on an argv it had only partly understood.
    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        let e = parse("new", &argv(&["--nonsense"])).expect_err("must not be tolerated");
        assert!(e.contains("unknown flag --nonsense"), "{e}");
        // And on every command, not just the one that bit: a tolerated flag on
        // `ask` sends a question nobody meant to ask.
        for cmd in ["ask", "ls", "run", "kill", "teardown", "outside", "guard"] {
            assert!(
                parse(cmd, &argv(&["--nonsense"])).is_err(),
                "{cmd} tolerated it"
            );
        }
    }

    /// A flag whose value was left out must not swallow the next flag as its value.
    #[test]
    fn a_flag_missing_its_value_is_refused() {
        let e = parse("new", &argv(&["--prompt"])).expect_err("nothing follows it");
        assert!(e.contains("--prompt needs a value"), "{e}");
        let e = parse("new", &argv(&["--prompt", "--workspace", "main"]))
            .expect_err("a flag is not a value");
        assert!(e.contains("--prompt needs a value"), "{e}");
        // But a value that merely looks flag-ish is still a value: a prompt is
        // prose, and refusing one for its first character would be its own bug.
        let p = parse("new", &argv(&["--prompt", "--rebase first"])).expect("prose is a value");
        assert_eq!(p.value("--prompt"), Some("--rebase first"));
    }

    /// `--worktree` is the one optional-value flag, and the two spellings mean
    /// different things from each other.
    #[test]
    fn worktree_takes_a_name_or_no_name() {
        let named = parse("new", &argv(&["--worktree", "fixer-a"])).unwrap();
        assert!(named.has("--worktree"));
        assert_eq!(named.value("--worktree"), Some("fixer-a"));

        let unnamed = parse("new", &argv(&["--worktree", "--prompt", "go"])).unwrap();
        assert!(unnamed.has("--worktree"), "asked for one");
        assert_eq!(unnamed.value("--worktree"), Some(""), "did not name it");
        assert_eq!(unnamed.value("--prompt"), Some("go"));
    }

    /// A stray word is a misunderstanding, not a thing to drop. `orch run` used to
    /// scan for the first non-`--` argument and find the word `run` itself.
    #[test]
    fn bare_words_are_counted() {
        let e = parse("new", &argv(&["main"])).expect_err("new takes no words");
        assert!(e.contains("does not take `main`"), "{e}");
        let e = parse("run", &argv(&[])).expect_err("run needs a name");
        assert!(e.contains("needs the name of a process"), "{e}");
        let e = parse("kill", &argv(&[])).expect_err("kill needs an id");
        assert!(e.contains("needs the id"), "{e}");
        assert_eq!(
            parse("run", &argv(&["docker"])).unwrap().words,
            vec!["docker"]
        );
    }

    /// Every command in the usage has help of its own, and that help names every
    /// flag it takes — otherwise a flag stays undocumented, which is how "the
    /// workspace must already exist" came to be learnable only by triggering it.
    #[test]
    fn every_command_documents_its_own_flags() {
        for cmd in ["new", "kill", "teardown", "ls", "ask", "run", "outside", "guard"] {
            let h = help_for(cmd).unwrap_or_else(|| panic!("{cmd} has no help"));
            let flags = spec(cmd).unwrap_or_else(|| panic!("{cmd} has no flag spec"));
            for (flag, _) in flags {
                assert!(
                    h.contains(flag),
                    "`orch {cmd} --help` does not mention {flag}"
                );
            }
            assert!(
                USAGE.contains(&format!("orch {cmd}")),
                "{cmd} is not in the usage"
            );
        }
    }

    /// The half-populated case names the variables. The old message said "only runs
    /// inside a session the daemon started" for every subset, which sent the reader
    /// looking at where they were instead of at what was absent.
    #[test]
    fn a_half_populated_environment_names_the_variable() {
        // Serial and restored, because these are process-wide.
        let keys = ["ORCH_URL", "ORCH_SESSION_ID", "ORCH_ASK_TOKEN"];
        let before: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        for k in keys {
            std::env::remove_var(k);
        }

        let e = session_env().expect_err("nothing is set");
        assert!(e.contains("only runs inside a session"), "{e}");

        std::env::set_var("ORCH_SESSION_ID", "abc");
        let e = session_env().expect_err("two are missing");
        assert!(e.contains("ORCH_URL and ORCH_ASK_TOKEN"), "{e}");
        assert!(!e.contains("only runs inside a session"), "wrong diagnosis: {e}");

        std::env::set_var("ORCH_URL", "http://127.0.0.1:7777");
        std::env::set_var("ORCH_ASK_TOKEN", "tok");
        let (url, me, token) = session_env().expect("all three");
        assert_eq!(
            (url.as_str(), me.as_str(), token.as_str()),
            ("http://127.0.0.1:7777", "abc", "tok")
        );

        for (k, v) in keys.iter().zip(before) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    /// `--state` is checked against the states that exist. A typo used to filter
    /// everything out, and an empty list reads as "no sessions".
    #[test]
    fn the_state_filter_is_checked_against_the_wire_spellings() {
        let a = parse("ls", &argv(&["--state", "workingg"])).unwrap();
        assert_eq!(a.value("--state"), Some("workingg"));
        assert!(!STATES.contains(&"workingg"), "run() refuses this");
        assert!(
            STATES.contains(&"your_turn"),
            "snake_case is what the snapshot sends"
        );
        assert!(HELP_LS.contains("your_turn"), "and what --help offers");
    }

    /// The daemon's refusals are the error, not `curl`'s exit code.
    #[test]
    fn an_error_body_becomes_an_error() {
        let e = reply(r#"{"error":"unknown workspace foo — known: main"}"#).expect_err("refusal");
        assert!(e.contains("known: main"), "{e}");
        let v = reply(r#"{"session":"abc","workspace":"main","path":"/tmp/x"}"#).unwrap();
        assert_eq!(str_at(&v, "path"), "/tmp/x");
        // An unknown route answers `{}`, which must not read as a successful spawn.
        assert_eq!(str_at(&reply("{}").unwrap(), "session"), "-");
    }
}
