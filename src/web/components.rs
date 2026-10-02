//! Server-rendered Leptos views. Every function returns a view that is turned
//! into an HTML string with `to_html()`; htmx swaps those fragments in.

use axum::response::Html;
use chrono::{DateTime, Utc};
use leptos::prelude::*;
use serde_json::Value;

use crate::{
    claude_auth::{AuthMethod, AuthStatus, FlowKind, FlowSnapshot, FlowState},
    db::{Agent, Job, Mode, Overlap, Run, RunStatus},
    executor::{CTM_PREFIX, STDERR_PREFIX, TTY_PREFIX},
    schedule::ScheduleKind,
};

pub fn render<V: IntoView>(v: V) -> Html<String> {
    Html(v.into_view().to_html())
}

pub fn render_page<V: IntoView + 'static>(title: &str, body: V) -> Html<String> {
    let title = format!("{title} · ctm");
    let page = view! {
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <title>{title}</title>
                <link rel="stylesheet" href="/assets/app.css"/>
                <script src="/assets/htmx.min.js"></script>
                <script src="/assets/sse.min.js"></script>
                <script src="/assets/app.js" defer></script>
            </head>
            <body hx-boost="true">
                <header class="top">
                    <a class="brand" href="/">"ctm"</a>
                    <nav>
                        <a href="/">"Runs"</a>
                        <a href="/agents">"Agents"</a>
                        <a href="/jobs">"Schedules"</a>
                        <a href="/settings/claude">"Claude account"</a>
                    </nav>
                </header>
                <main>{body}</main>
            </body>
        </html>
    };
    Html(format!("<!DOCTYPE html>{}", page.to_html()))
}

pub fn fmt_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn fmt_opt_time(t: Option<DateTime<Utc>>) -> String {
    t.map(fmt_time).unwrap_or_else(|| "—".into())
}

