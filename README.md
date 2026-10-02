# ctm — Claude multiplexer

`ctm` runs many headless [Claude Code](https://docs.claude.com/en/docs/claude-code)
sessions side by side. It has a web dashboard, a JSON API, an agent scheduler
and a terminal client. Each run can execute as a local process, inside a
throw-away Docker container, or as a **Claude Code cloud session**
(`claude --cloud`).

- **Backend:** Rust, [axum](https://github.com/tokio-rs/axum), SQLite (sqlx), tokio
- **UI:** server-rendered [Leptos](https://leptos.dev) components plus
  [htmx](https://htmx.org). Live logs use htmx's SSE extension. No JS
  build step: htmx is vendored in `assets/` and compiled into the binary.

## Concepts

| | |
|---|---|
| **Run** | One Claude invocation. Has a status, a live log, a session id, a result and a cost. |
| **Agent** | A reusable Claude setup: mode (local / docker / cloud), repo, model, permission mode, allowed tools, standing instructions (`--append-system-prompt`). Optionally **persistent**: every run resumes the agent's previous session, so it remembers earlier runs. |
| **Schedule (job)** | Fires an agent with a prompt on a cron expression, a fixed interval, or once at a set time. Has a time zone, jitter, overlap policy, timeout, retries with backoff, and an optional chained job. |

## Features

- Submit prompts from the dashboard or the terminal (`ctm run "…"`).
- Up to `CTM_MAX_CONCURRENT` runs at once. Extra runs wait in a FIFO queue,
  and each workspace runs one job at a time.
- Live output streamed over SSE. Claude's `stream-json` events are shown as
  text, tool calls, tool results and a final cost summary.
- **Continue session:** a follow-up resumes the same Claude session
  (`--resume`, or `--cloud <id>` for cloud runs) in the same workspace.
- **Cloud mode:** creates a Claude Code cloud session for the agent's repo,
  or sends the prompt to an existing session. The run is marked
  `dispatched`, links to `claude.ai/code/session_…`, and the session keeps
  working in Anthropic's cloud.
- **Claude sign-in from the dashboard:** ctm runs `claude auth login` (or
  `claude setup-token`) for you. You open the link, approve, and paste the
  code back. You can also paste an API key or token. Every run uses the
  stored credentials, so nothing has to be exported on the server.
- **Agent scheduling:**
  - cron with time zones (`0 9 * * 1-5` in `Europe/Berlin`, `@daily`),
    intervals (`15m`, `1h30m`), and one-shot times (`2026-12-01 09:00`), with
    a live preview of upcoming fire times
  - overlap policies: `skip` the slot, `queue` another run, or `replace`
    (cancel the running one)
  - per-run timeout, retries with exponential backoff, jitter
  - job chaining (`then run …` on success)
  - prompt templates (see below)
  - missed slots, e.g. while ctm was down, collapse into a single run
- Cancel queued or running jobs. In docker mode the container is killed too.
- Optional shared-token auth for the dashboard and API.

## Quick start (local)

```sh
cargo build --release
./target/release/ctm serve               # http://127.0.0.1:7878
```

Open **Claude account** in the dashboard and sign in, or run
`ctm auth login`. Alternatively, export `ANTHROPIC_API_KEY` or
`CLAUDE_CODE_OAUTH_TOKEN` before starting the server. `claude` must be on
`PATH` (`npm i -g @anthropic-ai/claude-code`).

## Quick start (Docker)

```sh
cp .env.example .env     # optional: CTM_TOKEN, Claude credentials
docker compose build     # builds ctm:latest and ctm-runner:latest
docker compose up -d ctm
```

Open http://localhost:7878 and sign in to Claude under **Claude account**.
By default the port is published on `127.0.0.1` only and there is no ctm
login. If you set `CTM_TOKEN`, the dashboard asks for it once (it's kept in
a cookie). Set it before exposing the port beyond localhost. The compose file mounts the host's Docker socket so
the server can start sibling `ctm-runner` containers for docker-mode runs.
Credentials and Claude's own config live in the `ctm-data` volume.

## Terminal client

The same binary is the client. Point it at a server with `CTM_URL` and
`CTM_TOKEN`, or with `--url` and `--token`.

```sh
export CTM_URL=https://ctm.example.com CTM_TOKEN=...

# credentials (delegated to the server)
ctm auth login                     # prints a sign-in URL, asks for the code
ctm auth login --token-only        # long-lived inference token instead
echo sk-ant-api03-... | ctm auth set-key
ctm auth status

# one-off runs
ctm run -f "fix the flaky test in tests/api.rs" --repo https://github.com/me/app.git --docker
ctm run --cloud "upgrade the deps and open a PR" --repo https://github.com/me/app.git
ctm run --cloud --session https://claude.ai/code/session_01... "also update the changelog"
ctm ps; ctm logs 42 -f; ctm resume 42 -f "now open a PR"; ctm cancel 43; ctm rerun 41

# agents
ctm agent add triage --repo https://github.com/me/app.git --persistent \
    --permission-mode acceptEdits --allowed-tools "Bash(gh issue *) Read" \
    --system-prompt "You triage incoming GitHub issues."
ctm agent add releaser --cloud --repo https://github.com/me/app.git
ctm agent run triage -f "anything urgent today?"

# schedules
ctm job add --name morning-triage --agent triage --cron "0 9 * * 1-5" --tz Europe/Berlin \
    --timeout 1800 --retries 2 "Triage issues opened since {{last_status}} run. Today is {{weekday}} {{date}}."
ctm job add --name hourly-ci --agent triage --every 1h --overlap skip "Check CI on main"
ctm job add --name release --agent releaser --at "2026-12-01 09:00" --tz America/New_York "Cut the 2.0 release"
ctm job preview --cron "30 9 * * 1-5" --tz Asia/Tokyo
ctm job ls; ctm job trigger 1 -f; ctm job disable 2
```

When run with `-f`, `ctm run`, `ctm resume`, `ctm logs`, `ctm agent run` and
`ctm job trigger` exit non-zero unless the run ends `succeeded` or
`dispatched`, so you can use them in scripts and CI.

### Prompt templates

Job prompts can use these placeholders, expanded when the job fires:

| Placeholder | Value |
|---|---|
| `{{date}}` `{{time}}` `{{datetime}}` `{{weekday}}` `{{timezone}}` | now, in the job's time zone |
| `{{job}}` `{{agent}}` | names |
| `{{trigger}}` | `schedule`, `manual` or `chain` |
| `{{last_status}}` `{{last_result}}` | the job's previous finished run |
| `{{upstream_result}}` | result of the run that triggered a chained job |

## How runs execute

| | local | docker | cloud |
|---|---|---|---|
| process | `claude -p` in `$CTM_DATA_DIR/workspaces/<ws>` | `docker run --rm -i --name ctm-run-<id> <image> claude -p …` | new: `claude --cloud=<prompt>` in a pseudo terminal, detached once the session URL appears · existing: `claude -p --cloud <session>` |
| workspace | directory, cloned from `repo` if empty | volume `ctm-ws-<ws>` at `/workspace` | local checkout (gives the CLI the repo); the work happens in the cloud |
| result | final `stream-json` result + cost | same | session URL (new) or the session's reply (existing) |

For local and docker runs, the prompt is written to `claude -p` on **stdin**.
Arguments are, in order: `CTM_CLAUDE_ARGS` (default
`--output-format stream-json --verbose`), `--model`, `--resume`,
`--append-system-prompt`, `--permission-mode`, `--allowedTools`, then the
extra args.

The workspace key `<ws>` is `agent-<id>` for agent and scheduled runs, so an
agent keeps its checkout (and, if persistent, its conversation) between
runs. Ad-hoc runs use `run-<id>`, and follow-ups reuse their parent's
workspace.

Headless Claude can't answer permission prompts. Give agents a permission
mode and allowed tools, or use `--dangerously-skip-permissions` only inside
docker mode. Cloud sessions can't bypass permissions.

### Cloud mode details

- Cloud sessions need the **Claude account login** (dashboard or
  `ctm auth login`). API keys and `setup-token` tokens only cover local and
  docker runs.
- Starting a new cloud session is interactive in the Claude CLI, so ctm drives
  it in a pseudo terminal. Before each launch it marks first-run onboarding and
  folder trust as done in its own config dir, and it never edits your
  `~/.claude.json`. Everything the CLI prints is logged as `[tty]` lines.
  If no session URL appears within 5 minutes, the run fails, and those lines
  show what the CLI was waiting for.
- The repo comes from the workspace checkout, so set `repo` on cloud agents.
  Use `environment` (`ccpool_…`) to run on a self-hosted environment, and
  put any other `claude` flags (for example `--ref <branch>`) in the extra
  args.
- A cloud run's session id is remembered. "Continue session", follow-ups and
  persistent cloud agents send later prompts to the same session with
  `claude -p --cloud <id>`.

### Claude credentials

The **Claude account** page (or `ctm auth …`) chooses what every run uses:

| Method | How | Covers |
|---|---|---|
| Claude account login | ctm runs `claude auth login` against `$CTM_DATA_DIR/claude-config`; the CLI stores and refreshes the tokens | local, docker\*, cloud |
| Long-lived token | ctm runs `claude setup-token`, or you paste the token | local, docker |
| API key | pasted | local, docker |
| Server environment (default) | whatever ctm was started with | depends |

\* Docker containers get the login's current access token as
`CLAUDE_CODE_OAUTH_TOKEN`. That token is short-lived, so for long or
infrequent docker jobs a long-lived token is more robust.

Stored keys and tokens are kept in the SQLite database, and the login lives in
`claude-config/`. Both are inside `CTM_DATA_DIR`, which ctm restricts to
mode `0700`. Treat that directory as a secret.

## Configuration

`ctm serve --help` lists every option. Each option also has an environment
variable:

| Variable | Default | |
|---|---|---|
| `CTM_BIND` | `127.0.0.1:7878` | listen address (the image uses `0.0.0.0:7878`) |
| `CTM_DATA_DIR` | `./ctm-data` | SQLite DB, logs, workspaces, managed Claude config |
| `CTM_TOKEN` | unset | password for the dashboard/API; unset or empty means no login. **Set it when exposed beyond localhost** |
| `CTM_MAX_CONCURRENT` | `4` | parallel runs |
| `CTM_CLAUDE_BIN` | `claude` | Claude CLI for local and cloud runs and sign-in |
| `CTM_CLAUDE_ARGS` | `--output-format stream-json --verbose` | base args for `-p` runs |
| `CTM_DOCKER_IMAGE` | `ctm-runner:latest` | default image for docker runs |
| `CTM_DOCKER_ARGS` | empty | extra `docker run` args (e.g. `--cpus=2 --memory=4g`) |
| `CTM_FORWARD_ENV` | `ANTHROPIC_API_KEY,CLAUDE_CODE_OAUTH_TOKEN,ANTHROPIC_BASE_URL,GH_TOKEN,GITHUB_TOKEN` | env passed into containers |
| `CTM_SCHEDULER_TICK` | `5` | seconds between scheduler checks |
| `CTM_LOG` | `info` | tracing filter |

## HTTP API

All routes are under `/api`. When `CTM_TOKEN` is set, they require
`Authorization: Bearer <token>`. Wherever a route takes `{agent}`, it accepts
the agent's id or its name.

| Method & path | |
|---|---|
| `GET /runs?limit=&job_id=&agent_id=` | list runs |
| `POST /runs` | `{prompt, mode?, repo?, model?, cloud_session?, environment?, permission_mode?, allowed_tools?, system_prompt?, extra_args?, timeout_secs?}` |
| `GET /runs/{id}` · `GET /runs/{id}/log[?follow=true]` | run / plain-text log (optionally streamed) |
| `POST /runs/{id}/cancel` · `/followup {prompt}` · `/rerun` | |
| `GET /agents` · `POST /agents` | list / create (`{name, mode, repo, …, persistent_session}`) |
| `GET/PUT/DELETE /agents/{agent}` · `POST /agents/{agent}/run {prompt}` · `/reset-session` | |
| `GET /jobs?agent_id=` · `POST /jobs` | list / create (`{name, agent_id, prompt, schedule_kind, schedule, timezone, overlap, timeout_secs, max_retries, retry_delay_secs, jitter_secs, then_job_id, enabled}`) |
| `GET/PUT/DELETE /jobs/{id}` · `POST /jobs/{id}/trigger` · `/enable` · `/disable` | |
| `POST /jobs/preview` | `{schedule_kind, schedule, timezone, count}` → description + next times |
| `GET /auth` | current Claude credential method and `claude auth status` |
| `POST /auth/flow {kind: login\|setup_token}` · `GET /auth/flow` · `POST /auth/flow/code {code}` · `DELETE /auth/flow` | delegated sign-in |
| `POST /auth/api-key {value}` · `/auth/oauth-token {value}` · `/auth/use-environment` · `/auth/sign-out` | |

## Security

- Every run executes arbitrary instructions with your Claude credentials.
  Always set `CTM_TOKEN` and put ctm behind TLS when it is reachable from a
  network.
- `CTM_DATA_DIR` holds credentials (see above).
- Mounting `/var/run/docker.sock` gives the ctm container root-equivalent
  access to the host. To isolate it, use a rootless Docker daemon or a
  dedicated VM.
- Docker-mode containers get only the selected Claude credential and the
  environment variables listed in `CTM_FORWARD_ENV`, and values never appear
  on a command line.

## Layout

```
src/main.rs          CLI entry (serve | client subcommands)
src/config.rs        server options
src/db.rs            SQLite models & queries (migrations/)
src/schedule.rs      cron / interval / one-shot schedules
src/jobs.rs          scheduler loop, overlap, templates, retries, chaining
src/executor.rs      queue, local/docker/cloud runners, log fan-out
src/claude_auth.rs   dashboard-managed Claude credentials and sign-in flows
src/pty.rs           pseudo-terminal driver for interactive claude commands
src/cli.rs           terminal client
src/web/             axum router, auth, JSON API, htmx pages, Leptos components
assets/              htmx, SSE extension, CSS (embedded in the binary)
docker/              runner image for docker-mode runs
```
