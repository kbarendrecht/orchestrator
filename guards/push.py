#!/usr/bin/env python3
"""Refuse git pushes that rebind tracking or rewrite a ref you do not own.

Registered as a `PreToolUse` hook on `Bash` by the daemon (§8's blast-radius
guards). Deliberately a local script rather than an HTTP hook: an http guard
fails open when the daemon is down, and "the force-push guard silently stopped
existing" is not a failure mode worth having.

Additive to the repo's own `pre-bash` — any hook exiting 2 blocks, so both sets
of rules apply (§11).
"""

import json
import re
import sys

PROTECTED = ("develop", "main", "master", "release")


def deny(message):
    # Exit 2 blocks the tool call and shows stderr to the model.
    print(message, file=sys.stderr)
    sys.exit(2)


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        # Unparseable input is not a reason to block a turn.
        return 0

    if payload.get("tool_name") != "Bash":
        return 0
    command = (payload.get("tool_input") or {}).get("command") or ""
    if not re.search(r"\bgit\s+push\b", command):
        return 0

    # `push -u` rebinds the branch's upstream to origin, which breaks `git pull`
    # tracking — load-bearing for a triangular remote setup.
    if re.search(r"(^|\s)-u(\s|$)", command) or "--set-upstream" in command:
        deny(
            "orchd: `git push -u` is denied. It rebinds the branch's upstream to "
            "origin and breaks pull tracking. Push bare: `git push`."
        )

    if re.search(r"--force(?!-with-lease)", command) or re.search(
        r"(^|\s)-f(\s|$)", command
    ):
        deny(
            "orchd: plain `--force` is denied. Use `--force-with-lease`, which "
            "refuses when someone else has pushed since you last fetched."
        )

    tokens = command.split()
    for index, token in enumerate(tokens):
        # `HEAD:develop` and a bare `develop` are the same mistake.
        ref = token.split(":")[-1].strip("'\"")
        if ref in PROTECTED:
            deny(f"orchd: pushing to `{ref}` is denied. Open a PR instead.")
        if token == "upstream" and index + 1 < len(tokens):
            deny(
                "orchd: pushing to `upstream` is denied. Feature branches go to "
                "origin; PRs are opened against upstream."
            )

    return 0


if __name__ == "__main__":
    sys.exit(main())
