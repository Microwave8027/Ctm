//! Server-rendered Leptos views. Every function returns a view that is turned
//! into an HTML string with `to_html()`; htmx swaps those fragments in.

use axum::response::Html;
use chrono::{DateTime, Utc};
use leptos::prelude::*;
use serde_json::Value;

use crate::{
    db::{Run, RunStatus, Task},
    executor::{CTM_PREFIX, STDERR_PREFIX},
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
                        <a href="/tasks">"Scheduled tasks"</a>
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

// ---- runs ---------------------------------------------------------------

fn mode_fields(prefix_model: Option<String>) -> impl IntoView {
    view! {
        <div class="grid">
            <label>"Mode"
                <select name="mode">
                    <option value="local">"local process"</option>
                    <option value="docker">"docker container"</option>
                </select>
            </label>
            <label>"Model" <input name="model" placeholder="default" value=prefix_model.unwrap_or_default()/></label>
            <label>"Git repo (cloned into workspace)" <input name="repo" placeholder="https://github.com/org/repo.git"/></label>
            <label>"Docker image" <input name="image" placeholder="ctm-runner:latest"/></label>
            <label class="wide">"Extra claude args" <input name="extra_args" placeholder="--max-turns 30 --permission-mode acceptEdits"/></label>
        </div>
    }
}

pub fn dashboard(stats: Vec<(RunStatus, i64)>, active: usize, runs: Vec<Run>) -> impl IntoView {
    view! {
        <section class="card">
            <h1>"New run"</h1>
            <form hx-post="/runs" hx-target="#run-form-error">
                <textarea name="prompt" rows="4" required=true placeholder="What should Claude do?"></textarea>
                <details>
                    <summary>"Options"</summary>
                    {mode_fields(None)}
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
            {runs_table(runs, None)}
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

pub fn runs_table(runs: Vec<Run>, task_id: Option<i64>) -> impl IntoView {
    let url = match task_id {
        Some(t) => format!("/fragments/runs?task_id={t}"),
        None => "/fragments/runs".to_string(),
    };
    view! {
        <table class="runs">
            <thead><tr>
                <th>"#"</th><th>"Status"</th><th>"Prompt"</th><th>"Mode"</th>
                <th>"Created"</th><th>"Duration"</th><th>"Cost"</th>
            </tr></thead>
            <tbody hx-get=url hx-trigger="every 3s" hx-swap="innerHTML">
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
                        {r.task_id.map(|t| view! { <a class="tag" href=format!("/tasks/{t}")>{format!("task {t}")}</a> })}
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
    view! {
        <dl class="meta">
            <dt>"Status"</dt><dd>{status_badge(run.status)}</dd>
            <dt>"Mode"</dt><dd>{run.mode.as_str()}{run.image.map(|i| format!(" · {i}"))}</dd>
            <dt>"Workspace"</dt><dd><code>{run.workspace}</code></dd>
            {run.repo.map(|r| view! { <dt>"Repo"</dt><dd><code>{r}</code></dd> })}
            {run.model.map(|m| view! { <dt>"Model"</dt><dd>{m}</dd> })}
            {run.task_id.map(|t| view! { <dt>"Task"</dt><dd><a href=format!("/tasks/{t}")>{format!("#{t}")}</a></dd> })}
            {run.parent_run_id.map(|p| view! { <dt>"Follows"</dt><dd><a href=format!("/runs/{p}")>{format!("#{p}")}</a></dd> })}
            {run.session_id.clone().map(|s| view! { <dt>"Session"</dt><dd><code>{s}</code></dd> })}
            <dt>"Created"</dt><dd>{fmt_time(run.created_at)}</dd>
            <dt>"Started"</dt><dd>{fmt_opt_time(run.started_at)}</dd>
            <dt>"Finished"</dt><dd>{fmt_opt_time(run.finished_at)}</dd>
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
                        <button type="submit" disabled=run.session_id.is_none()
                        title="Needs a session id from stream-json output">"Continue session"</button>
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
    if let Some(rest) = text.strip_prefix(CTM_PREFIX) {
        return view! { <div class="ll ctm">{rest.to_string()}</div> }.to_html();
    }
    if let Some(rest) = text.strip_prefix(STDERR_PREFIX) {
        return view! { <div class="ll stderr">{rest.to_string()}</div> }.to_html();
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

// ---- tasks --------------------------------------------------------------

pub fn tasks_page(tasks: Vec<Task>) -> impl IntoView {
    view! {
        <section class="card">
            <h1>"New scheduled task"</h1>
            <form hx-post="/tasks" hx-target="#task-rows" hx-swap="innerHTML" hx-on--after-request="if(event.detail.successful && !event.detail.xhr.getResponseHeader('HX-Retarget')) this.reset()">
                <div class="grid">
                    <label>"Name" <input name="name" required=true placeholder="nightly-deps"/></label>
                    <label>"Cron (UTC, 5 or 6 fields)" <input name="cron" placeholder="0 3 * * *  (blank = manual only)"/></label>
                </div>
                <textarea name="prompt" rows="3" required=true placeholder="Prompt to run on every fire"></textarea>
                {mode_fields(None)}
                <label class="check"><input type="checkbox" name="enabled" checked=true/>" Enabled"</label>
                <div id="task-form-error"></div>
                <button type="submit">"Create task"</button>
            </form>
        </section>
        <section class="card">
            <h2>"Tasks"</h2>
            <table class="tasks">
                <thead><tr>
                    <th>"Name"</th><th>"Schedule"</th><th>"Mode"</th><th>"Next run"</th>
                    <th>"Last run"</th><th>"Enabled"</th><th></th>
                </tr></thead>
                <tbody id="task-rows">{task_rows(tasks)}</tbody>
            </table>
        </section>
    }
}

pub fn task_rows(tasks: Vec<Task>) -> impl IntoView {
    if tasks.is_empty() {
        return view! { <tr><td colspan="7" class="muted">"No tasks yet."</td></tr> }.into_any();
    }
    tasks.into_iter().map(task_row).collect_view().into_any()
}

pub fn task_row(t: Task) -> impl IntoView {
    let id = t.id;
    view! {
        <tr>
            <td><a href=format!("/tasks/{id}")>{t.name}</a><div class="muted small">{truncate(&t.prompt, 80)}</div></td>
            <td><code>{t.cron.unwrap_or_else(|| "manual".into())}</code></td>
            <td>{t.mode.as_str()}</td>
            <td class="nowrap">{fmt_opt_time(t.next_run_at)}</td>
            <td class="nowrap">{fmt_opt_time(t.last_run_at)}</td>
            <td>
                <button class="secondary small" hx-post=format!("/tasks/{id}/toggle")
                    hx-target="closest tr" hx-swap="outerHTML">
                    {if t.enabled { "on" } else { "off" }}
                </button>
            </td>
            <td class="actions nowrap">
                <button class="small" hx-post=format!("/tasks/{id}/trigger")>"Run now"</button>
                <button class="danger small" hx-delete=format!("/tasks/{id}") hx-target="closest tr"
                    hx-swap="outerHTML" hx-confirm="Delete this task? Its runs are kept.">"Delete"</button>
            </td>
        </tr>
    }
}

pub fn task_detail(t: Task, runs: Vec<Run>) -> impl IntoView {
    let id = t.id;
    view! {
        <section class="card">
            <h1>{t.name.clone()}</h1>
            <pre class="prompt-full">{t.prompt.clone()}</pre>
            <dl class="meta">
                <dt>"Schedule"</dt><dd><code>{t.cron.clone().unwrap_or_else(|| "manual".into())}</code></dd>
                <dt>"Enabled"</dt><dd>{if t.enabled { "yes" } else { "no" }}</dd>
                <dt>"Mode"</dt><dd>{t.mode.as_str()}{t.image.clone().map(|i| format!(" · {i}"))}</dd>
                {t.repo.clone().map(|r| view! { <dt>"Repo"</dt><dd><code>{r}</code></dd> })}
                {t.model.clone().map(|m| view! { <dt>"Model"</dt><dd>{m}</dd> })}
                {t.extra_args.clone().map(|a| view! { <dt>"Extra args"</dt><dd><code>{a}</code></dd> })}
                <dt>"Next run"</dt><dd>{fmt_opt_time(t.next_run_at)}</dd>
                <dt>"Last run"</dt><dd>{fmt_opt_time(t.last_run_at)}</dd>
            </dl>
            <button hx-post=format!("/tasks/{id}/trigger")>"Run now"</button>
        </section>
        <section class="card">
            <h2>"Runs"</h2>
            {runs_table(runs, Some(id))}
        </section>
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
