//! Which coding agent a session runs, and how each one is started.
//!
//! **A closed set, so an enum and not a trait.** Every difference between the two
//! is a fact about a program somebody else ships, and a `match` per fact keeps
//! each one next to its twin, where a third agent would have to answer it too.
//! What is the same for both stays where it was: the pty, the state machine, the
//! hook routes. The hooks are the one place the two meet halfway: Codex adopted
//! Claude Code's payloads and its `PreToolUse` answer, so one set of handlers
//! serves both and only the delivery differs.
//!
//! The daemon's session id is its own. For Claude Code it is also the agent's,
//! because `--session-id` lets the daemon choose, until `/clear` moves the pane to
//! a conversation of Claude Code's choosing; Codex picks its own UUIDv7 and has
//! no flag to take one. Either way the agent's id is learned from a hook and kept
//! beside the daemon's (`Session::agent_session_id`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub enum Agent {
    /// The default, so a record written before there was a choice reads as what
    /// it always was.
    #[default]
    Claude,
    Codex,
}

/// What a spawn continues, in the agent's own ids.
#[derive(Debug, Clone, Copy)]
pub enum Start<'a> {
    New,
    Resume(&'a str),
    Fork(&'a str),
}

impl Agent {
    /// In the order a menu lists them.
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    /// The binary, as a `PATH` lookup. The e2e fakes rely on that: each stands in
    /// for its agent by being first on the `PATH` under this name.
    pub fn program(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// Whether the first hook waits for a prompt.
    ///
    /// Claude Code fires `SessionStart` once its prompt box is up, and the daemon
    /// types the line then: `initialUserMessage` is honoured only headless. Codex
    /// fires `SessionStart` inside the first turn instead (measured on 0.162.1,
    /// nothing fired until a prompt was sent), so two things follow: a first
    /// prompt known at spawn goes on the command line, which both `codex [PROMPT]`
    /// and `codex resume <id> [PROMPT]` submit on arrival, and readiness is read
    /// off the pty rather than off a hook.
    pub fn hooks_wait_for_a_prompt(self) -> bool {
        matches!(self, Agent::Codex)
    }

    /// Whether the agent picks its conversation's id itself.
    ///
    /// Claude Code takes the daemon's (`--session-id`) and finds its transcript
    /// from it. Codex picks its own and takes none, so the daemon learns the id and
    /// the rollout from the first hook that carries them, and has to carry both
    /// itself until then.
    pub fn names_its_own_session(self) -> bool {
        matches!(self, Agent::Codex)
    }

    /// Whether a turn that fails says so only in the transcript.
    ///
    /// Claude Code fires `StopFailure`. Codex fires nothing at all: measured on
    /// 0.162.1, a turn that gave up on a 401 after five reconnects sent no `Stop`
    /// and no other hook, so the rail went on reading `Working` until the next
    /// prompt. The rollout does record it, as a `task_complete` carrying an
    /// `error`, so for this agent the daemon watches the rollout for the end of
    /// each turn (`hooks::watch_turn_end`).
    pub fn fails_a_turn_silently(self) -> bool {
        matches!(self, Agent::Codex)
    }

    /// Whether the daemon can open the agent's own rewind picker, by typing two
    /// escapes at its prompt (`api::rewind_session`).
    ///
    /// Claude Code's alone: the gesture, its timing and the states where it means
    /// something else were measured in Claude Code's binary, and the bytes are
    /// written into the pane blind. In another agent two escapes at the prompt are
    /// whatever that agent makes of them, so the row does not offer it.
    pub fn can_rewind(self) -> bool {
        matches!(self, Agent::Claude)
    }

    /// The line that runs one of the daemon's skills with `args`.
    ///
    /// Claude Code namespaces a plugin's skills, so it is the slash command. Codex
    /// refuses a slash command it does not know, and reaches a skill the daemon
    /// carries only through the instructions that name it, which read this same
    /// form.
    pub fn skill_line(self, skill: &str, args: &str) -> String {
        let line = match self {
            Agent::Claude => format!("/orchd:{skill}"),
            Agent::Codex => format!("orchd:{skill}"),
        };
        if args.is_empty() {
            line
        } else {
            format!("{line} {args}")
        }
    }

    /// The interactive argv: the program, how it continues, the per-process
    /// `flags`, and the first prompt when [`Self::hooks_wait_for_a_prompt`].
    ///
    /// `id` is the daemon's id, which Claude Code is told to use; Codex ignores it.
    pub fn argv(
        self,
        id: uuid::Uuid,
        start: Start<'_>,
        flags: Vec<String>,
        prompt: Option<&str>,
    ) -> Vec<String> {
        let mut cmd = vec![self.program().to_string()];
        match self {
            Agent::Claude => {
                // `--resume` already decides the id; a fork does not, and
                // `--session-id` is honoured alongside `--fork-session`, so the
                // daemon's id and Claude's stay one value there too.
                match start {
                    Start::New => cmd.extend(["--session-id".into(), id.to_string()]),
                    Start::Resume(prev) => cmd.extend(["--resume".into(), prev.to_string()]),
                    Start::Fork(prev) => cmd.extend([
                        "--session-id".into(),
                        id.to_string(),
                        "--resume".into(),
                        prev.to_string(),
                        "--fork-session".into(),
                    ]),
                }
                cmd.extend(flags);
            }
            Agent::Codex => {
                let prev = match start {
                    Start::New => None,
                    Start::Resume(prev) => Some(("resume", prev)),
                    Start::Fork(prev) => Some(("fork", prev)),
                };
                if let Some((verb, _)) = prev {
                    cmd.push(verb.to_string());
                }
                cmd.extend(flags);
                // Positionals behind `--`, so a prompt that starts with a dash is a
                // prompt and not an option.
                let rest: Vec<&str> = prev.map(|(_, p)| p).into_iter().chain(prompt).collect();
                if !rest.is_empty() {
                    cmd.push("--".to_string());
                    cmd.extend(rest.into_iter().map(str::to_string));
                }
            }
        }
        cmd
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> Vec<String> {
        vec!["-x".to_string()]
    }

    #[test]
    fn claude_is_told_the_daemons_id_on_every_start_but_a_resume() {
        let id = uuid::Uuid::nil();
        let s = id.to_string();
        assert_eq!(
            Agent::Claude.argv(id, Start::New, flags(), Some("ignored")),
            ["claude", "--session-id", &s, "-x"]
        );
        assert_eq!(
            Agent::Claude.argv(id, Start::Resume("p"), flags(), None),
            ["claude", "--resume", "p", "-x"]
        );
        assert_eq!(
            Agent::Claude.argv(id, Start::Fork("p"), flags(), None),
            [
                "claude",
                "--session-id",
                &s,
                "--resume",
                "p",
                "--fork-session",
                "-x"
            ]
        );
    }

    #[test]
    fn codex_takes_its_subcommand_first_and_its_positionals_last() {
        let id = uuid::Uuid::nil();
        assert_eq!(
            Agent::Codex.argv(id, Start::New, flags(), None),
            ["codex", "-x"]
        );
        assert_eq!(
            Agent::Codex.argv(id, Start::New, flags(), Some("-p looks like a flag")),
            ["codex", "-x", "--", "-p looks like a flag"]
        );
        assert_eq!(
            Agent::Codex.argv(id, Start::Resume("01a1"), flags(), Some("go")),
            ["codex", "resume", "-x", "--", "01a1", "go"]
        );
        assert_eq!(
            Agent::Codex.argv(id, Start::Fork("01a1"), flags(), None),
            ["codex", "fork", "-x", "--", "01a1"]
        );
    }

    #[test]
    fn a_record_without_an_agent_is_claude() {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            agent: Agent,
        }
        let r: R = serde_json::from_str("{}").unwrap();
        assert_eq!(r.agent, Agent::Claude);
        assert_eq!(serde_json::to_string(&Agent::Codex).unwrap(), "\"codex\"");
    }
}
