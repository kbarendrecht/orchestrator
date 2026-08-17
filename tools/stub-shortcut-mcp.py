#!/usr/bin/env python3
"""A stub Shortcut MCP server, for proving the story-filing path without filing one.

Deliberately a *real* stdio MCP server named `shortcut`, rather than a fake
`file_all` in Rust. That way the tool names the prompt and the repo's own skill
reach for are byte-identical to the live ones, and there is no difference in the
daemon between stub and live beyond which flags `story::run_filer` passes. A Rust
stub would prove the cache and the report and nothing about the part that actually
breaks.

Every call is appended to `--log` as JSONL. That log is the point: it is how a test
asserts **exactly one** story was created for a batch, which is the property that
matters and the one a screenshot cannot show.

`--sleep-after-response` answers `stories-create` and then hangs, so the death
window — Shortcut said yes, the agent never reported back — can be reproduced on
purpose rather than waited for.
"""

import argparse
import json
import sys
import time

ORG = "acme-stub"
ARGS = None


def log(entry):
    if not ARGS.log:
        return
    with open(ARGS.log, "a") as f:
        f.write(json.dumps(entry) + "\n")
        f.flush()


def created_stories():
    """Every story this stub has ever created, replayed from the log.

    State lives in the log rather than in memory because the interesting tests
    span two runs: one that creates and dies, and one that has to *find* what the
    first one made.
    """
    out = []
    if not ARGS.log:
        return out
    try:
        with open(ARGS.log) as f:
            for line in f:
                try:
                    entry = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if entry.get("created"):
                    out.append(entry["created"])
    except FileNotFoundError:
        pass
    return out


def story(number, name, description):
    return {
        "id": number,
        "name": name,
        "description": description,
        "app_url": f"https://app.shortcut.com/{ORG}/story/{number}",
        "story_type": "chore",
        "workflow_state_id": 500000008,
    }


TOOLS = [
    ("stories-create", "Create a story", ["name", "description"]),
    ("stories-search", "Search stories", ["query"]),
    ("stories-update", "Update a story", ["storyPublicId"]),
    ("stories-get-by-id", "Get a story", ["storyPublicId"]),
    ("epics-search", "Search epics", ["query"]),
    ("labels-list", "List labels", []),
    ("teams-list", "List teams", []),
    ("workflows-list", "List workflows", []),
    ("custom-fields-list", "List custom fields", []),
]


def call(name, args):
    """Answer one tool call. Returns the text payload the agent sees."""
    if name == "stories-create":
        number = ARGS.first_id + len(created_stories())
        made = story(number, args.get("name", ""), args.get("description", ""))
        log({"tool": name, "args": args, "created": made})
        if ARGS.sleep_after_response:
            # Flush the answer first, then hang: the agent has its id and is about
            # to be killed before it can write the report.
            sys.stdout.flush()
            time.sleep(ARGS.sleep_after_response)
        return made

    if name == "stories-search":
        # Substring match on the description, which is what the real search is
        # being relied on for: finding the thread permalink the daemon appended.
        query = str(args.get("query", "")).strip()
        hits = [s for s in created_stories() if query and query in s.get("description", "")]
        log({"tool": name, "args": args, "hits": [s["id"] for s in hits]})
        return {"stories": hits}

    if name == "stories-get-by-id":
        want = str(args.get("storyPublicId", ""))
        log({"tool": name, "args": args})
        for s in created_stories():
            if str(s["id"]) == want.removeprefix("sc-"):
                return s
        return {"error": f"no story {want}"}

    log({"tool": name, "args": args})
    if name == "stories-update":
        return {"id": args.get("storyPublicId"), "updated": True}
    if name == "epics-search":
        return {"epics": [{"id": 39943, "name": "Chores (stub)"}]}
    if name == "labels-list":
        return {"labels": []}
    if name == "teams-list":
        return {"teams": [{"id": "stub-team", "mention_name": "dev"}]}
    if name == "workflows-list":
        return {
            "workflows": [{
                "id": 500000005,
                "name": "Development",
                "states": [{"id": 500000008, "name": "Backlog"}],
            }]
        }
    if name == "custom-fields-list":
        return {"custom_fields": []}
    return {"error": f"the stub does not implement {name}"}


def handle(req):
    method = req.get("method")
    if method == "initialize":
        return {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "shortcut", "version": "0.0.1-stub"},
        }
    if method == "tools/list":
        return {
            "tools": [{
                "name": name,
                "description": f"{desc} (stub)",
                "inputSchema": {
                    "type": "object",
                    "properties": {k: {"type": "string"} for k in required},
                    "required": required,
                },
            } for name, desc, required in TOOLS]
        }
    if method == "tools/call":
        params = req.get("params") or {}
        name = params.get("name", "")
        result = call(name, params.get("arguments") or {})
        # MCP wraps a tool result in content blocks; the agent reads the text.
        return {"content": [{"type": "text", "text": json.dumps(result, indent=2)}]}
    if method == "ping":
        return {}
    return None


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--log", help="append every call here as JSONL")
    p.add_argument("--first-id", type=int, default=90001)
    p.add_argument("--sleep-after-response", type=float, default=0.0,
                   help="hang this long after answering stories-create")
    ARGS = p.parse_args()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        # A notification has no id and takes no reply.
        if "id" not in req:
            continue
        try:
            result = handle(req)
            if result is None:
                out = {"jsonrpc": "2.0", "id": req["id"],
                       "error": {"code": -32601, "message": f"no method {req.get('method')}"}}
            else:
                out = {"jsonrpc": "2.0", "id": req["id"], "result": result}
        except Exception as e:  # noqa: BLE001 — a stub must answer, not die
            out = {"jsonrpc": "2.0", "id": req["id"],
                   "error": {"code": -32603, "message": str(e)}}
        sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
