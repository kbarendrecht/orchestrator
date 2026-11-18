---
name: new
description: Use when the user types /new or asks to "run the new build", "restart on the local build": build the release app from this checkout, close the running Orchestrator, and start the local build in its place. Invoking it is the go-ahead for the restart.
---

# Run the new build

Build the app from this checkout, close the Orchestrator that is running, and
start the local build. The person typed `/new`, so the restart is approved: do
not ask again.

## Steps

1. **Build.** From the checkout root:

   ```
   cargo build --release --workspace
   ```

   `--workspace`, not `-p orchestrator-desktop`: the app starts each project's
   daemon from `target/release/orchd`, and building the app alone leaves that
   one stale. If the build fails, stop and show the error. Never close the app
   for a build that did not finish.

2. **Find the running app** by exact pid. List the processes and read the pid
   off the line whose command is the app binary itself:

   ```
   ps -eo pid,args | grep -E "orchestrator-desktop|orchd --main" | grep -v grep
   ```

   The app may be the local build (`./target/release/orchestrator-desktop`) or
   an installed one (a path under `~/.local/share/mise/installs/`). Either one
   gets closed. Note the pids of the `orchd --main` children too, to wait on.
   Never stop it with `pgrep -f` or `pkill`: a pattern also matches this shell's
   own command line, and that has taken the app down before.

3. **Close it, wait, and start the local build, in one detached script.** This
   session is most likely running inside the app, and closing the app kills
   every session in it, this one too. So the kill, the wait and the start go in
   one detached shell that outlives this one; a separate call after the kill
   never runs:

   ```
   cd <checkout> && setsid nohup sh -c '
     kill -TERM <app>
     for i in $(seq 1 20); do ps -p <app>,<child>,... >/dev/null || break; timeout 1 tail -f /dev/null; done
     if ps -p <app>,<child>,... >/dev/null; then
       ps -p <app>,<child>,... -o pid= > /tmp/orchd-new-stuck; exit 1
     fi
     exec env -u ORCHD_CONFIG_DIR -u ORCHD_ADOPTED_LOGIN_PATH -u ORCH_URL \
       -u ORCH_SESSION_ID -u ORCH_ASK_TOKEN ./target/release/orchestrator-desktop
   ' >/dev/null 2>&1 &
   ```

   The `env -u` is required. A session in the app carries its daemon's
   `ORCHD_CONFIG_DIR`, and an app started with it uses that checkout's directory
   as the host's: it finds no `host.json` there and opens only one project.

   It waits up to about 20 seconds (foreground `sleep` is blocked, hence the
   `timeout 1 tail -f /dev/null`). If something is still alive after that it
   does not start, and writes the pids left to `/tmp/orchd-new-stuck`. Do not
   `kill -9`. A child that still holds its checkout's lock makes the new app
   refuse that project. A start while another app runs only hands over to that
   app, which is why the start waits.

4. **Check it came up**, if this session is still alive (outside the app it
   is; inside, the new app resumes it). After about ten seconds, run the `ps`
   line from step 2 again, and read `/tmp/orchd-new-stuck` if it exists. Report the new app pid and which projects have an `orchd --main`
   child. A project missing from that list is either closed in the app (look
   for `asked=true` in `~/.config/orchd/orchd.log`) or failed to start (that
   checkout's own `orchd.log` under `~/.config/orchd/checkouts/`).

## Report

Two or three lines: the commit the build is on (`git log --oneline -1`), the
new pid, and the projects that came back. Say so if the build ran on a dirty
tree, because then the build is not the commit.
