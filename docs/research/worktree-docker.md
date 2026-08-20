# Research: docker-compose + git worktrees, and how agent-orchestrators isolate

Deep-research pass (fan-out web search, adversarial verification). Captured for the
decision in `../workspace-isolation.md`. Confidence and sources are the workflow's.

## Summary

Across real-world projects and the emerging crop of AI coding-agent orchestrators, the dominant and least-surprising convention is to place git worktrees in sibling/external directories (not nested inside the repo) and to give each worktree its own docker-compose project name so containers, networks, and volumes are namespaced per worktree — exploiting Compose's default of deriving the project name from the compose file's directory basename. The most common isolation model for N parallel worktrees is a separate compose project per worktree (via COMPOSE_PROJECT_NAME / -p / dir-derived name), not one shared long-lived stack with docker-exec cwd-switching; a shared-stack-with-host-port-ranges approach exists but is a minority pattern. Containerizing only the services (Postgres/Redis) while running the language toolchain/test runner on the host is a well-established model (Testcontainers), and several AI-agent tools deliberately avoid containers entirely (Uzi = worktrees+tmux+host port ranges; Vibe Kanban = filesystem-only isolation in temp dirs), while others make containers central (Dagger container-use = one fresh container per agent/branch; Docktree/worktree-compose = one isolated compose stack per worktree). The known breakage of worktrees living outside a mounted repo root — the worktree's `.git` file pointing to a gitdir outside the mount — is solved in the devcontainers CLI by explicitly bind-mounting the worktree's common dir (the `--mount-git-worktree-common-dir` flag), without which in-container git fails with `fatal: not a git repository`. For portable, least-surprising defaults: sibling worktree dirs + per-worktree COMPOSE_PROJECT_NAME + services-in-containers/toolchain-on-host is the convergent mainstream.

## Findings

### 1. (high confidence)

Docker Compose derives its project name (which namespaces all containers, networks, and volumes) by a fixed precedence: -p flag, then COMPOSE_PROJECT_NAME env var, then top-level name: in the config, then the basename of the compose-file's directory, then the basename of the current directory. Because the default is the directory basename, two worktrees in differently-named directories automatically get distinct compose projects and thus separate, non-shared resources — but worktrees whose leaf dir names sanitize to the same string (e.g. multiple 'main'/'master' dirs) collide and produce non-descriptive names like main-web-1.

*Evidence.* Official Docker CLI reference states the five-step precedence verbatim and that the default is 'the basename of the project directory containing the config file.' Because resources are namespaced by project name, differently-named worktree dirs yield distinct volumes/containers automatically; identically-named leaf dirs collide. One version nuance: Compose v1 used underscores (main_web_1), v2 uses dashes (main-web-1).

Sources:
- https://docs.docker.com/reference/cli/docker/compose/
- https://docs.docker.com/compose/how-tos/project-name
- https://oliverdavies.dev/archive/2022/08/12/git-worktrees-docker-compose
- https://lours.me/posts/compose-tip-053-project-name-workdir/

### 2. (high confidence)

The recommended, widely-used fix for combining docker-compose with parallel worktrees is to give each worktree its own project name — either by setting COMPOSE_PROJECT_NAME in a per-worktree .env file (which Compose auto-loads) or by passing --project-name/-p. This makes container names, networks, and named volumes unique per worktree, letting multiple project instances run simultaneously with separate data and no cross-worktree interference. A unique project name alone does NOT resolve host PORT collisions when services publish fixed ports.

*Evidence.* Docker docs confirm COMPOSE_PROJECT_NAME overrides the dir-derived default and can be set via an auto-loaded .env file; practitioner blogs demonstrate my-project-main / my-project-staging naming for per-worktree isolation and 'docker-compose up -d --project-name myproject-hotfix'. Named volumes prefixed by project name yield separate data. Caveat (from claim 16 verifier): does not fix fixed-port publishing collisions.

Sources:
- https://docs.docker.com/compose/how-tos/project-name
- https://oliverdavies.dev/archive/2022/08/12/git-worktrees-docker-compose
- https://www.seanmcn.com/blog/2025/07/claude-code-git-worktrees-docker/

### 3. (medium confidence)

The prevailing convention is to place git worktrees in sibling/external directories rather than nested inside the repo. Practitioners use 'git worktree add ../myproject-feature-auth', a dedicated sibling parent dir like my-app-worktrees/<branch>, or (Vibe Kanban) platform temp dirs (/var/tmp/vibe-kanban/worktrees on Linux, system temp on macOS, %TEMP% on Windows). Two candidate 'best practice: siblings' generalizations were adversarially REFUTED, so the sibling preference is real but documented as individual-project practice rather than a proven universal rule.

*Evidence.* Multiple 2025-era blogs and Vibe Kanban's docs independently place worktrees as siblings/external. Note: a broad 'best practice = siblings' claim (gitworktree.org) was refuted 0-3 and a claim that worktree-compose expects siblings was refuted 1-2, so the sibling convention is empirically common in the sources examined but not established as a formally documented universal best practice. The nested-in-repo pattern (.claude/worktrees/, .worktrees/) was not corroborated by any surviving claim.

Sources:
- https://www.seanmcn.com/blog/2025/07/claude-code-git-worktrees-docker/
- https://fabiorehm.com/blog/2025/11/27/working-on-multiple-branches-without-losing-my-mind/
- https://vibekanban.com/docs/workspaces/managing-workspaces

### 4. (medium confidence)