fn fmt_duration(secs: Option<i64>) -> String {
    match secs {
        None => "—".into(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        Some(s) => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn opt(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

pub fn status_badge(status: RunStatus) -> impl IntoView {
    view! { <span class=format!("badge {}", status.as_str())>{status.as_str()}</span> }
}

pub fn login_view(error: Option<&'static str>) -> impl IntoView {
    view! {
        <section class="card narrow">
            <h1>"Log in"</h1>
            {error.map(|e| view! { <p class="error">{e}</p> })}
            <form method="post" action="/login" hx-boost="false">
                <label>"Access token" <input type="password" name="token" required=true autofocus=true/></label>
                <button type="submit">"Log in"</button>
            </form>
        </section>
    }
}

fn mode_select(selected: Mode) -> AnyView {
    view! {
        <label>"Mode"
            <select name="mode">
                <option value="local" selected=selected == Mode::Local>"local process"</option>
                <option value="docker" selected=selected == Mode::Docker>"docker container"</option>
                <option value="cloud" selected=selected == Mode::Cloud>"claude --cloud session"</option>
            </select>
        </label>
    }
    .into_any()
}

/// Inputs shared by ad-hoc runs and agents. `a` pre-fills them.
fn claude_fields(a: Option<&Agent>) -> AnyView {
    let get = |f: fn(&Agent) -> &Option<String>| a.map(|a| opt(f(a))).unwrap_or_default();
    view! {
        <div class="grid">
            {mode_select(a.map(|a| a.mode).unwrap_or_default())}
            <label>"Model" <input name="model" placeholder="default" value=get(|a| &a.model)/></label>
            <label>"Git repo (cloned into the workspace)"
                <input name="repo" placeholder="https://github.com/org/repo.git" value=get(|a| &a.repo)/></label>
            <label>"Permission mode"
                <input name="permission_mode" list="permission-modes" placeholder="default" value=get(|a| &a.permission_mode)/></label>
            <label class="wide">"Allowed tools"
                <input name="allowed_tools" placeholder="Bash(npm test) Edit Read" value=get(|a| &a.allowed_tools)/></label>
            <label class="wide">"Append to system prompt"
                <textarea name="system_prompt" rows="2" placeholder="You are the release manager for…">{get(|a| &a.system_prompt)}</textarea></label>
            <label class="wide">"Extra claude args"
                <input name="extra_args" placeholder="--max-turns 30" value=get(|a| &a.extra_args)/></label>
        </div>
        <datalist id="permission-modes">
            <option value="default"></option>
            <option value="acceptEdits"></option>
            <option value="plan"></option>
            <option value="bypassPermissions"></option>
        </datalist>
        <fieldset>
            <legend>"Docker mode"</legend>
            <label>"Image" <input name="image" placeholder="ctm-runner:latest" value=get(|a| &a.image)/></label>
        </fieldset>
        <fieldset>
            <legend>"Cloud mode"</legend>
            <p class="muted small">
                "Starts a new Claude Code cloud session for the repo, or sends the prompt to an existing session. "
                "Requires signing in with a Claude account under " <a href="/settings/claude">"Claude account"</a> "."
            </p>
            <div class="grid">
                <label>"Existing session (id or URL)"
                    <input name="cloud_session" placeholder="session_… or https://claude.ai/code/session_…" value=get(|a| &a.cloud_session)/></label>
                <label>"Self-hosted environment (new sessions)"
                    <input name="environment" placeholder="ccpool_…" value=get(|a| &a.environment)/></label>
            </div>
        </fieldset>
    }
    .into_any()
}

// ---- runs ---------------------------------------------------------------

pub fn dashboard(stats: Vec<(RunStatus, i64)>, active: usize, runs: Vec<Run>) -> impl IntoView {
    view! {
        <section class="card">
            <h1>"New run"</h1>
            <form hx-post="/runs" hx-target="#run-form-error">
                <textarea name="prompt" rows="4" required=true placeholder="What should Claude do?"></textarea>
                <details>
                    <summary>"Options"</summary>
                    {claude_fields(None)}
                    <label>"Timeout (seconds)" <input name="timeout_secs" type="number" min="1" placeholder="none"/></label>
                </details>
                <div id="run-form-error"></div>
                <button type="submit">"Run"</button>
            </form>
        </section>
        <section id="stats" hx-get="/fragments/stats" hx-trigger="every 3s" hx-swap="innerHTML">
            {stats_view(stats, active)}
        </section>
        <section class="card">
            <h2>"Runs"</h2>
            {runs_table(runs, "/fragments/runs".into())}
        </section>
    }
}

pub fn stats_view(stats: Vec<(RunStatus, i64)>, active: usize) -> impl IntoView {
    let get = |s: RunStatus| {
        stats
            .iter()
            .find(|(k, _)| *k == s)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };
    let items = [
        ("in flight", active as i64, "running"),
        ("queued", get(RunStatus::Queued), "queued"),
        ("succeeded", get(RunStatus::Succeeded), "succeeded"),
        ("in the cloud", get(RunStatus::Dispatched), "dispatched"),
        ("failed", get(RunStatus::Failed), "failed"),
        ("cancelled", get(RunStatus::Cancelled), "cancelled"),
    ];
    view! {
        <div class="stats">
            {items.into_iter().map(|(label, n, class)| view! {
                <div class=format!("stat {class}")><b>{n}</b><span>{label}</span></div>
            }).collect_view()}
        </div>
    }
}

pub fn runs_table(runs: Vec<Run>, refresh_url: String) -> impl IntoView {
    view! {
        <table class="runs">
            <thead><tr>
                <th>"#"</th><th>"Status"</th><th>"Prompt"</th><th>"Mode"</th>
                <th>"Created"</th><th>"Duration"</th><th>"Cost"</th>
            </tr></thead>
            <tbody hx-get=refresh_url hx-trigger="every 3s" hx-swap="innerHTML">
                {run_rows(runs)}
            </tbody>
        </table>
    }
}

pub fn run_rows(runs: Vec<Run>) -> impl IntoView {
    if runs.is_empty() {
        return view! { <tr><td colspan="7" class="muted">"No runs yet."</td></tr> }.into_any();
    }
    runs.into_iter()
        .map(|r| {
            let href = format!("/runs/{}", r.id);
            let duration = fmt_duration(r.duration_secs());
            view! {
                <tr>
                    <td><a href=href.clone()>{r.id}</a></td>
                    <td>{status_badge(r.status)}</td>
                    <td class="prompt"><a href=href>{truncate(&r.prompt, 90)}</a>
                        {r.agent_id.map(|a| view! { <a class="tag" href=format!("/agents/{a}")>{format!("agent {a}")}</a> })}
                        {r.job_id.map(|j| view! { <a class="tag" href=format!("/jobs/{j}")>{format!("job {j}")}</a> })}
                        {(r.attempt > 1).then(|| view! { <span class="tag">{format!("attempt {}", r.attempt)}</span> })}
                        {r.parent_run_id.map(|p| view! { <span class="tag">{format!("follow-up of #{p}")}</span> })}
                    </td>
                    <td>{r.mode.as_str()}</td>
                    <td class="nowrap">{fmt_time(r.created_at)}</td>
                    <td>{duration}</td>
                    <td>{r.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_default()}</td>
                </tr>
            }
        })
        .collect_view()
        .into_any()
}

pub fn run_detail(run: Run) -> impl IntoView {
    let id = run.id;
    // One SSE connection per page: `line` events append to the log, the
    // final `done` event closes the stream and refreshes the metadata.
    view! {
        <div hx-ext="sse" sse-connect=format!("/runs/{id}/events") sse-close="done">
            <section class="card">
                <h1>{format!("Run #{id}")}</h1>
                <pre class="prompt-full">{run.prompt.clone()}</pre>
                <div id="run-meta" hx-get=format!("/runs/{id}/meta") hx-trigger="sse:done" hx-swap="innerHTML">
                    {run_meta(run.clone())}
                </div>
            </section>
            <section class="card">
                <h2>"Output"</h2>
                <div class="log" id="log" sse-swap="line" hx-swap="beforeend"></div>
            </section>
        </div>
    }
}

pub fn run_meta(run: Run) -> impl IntoView {
    let id = run.id;
    let active = run.status.is_active();
    let can_continue = run.session_id.is_some();
    view! {
        <dl class="meta">
            <dt>"Status"</dt><dd>{status_badge(run.status)}</dd>
            <dt>"Mode"</dt><dd>{run.mode.as_str()}{run.image.map(|i| format!(" · {i}"))}</dd>
            <dt>"Workspace"</dt><dd><code>{run.workspace}</code></dd>
            {run.agent_id.map(|a| view! { <dt>"Agent"</dt><dd><a href=format!("/agents/{a}")>{format!("#{a}")}</a></dd> })}
            {run.job_id.map(|j| view! { <dt>"Job"</dt><dd><a href=format!("/jobs/{j}")>{format!("#{j}")}</a>{format!(" · attempt {}", run.attempt)}</dd> })}
            {run.parent_run_id.map(|p| view! { <dt>"Follows"</dt><dd><a href=format!("/runs/{p}")>{format!("#{p}")}</a></dd> })}
            {run.repo.map(|r| view! { <dt>"Repo"</dt><dd><code>{r}</code></dd> })}
            {run.model.map(|m| view! { <dt>"Model"</dt><dd>{m}</dd> })}
            {run.resume_session.map(|s| view! { <dt>"Resumed"</dt><dd><code>{s}</code></dd> })}
            {run.session_id.clone().map(|s| view! { <dt>"Session"</dt><dd><code>{s}</code></dd> })}
            {run.session_url.clone().map(|u| {
                let href = u.clone();
                view! { <dt>"Cloud session"</dt><dd><a href=href target="_blank" rel="noopener">{u}</a></dd> }
            })}
            <dt>"Created"</dt><dd>{fmt_time(run.created_at)}</dd>
            {run.not_before.filter(|_| run.started_at.is_none()).map(|t| view! { <dt>"Not before"</dt><dd>{fmt_time(t)}</dd> })}
            <dt>"Started"</dt><dd>{fmt_opt_time(run.started_at)}</dd>
            <dt>"Finished"</dt><dd>{fmt_opt_time(run.finished_at)}</dd>
            {run.timeout_secs.map(|t| view! { <dt>"Timeout"</dt><dd>{format!("{t}s")}</dd> })}
            {run.exit_code.map(|c| view! { <dt>"Exit code"</dt><dd>{c}</dd> })}
            {run.cost_usd.map(|c| view! { <dt>"Cost"</dt><dd>{format!("${c:.4}")}</dd> })}
            {run.error.map(|e| view! { <dt>"Error"</dt><dd class="error">{e}</dd> })}
        </dl>
        {run.result.map(|r| view! { <h3>"Result"</h3><pre class="result">{r}</pre> })}
        {if active {
            view! {
                <button class="danger" hx-post=format!("/runs/{id}/cancel") hx-target="#run-meta"
                    hx-confirm="Cancel this run?">"Cancel"</button>
            }.into_any()
        } else {
            view! {
                <form class="followup" hx-post=format!("/runs/{id}/followup") hx-target="#followup-error">
                    <textarea name="prompt" rows="2" required=true
                        placeholder="Follow up in the same session and workspace…"></textarea>
                    <div id="followup-error"></div>
                    <div class="row">
                        <button type="submit" disabled=!can_continue
                            title="Needs a session id from the run">"Continue session"</button>
                        <button type="button" class="secondary" hx-post=format!("/runs/{id}/rerun")
                            hx-target="#followup-error">"Re-run"</button>
                    </div>
                </form>
            }.into_any()
        }}
    }
}

/// Renders one line of a run log. Claude's `stream-json` events are turned
/// into readable entries; anything else is shown verbatim.
pub fn log_line(text: &str) -> String {
    for (prefix, class) in [
        (CTM_PREFIX, "ll ctm"),
        (STDERR_PREFIX, "ll stderr"),
        (TTY_PREFIX, "ll tty"),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return view! { <div class=class>{rest.to_string()}</div> }.to_html();
        }
    }
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return view! { <div class="ll raw">{text.to_string()}</div> }.to_html();
    };
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match v.get("type").and_then(Value::as_str) {
        Some("system") if s(&v, "subtype") == "init" => view! {
            <div class="ll sys">{format!("session {} · model {} · cwd {}", s(&v, "session_id"), s(&v, "model"), s(&v, "cwd"))}</div>
        }
        .to_html(),
        Some("assistant") => content_blocks(&v)
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => view! { <div class="ll text">{s(b, "text")}</div> }.to_html(),
                Some("tool_use") => {
                    let input = b.get("input").map(summarize_input).unwrap_or_default();
                    view! { <div class="ll tool"><b>{format!("→ {}", s(b, "name"))}</b>" "<code>{input}</code></div> }.to_html()
                }
                Some("thinking") => view! {
                    <details class="ll thinking"><summary>"thinking"</summary><pre>{s(b, "thinking")}</pre></details>
                }
                .to_html(),
                _ => String::new(),
            })
            .collect(),
        Some("user") => content_blocks(&v)
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .map(|b| {
                let body = tool_result_text(b);
                let class = if b.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                    "ll toolres err"
                } else {
                    "ll toolres"
                };
                view! {
                    <details class=class>
                        <summary>{format!("← {}", truncate(body.lines().next().unwrap_or(""), 120))}</summary>
                        <pre>{truncate(&body, 8000)}</pre>
                    </details>
                }
                .to_html()
            })
            .collect(),
        Some("result") => {
            let cost = v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
            let turns = v.get("num_turns").and_then(Value::as_i64).unwrap_or(0);
            let secs = v.get("duration_ms").and_then(Value::as_i64).unwrap_or(0) / 1000;
            let class = if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                "ll result err"
            } else {
                "ll result"
            };
            view! {
                <div class=class>
                    <b>{format!("■ {} · {turns} turns · {secs}s · ${cost:.4}", s(&v, "subtype"))}</b>
                    <pre>{s(&v, "result")}</pre>
                </div>
            }
            .to_html()
        }
        _ => view! { <div class="ll raw">{text.to_string()}</div> }.to_html(),
    }
}

