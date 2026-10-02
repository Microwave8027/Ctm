# ctm — Claude multiplexer

`ctm` runs many headless [Claude Code](https://docs.claude.com/en/docs/claude-code)
sessions side by side. It has a web dashboard, a JSON API, a cron scheduler
and a terminal client, and it can run each job as a local process or inside a
throw-away Docker container.

- **Backend:** Rust, [axum](https://github.com/tokio-rs/axum), SQLite (sqlx), tokio
- **UI:** server-rendered [Leptos](https://leptos.dev) components plus
  [htmx](https://htmx.org). Live logs use htmx's SSE extension. No JS
  build step: htmx is vendored in `assets/` and compiled into the binary.

## Features

- Submit prompts from the dashboard or the terminal (`ctm run "…"`).
- Up to `CTM_MAX_CONCURRENT` runs at once. Extra runs wait in a FIFO queue.
- Live output streamed over SSE. Claude's `stream-json` events are shown as
  text, tool calls, tool results and a final cost summary.
- **Continue session:** a follow-up runs `claude --resume <session>` in the
  same workspace.
- **Scheduled tasks:** cron (UTC, 5 or 6 fields), enable/disable, run now. A
  slot is skipped if the previous run is still active, and slots missed while
  ctm was down collapse into a single run.
- **Docker mode:** each run gets its own container (`ctm-run-<id>`), a
  per-workspace volume, and an optional `git clone` on first use.
- Cancel queued or running jobs. In docker mode the container is killed too.
- Optional shared-token auth: a bearer token for the API, a cookie for the
  browser.

## Quick start (local)

```sh
cargo build --release
export ANTHROPIC_API_KEY=sk-ant-...      # or CLAUDE_CODE_OAUTH_TOKEN
./target/release/ctm serve               # http://127.0.0.1:7878
```

`claude` must be on `PATH` for local runs (`npm i -g @anthropic-ai/claude-code`).

## Quick start (Docker)

```sh
cp .env.example .env     # set CTM_TOKEN and ANTHROPIC_API_KEY
docker compose build     # builds ctm:latest and ctm-runner:latest
docker compose up -d ctm
```

Open http://localhost:7878 and log in with `CTM_TOKEN`. The compose file
mounts the host's Docker socket so the server can start sibling
`ctm-runner` containers for docker-mode runs.

## Terminal client

The same binary is the client. Point it at a server with `CTM_URL` and
`CTM_TOKEN`, or with `--url` and `--token`.

```sh
export CTM_URL=https://ctm.example.com CTM_TOKEN=...

ctm run "fix the flaky test in tests/api.rs" --repo https://github.com/me/app.git --docker -f
echo "summarize open TODOs" | ctm run --model sonnet      # prompt from stdin; prints run id
ctm ps                                                    # recent runs
ctm logs 42 -f                                            # stream output; exit code reflects result
ctm resume 42 -f "now open a PR"                          # continue the session
ctm cancel 43

ctm task add --name nightly-deps --cron "0 3 * * *" --docker \
    --repo https://github.com/me/app.git "update dependencies and run the tests"
ctm task ls
ctm task trigger 1 -f
ctm task disable 1
```

When run with `-f`, `ctm run`, `ctm resume`, `ctm logs` and `ctm task trigger`
exit non-zero unless the run succeeds, so you can use them in scripts and CI.

## How runs execute

The prompt is written to `claude -p` on **stdin**. Arguments are, in order:
`CTM_CLAUDE_ARGS` (default `--output-format stream-json --verbose`),
`--model`, `--resume <session>` for follow-ups, then the run's extra args.

| | local | docker |
|---|---|---|
| process | `claude` in `$CTM_DATA_DIR/workspaces/<ws>` | `docker run --rm -i --name ctm-run-<id> <image> claude …` |
| workspace | directory, cloned from `repo` if empty | volume `ctm-ws-<ws>` at `/workspace`, cloned by the entrypoint |
| session state | `~/.claude` of the ctm user | shared volume `ctm-claude-home` |
| env | inherits ctm's environment | only the variables listed in `CTM_FORWARD_ENV` |

The workspace key `<ws>` is `task-<id>` for scheduled tasks, so a task keeps
its checkout between runs. Ad-hoc runs use `run-<id>`, and follow-ups reuse
their parent's workspace.

Headless Claude can't answer permission prompts. Pass a permission policy in
extra args or `CTM_CLAUDE_ARGS`. For example, use
`--permission-mode acceptEdits --allowedTools "Bash(npm test)"`, or
`--dangerously-skip-permissions` only inside docker mode.

Docker volumes are not removed automatically. Clean them up with
`docker volume ls -q --filter name=ctm-ws- | xargs docker volume rm`.

## Configuration

`ctm serve --help` lists every option. Each option also has an environment
variable:

| Variable | Default | |
|---|---|---|
| `CTM_BIND` | `127.0.0.1:7878` | listen address (the image uses `0.0.0.0:7878`) |
| `CTM_DATA_DIR` | `./ctm-data` | SQLite DB, logs, local workspaces |
| `CTM_TOKEN` | unset | shared secret; **set this when exposed beyond localhost** |
| `CTM_MAX_CONCURRENT` | `4` | parallel runs |
| `CTM_CLAUDE_BIN` | `claude` | binary for local runs |
| `CTM_CLAUDE_ARGS` | `--output-format stream-json --verbose` | base args |
| `CTM_DOCKER_IMAGE` | `ctm-runner:latest` | default image for docker runs |
| `CTM_DOCKER_ARGS` | empty | extra `docker run` args (e.g. `--cpus=2 --memory=4g`) |
| `CTM_FORWARD_ENV` | `ANTHROPIC_API_KEY,CLAUDE_CODE_OAUTH_TOKEN,ANTHROPIC_BASE_URL,GH_TOKEN,GITHUB_TOKEN` | env passed into containers |
| `CTM_SCHEDULER_TICK` | `5` | seconds between scheduler checks |
| `CTM_LOG` | `info` | tracing filter |

## HTTP API

All routes are under `/api`. When `CTM_TOKEN` is set, they require
`Authorization: Bearer <token>`.

| Method & path | |
|---|---|
| `GET /runs?limit=&task_id=` | list runs |
| `POST /runs` | `{prompt, mode?, image?, repo?, model?, extra_args?}` → run |
| `GET /runs/{id}` | run |
| `GET /runs/{id}/log[?follow=true]` | plain-text log, optionally streamed |
| `POST /runs/{id}/cancel` | `{cancelled}` |
| `POST /runs/{id}/followup` | `{prompt}` → new run resuming the session |
| `GET /tasks` · `POST /tasks` | list / create (`{name, cron?, enabled?, prompt, mode?, …}`) |
| `GET /tasks/{id}` · `DELETE /tasks/{id}` | get / delete |
| `POST /tasks/{id}/trigger` | queue a run now |
| `POST /tasks/{id}/enable` · `/disable` | toggle schedule |

## Security

- Every run executes arbitrary instructions with your Anthropic credentials.
  Always set `CTM_TOKEN` and put ctm behind TLS when it is reachable from a
  network.
- Mounting `/var/run/docker.sock` gives the ctm container root-equivalent
  access to the host. To isolate it, use a rootless Docker daemon or a
  dedicated VM.
- Docker-mode containers only get the environment variables listed in
  `CTM_FORWARD_ENV`.

## Layout

```
src/main.rs         CLI entry (serve | client subcommands)
src/config.rs       server options
src/db.rs           SQLite models & queries (migrations/)
src/executor.rs     queue, process/container runner, log fan-out
src/scheduler.rs    cron loop
src/cli.rs          terminal client
src/web/            axum router, auth, JSON API, htmx pages, Leptos components
assets/             htmx, SSE extension, CSS (embedded in the binary)
docker/             runner image for docker-mode runs
```
