//! `orch` — what a running session can ask the daemon for.
//!
//! Shipped in the same tarball as the app and installed by the same mise tool, so
//! an agent finds it on `PATH` and the monorepo it is working in stays clean: no
//! script, no `.claude/` entry, nothing to keep in step with this repo.
//!
//! It needs no configuration. A session's environment already says where the
//! daemon is (`ORCH_URL`), who the session is (`ORCH_SESSION_ID`) and what it is
//! allowed to do (`ORCH_ASK_TOKEN`). That token opens asking and spawning and
//! nothing else, which is the point: this is not a remote control for the daemon,
//! it is the two things an agent legitimately needs.
//!
//! It replaces a page of `curl | jq` in `commands/resolve-run.md`, where the
//! long-poll loop was written out by hand and easy to get wrong.

use std::process::ExitCode;

const USAGE: &str = "\
orch — talk to the orchestrator you are running inside

  orch new [--workspace <name>] [--prompt <text>]
      Start another session. Defaults to your own workspace. Refused when the
      machine is low on memory, so this cannot be how the desktop dies.

  orch ask --question <text> [--detail <text>] [--thread <id>]
           --option <value>:<label>[:<sub>] ...  [--free <value>:<label>]
      Ask the human something and block until they answer. Prints the chosen
      value, and their words on a second line when they wrote any.

  orch ls
      The sessions the daemon knows about, one per line.

Environment (set for you): ORCH_URL, ORCH_SESSION_ID, ORCH_ASK_TOKEN
";

fn env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| {
        format!("{key} is not set — `orch` only runs inside a session the daemon started")
    })
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
        return Err(format!(
            "{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pull one string field out of a JSON object without a JSON crate.
///
/// The daemon's replies here are flat and its own, so a full parser buys nothing.
/// Anything unexpected falls through to the caller printing the raw body, which is
/// more useful than a parse error about a shape nobody promised.
fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{key}\""))?;
    let rest = &json[at + key.len() + 2..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(&rest[start..end])
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn flags(args: &[String], name: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| *a == name)
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect()
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    };
    match run(cmd, &args) {
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

fn run(cmd: &str, args: &[String]) -> Result<String, String> {
    if matches!(cmd, "-h" | "--help" | "help") {
        return Ok(USAGE.trim_end().to_string());
    }
    let base = env("ORCH_URL")?;
    let me = env("ORCH_SESSION_ID")?;
    let token = env("ORCH_ASK_TOKEN")?;

    match cmd {
        "new" => {
            let mut body = String::from("{");
            if let Some(w) = flag(args, "--workspace") {
                body.push_str(&format!("\"workspace\":\"{}\",", esc(&w)));
            }
            let prompt = flag(args, "--prompt").unwrap_or_default();
            body.push_str(&format!("\"prompt\":\"{}\"}}", esc(&prompt)));
            let out = http("POST", &format!("{base}/api/session/{me}/spawn"), &token, Some(&body))?;
            match (field(&out, "session"), field(&out, "error")) {
                (_, Some(err)) => Err(err.to_string()),
                (Some(id), _) => Ok(id.to_string()),
                _ => Ok(out.trim().to_string()),
            }
        }
        "ask" => {
            let question = flag(args, "--question")
                .ok_or("ask needs --question")?;
            let mut opts: Vec<String> = Vec::new();
            for o in flags(args, "--option") {
                let mut parts = o.splitn(3, ':');
                let value = parts.next().unwrap_or_default();
                let label = parts.next().unwrap_or(value);
                let sub = parts.next().unwrap_or("");
                opts.push(format!(
                    "{{\"value\":\"{}\",\"label\":\"{}\",\"sub\":\"{}\"}}",
                    esc(value), esc(label), esc(sub)
                ));
            }
            // The way out when none of the offered answers fit: the overlay opens
            // a box instead of answering, and what they type comes back too.
            if let Some(f) = flag(args, "--free") {
                let mut parts = f.splitn(2, ':');
                let value = parts.next().unwrap_or("mine");
                let label = parts.next().unwrap_or("Let me write it…");
                opts.push(format!(
                    "{{\"value\":\"{}\",\"label\":\"{}\",\"free\":true}}",
                    esc(value), esc(label)
                ));
            }
            if opts.is_empty() {
                return Err("ask needs at least one --option".into());
            }
            let mut body = format!("{{\"question\":\"{}\"", esc(&question));
            if let Some(d) = flag(args, "--detail") {
                body.push_str(&format!(",\"detail\":\"{}\"", esc(&d)));
            }
            if let Some(t) = flag(args, "--thread") {
                body.push_str(&format!(",\"thread_id\":\"{}\"", esc(&t)));
            }
            body.push_str(&format!(",\"options\":[{}]}}", opts.join(",")));

            let out = http("POST", &format!("{base}/api/session/{me}/ask"), &token, Some(&body))?;
            let ask = field(&out, "ask")
                .ok_or_else(|| format!("the daemon did not take the question: {}", out.trim()))?
                .to_string();

            // Each poll blocks up to a minute and comes back "not yet"; looping is
            // what makes a human taking ten minutes safe.
            loop {
                let r = http(
                    "GET",
                    &format!("{base}/api/session/{me}/ask/{ask}/wait"),
                    &token,
                    None,
                )?;
                if r.contains("\"answered\":true") {
                    let answer = field(&r, "answer").unwrap_or("").to_string();
                    let text = field(&r, "text").unwrap_or("");
                    return Ok(if text.is_empty() {
                        answer
                    } else {
                        format!("{answer}\n{text}")
                    });
                }
                if let Some(err) = field(&r, "error") {
                    return Err(err.to_string());
                }
            }
        }
        "ls" => {
            let out = http("GET", &format!("{base}/api/state"), &token, None)?;
            // From the sessions array onward: the snapshot lists workspaces first,
            // and their ids look identical to a scan that starts at the top.
            let from = out
                .find("\"sessions\"")
                .ok_or("the daemon's state has no sessions")?;
            let mut lines = Vec::new();
            for chunk in out[from..].split("\"id\":\"").skip(1) {
                let Some(end) = chunk.find('"') else { continue };
                let id = &chunk[..end];
                // Deliberately crude: id, workspace and state, for a shell loop
                // rather than for reading. Anything richer wants the real API.
                let ws = field(chunk, "workspace").unwrap_or("?");
                // `state` is an object whose first key is also `state`, so the
                // plain lookup finds the key rather than the value.
                let st = chunk
                    .find("\"state\":{")
                    .and_then(|at| field(&chunk[at + 8..], "state"))
                    .unwrap_or("?");
                lines.push(format!("{id}  {ws}  {st}"));
            }
            Ok(lines.join("\n"))
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}