fn content_blocks(v: &Value) -> Vec<Value> {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn summarize_input(input: &Value) -> String {
    for key in [
        "command",
        "file_path",
        "pattern",
        "url",
        "description",
        "prompt",
    ] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return truncate(s, 200);
        }
    }
    truncate(&input.to_string(), 200)
}

fn tool_result_text(b: &Value) -> String {
    match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---- agents -------------------------------------------------------------

fn agent_form(a: Option<&Agent>, action: String, submit: &'static str) -> AnyView {
    let persistent = a.is_some_and(|a| a.persistent_session);
    view! {
        <form hx-post=action hx-target="#agent-form-error">
            <div class="grid">
                <label>"Name" <input name="name" required=true placeholder="release-manager" value=a.map(|a| a.name.clone()).unwrap_or_default()/></label>
                <label>"Description" <input name="description" placeholder="What this agent is for" value=a.map(|a| opt(&a.description)).unwrap_or_default()/></label>
            </div>
            {claude_fields(a)}
            <label class="check">
                <input type="checkbox" name="persistent_session" checked=persistent/>
                " Persistent session — every run resumes the agent's previous conversation (memory across scheduled runs)"
            </label>
            <div id="agent-form-error"></div>
            <button type="submit">{submit}</button>
        </form>
    }
    .into_any()
}

pub fn agents_page(agents: Vec<Agent>) -> impl IntoView {
    view! {
        <section class="card">
            <h1>"Agents"</h1>
            <p class="muted">"An agent is a reusable Claude setup: where it runs, which repo, model, permissions and standing instructions. Schedules fire agents with prompts."</p>
            <table>
                <thead><tr><th>"Name"</th><th>"Mode"</th><th>"Repo"</th><th>"Session"</th><th></th></tr></thead>
                <tbody>
                    {if agents.is_empty() {
                        view! { <tr><td colspan="5" class="muted">"No agents yet."</td></tr> }.into_any()
                    } else {
                        agents.into_iter().map(|a| view! {
                            <tr>
                                <td><a href=format!("/agents/{}", a.id)>{a.name.clone()}</a>
                                    <div class="muted small">{opt(&a.description)}</div></td>
                                <td>{a.mode.as_str()}</td>
                                <td><code>{opt(&a.repo)}</code></td>
                                <td>{if a.persistent_session { "persistent" } else { "fresh each run" }}</td>
                                <td class="actions nowrap">
                                    <button class="danger small" hx-delete=format!("/agents/{}", a.id) hx-target="closest tr"
                                        hx-swap="outerHTML" hx-confirm="Delete this agent and its schedules? Runs are kept.">"Delete"</button>
                                </td>
                            </tr>
                        }).collect_view().into_any()
                    }}
                </tbody>
            </table>
        </section>
        <section class="card">
            <h2>"New agent"</h2>
            {agent_form(None, "/agents".into(), "Create agent")}
        </section>
    }
}

pub fn agent_detail(a: Agent, jobs: Vec<Job>, runs: Vec<Run>) -> impl IntoView {
    let id = a.id;
    view! {
        <section class="card">
            <h1>{a.name.clone()}</h1>
            {a.description.clone().map(|d| view! { <p class="muted">{d}</p> })}
            <dl class="meta">
                <dt>"Mode"</dt><dd>{a.mode.as_str()}</dd>
                <dt>"Workspace"</dt><dd><code>{a.workspace()}</code></dd>
                <dt>"Session"</dt><dd>
                    {if a.persistent_session { "persistent" } else { "fresh each run" }}
                    {a.session_id.clone().map(|s| view! {
                        " · " <code>{s}</code> " "
                        <button class="secondary small" hx-post=format!("/agents/{id}/reset-session")
                            hx-confirm="Forget this agent's session? The next run starts a new conversation.">"Reset"</button>
                    })}
                </dd>
            </dl>
            <form hx-post=format!("/agents/{id}/run") hx-target="#agent-run-error">
                <textarea name="prompt" rows="3" required=true placeholder="Ask this agent to do something now…"></textarea>
                <div id="agent-run-error"></div>
                <button type="submit">"Run agent"</button>
            </form>
        </section>
        <section class="card">
            <h2>"Schedules"</h2>
            {jobs_table(jobs)}
            <p><a href=format!("/jobs?agent_id={id}")>"Add a schedule for this agent →"</a></p>
        </section>
        <section class="card">
            <h2>"Runs"</h2>
            {runs_table(runs, format!("/fragments/runs?agent_id={id}"))}
        </section>
        <section class="card">
            <details>
                <summary>"Edit agent"</summary>
                {agent_form(Some(&a), format!("/agents/{id}"), "Save changes")}
            </details>
        </section>
    }
}

// ---- jobs ---------------------------------------------------------------

pub const TEMPLATE_HELP: &str = "{{date}} {{time}} {{datetime}} {{weekday}} {{timezone}} {{job}} {{agent}} \
{{trigger}} {{last_status}} {{last_result}} {{upstream_result}}";

fn job_form(
    j: Option<&Job>,
    agents: &[Agent],
    jobs: &[Job],
    preset_agent: Option<i64>,
    action: String,
    submit: &'static str,
) -> AnyView {
    let kind = j.map(|j| j.schedule_kind).unwrap_or(ScheduleKind::Cron);
    let agent_id = j.map(|j| j.agent_id).or(preset_agent);
    let overlap = j.map(|j| j.overlap).unwrap_or_default();
    let self_id = j.map(|j| j.id);
    let then = j.and_then(|j| j.then_job_id);
    let enabled = j.is_none_or(|j| j.enabled);
    let num = |v: Option<i64>| v.map(|n| n.to_string()).unwrap_or_default();
    view! {
        <form hx-post=action hx-target="#job-form-error">
            <div class="grid">
                <label>"Name" <input name="name" required=true placeholder="nightly-triage" value=j.map(|j| j.name.clone()).unwrap_or_default()/></label>
                <label>"Agent"
                    <select name="agent_id" required=true>
                        {agents.iter().map(|a| view! {
                            <option value=a.id.to_string() selected=Some(a.id) == agent_id>{format!("{} ({})", a.name, a.mode)}</option>
                        }).collect_view()}
                    </select>
                </label>
            </div>
            <label>"Prompt"
                <textarea name="prompt" rows="4" required=true placeholder="Summarise what changed since yesterday ({{date}})…">{j.map(|j| j.prompt.clone()).unwrap_or_default()}</textarea>
            </label>
            <p class="muted small">"Placeholders: " <code>{TEMPLATE_HELP}</code></p>
            <fieldset>
                <legend>"Schedule"</legend>
                <div class="grid" hx-get="/jobs/preview" hx-trigger="input changed delay:400ms, change, load"
                    hx-target="next .preview" hx-include="this">
                    <label>"Kind"
                        <select name="schedule_kind">
                            {[ScheduleKind::Cron, ScheduleKind::Interval, ScheduleKind::Once, ScheduleKind::Manual].into_iter().map(|k| view! {
                                <option value=k.as_str() selected=k == kind>{match k {
                                    ScheduleKind::Cron => "cron expression",
                                    ScheduleKind::Interval => "fixed interval",
                                    ScheduleKind::Once => "once at a time",
                                    ScheduleKind::Manual => "manual / chained only",
                                }}</option>
                            }).collect_view()}
                        </select>
                    </label>
                    <label>"Expression"
                        <input name="schedule" placeholder="0 9 * * 1-5 · 30m · 2026-12-01 09:00" value=j.map(|j| opt(&j.schedule)).unwrap_or_default()/></label>
                    <label>"Time zone"
                        <input name="timezone" list="timezones" value=j.map(|j| j.timezone.clone()).unwrap_or_else(|| "UTC".into())/></label>
                    <label>"Jitter (seconds)"
                        <input name="jitter_secs" type="number" min="0" max="3600" value=num(j.map(|j| j.jitter_secs))/></label>
                </div>
                <div class="preview muted small"></div>
            </fieldset>
            <fieldset>
                <legend>"Behaviour"</legend>
                <div class="grid">
                    <label>"If the previous run is still going"
                        <select name="overlap">
                            <option value="skip" selected=overlap == Overlap::Skip>"skip this slot"</option>
                            <option value="queue" selected=overlap == Overlap::Queue>"queue another run"</option>
                            <option value="replace" selected=overlap == Overlap::Replace>"cancel it and start fresh"</option>
                        </select>
                    </label>
                    <label>"Timeout (seconds)"
                        <input name="timeout_secs" type="number" min="1" placeholder="none" value=num(j.and_then(|j| j.timeout_secs))/></label>
                    <label>"Retries on failure"
                        <input name="max_retries" type="number" min="0" max="10" value=num(j.map(|j| j.max_retries).or(Some(0)))/></label>
                    <label>"First retry after (seconds, doubles each time)"
                        <input name="retry_delay_secs" type="number" min="1" value=num(j.map(|j| j.retry_delay_secs).or(Some(60)))/></label>
                    <label>"Then run (on success)"
                        <select name="then_job_id">
                            <option value="">"—"</option>
                            {jobs.iter().filter(|o| Some(o.id) != self_id).map(|o| view! {
                                <option value=o.id.to_string() selected=Some(o.id) == then>{o.name.clone()}</option>
                            }).collect_view()}
                        </select>
                    </label>
                </div>
            </fieldset>
            <label class="check"><input type="checkbox" name="enabled" checked=enabled/>" Enabled"</label>
            <div id="job-form-error"></div>
            <button type="submit">{submit}</button>
        </form>
        <datalist id="timezones">
            {["UTC", "Europe/London", "Europe/Berlin", "America/New_York", "America/Chicago", "America/Los_Angeles", "Asia/Tokyo", "Asia/Kolkata", "Australia/Sydney"]
                .into_iter().map(|tz| view! { <option value=tz></option> }).collect_view()}
        </datalist>
    }
    .into_any()
}

pub fn schedule_preview(described: Result<(String, Vec<DateTime<Utc>>), String>) -> impl IntoView {
    match described {
        Err(e) => view! { <span class="error">{e}</span> }.into_any(),
        Ok((desc, next)) => view! {
            <span>{desc}</span>
            {(!next.is_empty()).then(|| view! {
                <span>" · next: "{next.iter().map(|t| fmt_time(*t)).collect::<Vec<_>>().join(", ")}</span>
            })}
        }
        .into_any(),
    }
}

pub fn jobs_page(jobs: Vec<Job>, agents: Vec<Agent>, preset_agent: Option<i64>) -> impl IntoView {
    let has_agents = !agents.is_empty();
    view! {
        <section class="card">
            <h1>"Schedules"</h1>
            {jobs_table(jobs.clone())}
        </section>
        <section class="card">
            <h2>"New schedule"</h2>
            {if has_agents {
                job_form(None, &agents, &jobs, preset_agent, "/jobs".into(), "Create schedule").into_any()
            } else {
                view! { <p>"Create an " <a href="/agents">"agent"</a> " first; schedules fire agents."</p> }.into_any()
            }}
        </section>
    }
}

pub fn jobs_table(jobs: Vec<Job>) -> impl IntoView {
    view! {
        <table class="jobs">
            <thead><tr>
                <th>"Name"</th><th>"Schedule"</th><th>"Next run"</th><th>"Last"</th><th>"On"</th><th></th>
            </tr></thead>
            <tbody>
                {if jobs.is_empty() {
                    view! { <tr><td colspan="6" class="muted">"No schedules yet."</td></tr> }.into_any()
                } else {
                    jobs.into_iter().map(job_row).collect_view().into_any()
                }}
            </tbody>
        </table>
    }
}

pub fn job_row(j: Job) -> impl IntoView {
    let id = j.id;
    let schedule = j
        .parsed_schedule()
        .map(|s| s.describe())
        .unwrap_or_else(|e| format!("invalid: {e}"));
    view! {
        <tr>
            <td><a href=format!("/jobs/{id}")>{j.name}</a>
                <div class="muted small">{truncate(&j.prompt, 70)}</div></td>
            <td><code>{opt(&j.schedule)}</code><div class="muted small">{schedule}</div></td>
            <td class="nowrap">{fmt_opt_time(j.next_run_at)}</td>
            <td class="nowrap">{j.last_status.map(status_badge)}<div class="muted small">{fmt_opt_time(j.last_run_at)}</div></td>
            <td>
                <button class="secondary small" hx-post=format!("/jobs/{id}/toggle") hx-target="closest tr" hx-swap="outerHTML">
                    {if j.enabled { "on" } else { "off" }}
                </button>
            </td>
            <td class="actions nowrap">
                <button class="small" hx-post=format!("/jobs/{id}/trigger")>"Run now"</button>
                <button class="danger small" hx-delete=format!("/jobs/{id}") hx-target="closest tr"
                    hx-swap="outerHTML" hx-confirm="Delete this schedule? Its runs are kept.">"Delete"</button>
            </td>
        </tr>
    }
}

pub fn job_detail(
    j: Job,
    agent: Option<Agent>,
    agents: Vec<Agent>,
    jobs: Vec<Job>,
    runs: Vec<Run>,
) -> impl IntoView {
    let id = j.id;
    let schedule = j
        .parsed_schedule()
        .map(|s| s.describe())
        .unwrap_or_else(|e| e.to_string());
    let upcoming = j
        .parsed_schedule()
        .map(|s| {
            if j.enabled {
                s.upcoming(Utc::now(), 5)
            } else {
                vec![]
            }
        })
        .unwrap_or_default();
    let upcoming = if upcoming.is_empty() {
        "—".to_string()
    } else {
        upcoming
            .iter()
            .map(|t| fmt_time(*t))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let then = j
        .then_job_id
        .and_then(|t| jobs.iter().find(|o| o.id == t).map(|o| (t, o.name.clone())));
    view! {
        <section class="card">
            <h1>{j.name.clone()}</h1>
            <pre class="prompt-full">{j.prompt.clone()}</pre>
            <dl class="meta">
                <dt>"Agent"</dt><dd>{agent.map(|a| view! { <a href=format!("/agents/{}", a.id)>{a.name}</a> })}</dd>
                <dt>"Schedule"</dt><dd>{schedule}</dd>
                <dt>"Enabled"</dt><dd>{if j.enabled { "yes" } else { "no" }}</dd>
                <dt>"Next runs"</dt><dd>{upcoming}</dd>
                <dt>"Overlap"</dt><dd>{j.overlap.as_str()}</dd>
                <dt>"Timeout"</dt><dd>{j.timeout_secs.map(|t| format!("{t}s")).unwrap_or_else(|| "none".into())}</dd>
                <dt>"Retries"</dt><dd>{format!("{} (first after {}s, doubling)", j.max_retries, j.retry_delay_secs)}</dd>
                {then.map(|(t, name)| view! { <dt>"Then"</dt><dd><a href=format!("/jobs/{t}")>{name}</a></dd> })}
                <dt>"Last run"</dt><dd>{fmt_opt_time(j.last_run_at)}</dd>
            </dl>
            <button hx-post=format!("/jobs/{id}/trigger")>"Run now"</button>
        </section>
        <section class="card">
            <h2>"Runs"</h2>
            {runs_table(runs, format!("/fragments/runs?job_id={id}"))}
        </section>
        <section class="card">
            <details>
                <summary>"Edit schedule"</summary>
                {job_form(Some(&j), &agents, &jobs, None, format!("/jobs/{id}"), "Save changes")}
            </details>
        </section>
    }
}

// ---- Claude account -----------------------------------------------------

pub fn claude_page(status: AuthStatus) -> impl IntoView {
    let method = status.method;
    let cli_rows: Vec<(String, String)> = status
        .cli
        .as_ref()
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !k.ends_with("Directory") && k.as_str() != "loggedIn")
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some((k.clone(), s.clone())),
                    Value::Bool(b) => Some((k.clone(), b.to_string())),
                    Value::Number(n) => Some((k.clone(), n.to_string())),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let logged_in = status
        .cli
        .as_ref()
        .and_then(|v| v.get("loggedIn"))
        .and_then(Value::as_bool);
    view! {
        <section class="card">
            <h1>"Claude account"</h1>
            <p class="muted">"Credentials every run uses. Sign in here once and local, docker and cloud runs all pick it up — nothing to export on the server."</p>
            <dl class="meta">
                <dt>"Method"</dt><dd><b>{status.method_label}</b></dd>
                <dt>"Credential"</dt><dd><code>{status.credential.unwrap_or_else(|| "none".into())}</code></dd>
                <dt>"CLI check"</dt><dd>{match (logged_in, status.cli_error) {
                    (Some(true), _) => view! { <span class="badge succeeded">"logged in"</span> }.into_any(),
                    (Some(false), _) => view! { <span class="badge failed">"not logged in"</span> }.into_any(),
                    (None, Some(e)) => view! { <span class="error">{e}</span> }.into_any(),
                    (None, None) => view! { <span class="muted">"—"</span> }.into_any(),
                }}</dd>
                {cli_rows.into_iter().map(|(k, v)| view! { <dt class="small">{k}</dt><dd class="small"><code>{v}</code></dd> }).collect_view()}
            </dl>
            {(method != AuthMethod::Login).then(|| view! {
                <p class="muted small">"Cloud sessions need the Claude account login below; API keys and long-lived tokens only cover local and docker runs."</p>
            })}
        </section>
        <section class="card">
            <h2>"Sign in"</h2>
            <div id="auth-flow">{flow_view(status.flow)}</div>
            <div class="row">
                <button hx-post="/settings/claude/flow?kind=login" hx-target="#auth-flow" hx-disabled-elt="this">
                    "Sign in with Claude account"
                </button>
                <button class="secondary" hx-post="/settings/claude/flow?kind=setup_token" hx-target="#auth-flow" hx-disabled-elt="this">
                    "Generate long-lived token"
                </button>
            </div>
            <p class="muted small">"ctm runs the official " <code>"claude auth login"</code> " / " <code>"claude setup-token"</code>
                " for you: open the link, approve, and paste the code you're shown back here."</p>
        </section>
        <section class="card">
            <h2>"Or paste a credential"</h2>
            <div class="grid">
                <form hx-post="/settings/claude/api-key" hx-target="#cred-error">
                    <label>"Anthropic API key" <input type="password" name="value" placeholder="sk-ant-api…" required=true autocomplete="off"/></label>
                    <button type="submit">"Use API key"</button>
                </form>
                <form hx-post="/settings/claude/oauth-token" hx-target="#cred-error">
                    <label>"OAuth token (from claude setup-token)" <input type="password" name="value" placeholder="sk-ant-oat…" required=true autocomplete="off"/></label>
                    <button type="submit">"Use token"</button>
                </form>
            </div>
            <div id="cred-error"></div>
        </section>
        <section class="card">
            <h2>"Reset"</h2>
            <div class="row">
                <button class="secondary" hx-post="/settings/claude/use-environment" disabled=method == AuthMethod::Environment>
                    "Use the server's environment instead"
                </button>
                <button class="danger" hx-post="/settings/claude/sign-out" hx-confirm="Forget all stored Claude credentials and log the managed login out?">
                    "Sign out & forget credentials"
                </button>
            </div>
        </section>
    }
}

/// The sign-in panel. While the CLI is working it polls itself.
pub fn flow_view(flow: Option<FlowSnapshot>) -> impl IntoView {
    let Some(flow) = flow else {
        return view! { <p class="muted">"No sign-in in progress."</p> }.into_any();
    };
    let what = match flow.kind {
        FlowKind::Login => "Claude account login",
        FlowKind::SetupToken => "Long-lived token",
    };
    let poll = |inner: AnyView| {
        view! { <div hx-get="/settings/claude/flow" hx-trigger="every 1s" hx-target="#auth-flow">{inner}</div> }.into_any()
    };
    match flow.state {
        FlowState::Starting => poll(view! { <p>{what}": starting the Claude CLI…"</p> }.into_any()),
        FlowState::AwaitingCode { url } => view! {
            <ol class="steps">
                <li>"Open " <a href=url.clone() target="_blank" rel="noopener">"the Claude sign-in page"</a> " and approve access."
                    <details><summary class="small">"show link"</summary><code class="small wrap">{url}</code></details></li>
                <li>"Paste the code it shows you:"
                    <form hx-post="/settings/claude/flow/code" hx-target="#auth-flow" class="inline">
                        <input name="code" required=true autocomplete="off" placeholder="code#state"/>
                        <button type="submit">"Finish sign-in"</button>
                    </form>
                </li>
            </ol>
            <button class="secondary small" hx-delete="/settings/claude/flow" hx-target="#auth-flow">"Cancel"</button>
        }
        .into_any(),
        FlowState::Verifying => poll(view! { <p>{what}": verifying the code…"</p> }.into_any()),
        FlowState::Succeeded { message } => view! { <p class="ok">"✓ " {message}</p> }.into_any(),
        FlowState::Failed { message } => view! { <p class="error">{what}" failed: "{message}</p> }.into_any(),
    }
}

pub fn error_box(msg: &str) -> Html<String> {
    render(view! { <p class="error">{msg.to_string()}</p> })
}

pub fn not_found(what: &str) -> Html<String> {
    render_page(
        "Not found",
        view! { <section class="card"><h1>{format!("{what} not found")}</h1><a href="/">"Back"</a></section> },
    )
}