A dedicated tooling ecosystem has emerged that gives each git worktree/branch its OWN isolated docker-compose stack (separate containers, networks, volumes, and DB namespaces) rather than a shared long-lived stack. worktree-compose derives a per-worktree COMPOSE_PROJECT_NAME of the form {repo}-wt-{index}-{branch} and per-worktree ports (20000 + default_port + index). Docktree generates a Compose override with unique project/container names and volumes, allocates ports from a managed pool (41000-49999), and gives shared databases per-worktree tenant namespaces; it also ships agent skills for Claude Code, Codex, Cursor, OpenCode and 60+ agents teaching multi-worktree isolation semantics.

*Evidence.* worktree-compose README states each worktree gets its own COMPOSE_PROJECT_NAME producing separate containers/networks/volumes; Docktree homepage and repo state per-branch isolated stacks with pooled ports, generated Compose overrides, and per-worktree DB tenant namespaces, plus shipped agent skills. Confidence is medium (not high) because these are vendor self-descriptions of mechanism with no third-party functional review found — but the mechanism (project-name namespacing) is standard Compose behavior, not an extraordinary claim.

Sources:
- https://github.com/mostafasudo/worktree-compose
- https://docktree.dev/
- https://github.com/Bnjoroge1/Docktree

### 5. (low confidence)

A minority but real pattern is a SINGLE shared docker-compose stack serving all worktrees, using host port RANGES rather than a separate compose project per worktree — e.g. main gets 3000/3036 and worktrees get 3001-3009 via a '3000-3009:3000-3009' mapping, with the parent folder bind-mounted ('../..:/workspaces') so main and worktrees share one stack. This is the alternative to per-worktree projects and to docker-exec cwd-switching.

*Evidence.* A single Nov-2025 personal blog documents this shared-stack-with-port-ranges model with a parent bind mount. Confidence low: single blog source, describes one author's setup, not a widely corroborated convention. Notably, no surviving claim documented the 'one shared stack + docker exec -w <path> cwd-switching' model the research question asked about — suggesting it is uncommon in practice.

Sources:
- https://fabiorehm.com/blog/2025/11/27/working-on-multiple-branches-without-losing-my-mind/

### 6. (high confidence)

AI coding-agent orchestration tools that spawn many parallel worktrees have split into two camps on containers. Container-central: Dagger container-use gives each agent a fresh dedicated container paired with its own git branch for conflict-free parallelism; Docktree/worktree-compose give each worktree its own compose stack. Container-free: Uzi isolates agents with git worktrees + tmux sessions entirely on the host (no Docker), solving dev-server port collisions by allocating each agent a port from a configured range (e.g. 3000-3010) injected via a $PORT placeholder; Vibe Kanban's worktree management is filesystem/working-directory isolation only in temp dirs, with no container-based isolation described.

*Evidence.* container-use README: 'Each agent gets a fresh container in its own git branch.' Uzi README/docs: git worktrees + tmux, portRange + $PORT placeholder, no Docker layer. Vibe Kanban docs mention no Docker/containers for worktree isolation (only filesystem worktrees); third-party notes it 'avoids Docker overhead.' This directly answers Q4: the ecosystem has NOT converged on containers — the lightweight worktree(+tmux)+host-port model is at least as common as the container-per-agent model. Note: the specific tools named in the question (Claude Squad, Crystal, Conductor) were not covered by surviving claims.

Sources:
- https://github.com/dagger/container-use
- https://github.com/devflowinc/uzi
- https://vibekanban.com/docs/workspaces/managing-workspaces
- https://docktree.dev/

### 7. (high confidence)

Running the test dependencies (databases, brokers) as Docker containers while the tests/language toolchain run on the HOST — the services-only-in-containers model — is a well-established, mainstream pattern, exemplified by Testcontainers, which embeds a library in the test process that provisions services as containers and maps their ports to random host ports so host-run tests connect via localhost:{dynamic-port}. This is presented as the default, not a containerized test runner.

*Evidence.* Testcontainers getting-started states 'Your tests run using these containerized services' and 'You can run your integration tests right from your IDE, just like you run unit tests', confirming host-run tests + containerized dependencies. This answers Q3: containerizing only services is common and mature. However, no surviving claim quantified how common services-only is VERSUS a containerized test runner across the field, so the relative prevalence is inferred, not measured.

Sources:
- https://testcontainers.com/getting-started/
- https://testcontainers.com/guides/introducing-testcontainers/

### 8. (high confidence)

When a worktree lives OUTSIDE the mounted repo root, in-container git breaks because the worktree's .git file points to a gitdir/common-dir outside the mount. The devcontainers CLI solves this with a dedicated flag, --mount-git-worktree-common-dir (added in PR #1127, changelog v0.81.0, Jan 2026): the CLI reads the worktree's .git file, resolves the common dir it points to, and adds a separate bind mount for it. Without that mount, in-container git fails with 'fatal: not a git repository'. Known gap: the resolution is silently skipped when devcontainer.json sets a custom workspaceMount (issue #1243).

*Evidence.* devcontainers/cli CHANGELOG v0.81.0 lists PR #1127 'Add option to mount a worktree's common folder'; issue #1243 states verbatim that when workspaceMount is present 'the .git file is never read, the common dir is never resolved, and no mount is added,' causing 'fatal: not a git repository.' This directly answers Q5: the established fix is to bind-mount the resolved common dir alongside the worktree. For a custom Rust daemon the takeaway is to detect the worktree .git gitdir pointer and mount its target explicitly.

Sources:
- https://github.com/devcontainers/cli/issues/1243
- https://github.com/devcontainers/cli/blob/main/CHANGELOG.md

