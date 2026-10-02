//! HTML pages and htmx fragments.

use std::{convert::Infallible, time::Duration};

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::HeaderValue,
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::{Stream, StreamExt};
use serde::Deserialize;

use super::{AppError, AppState, components as c};
use crate::{
    db::{Mode, NewTask, Run, RunSpec},
    executor::LogEvent,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/fragments/runs", get(runs_fragment))
        .route("/fragments/stats", get(stats_fragment))
        .route("/runs", post(create_run))
        .route("/runs/{id}", get(run_page))
        .route("/runs/{id}/meta", get(run_meta))
        .route("/runs/{id}/events", get(run_events))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/runs/{id}/followup", post(follow_up))
        .route("/runs/{id}/rerun", post(rerun))
        .route("/tasks", get(tasks_page).post(create_task))
        .route("/tasks/{id}", get(task_page).delete(delete_task))
        .route("/tasks/{id}/toggle", post(toggle_task))
        .route("/tasks/{id}/trigger", post(trigger_task))
}

/// Tells htmx to navigate to `url`.
fn hx_redirect(url: String) -> Response {
    let mut resp = Html(String::new()).into_response();
    if let Ok(v) = HeaderValue::from_str(&url) {
        resp.headers_mut().insert("HX-Redirect", v);
    }
    resp
}

/// Renders a validation error into the given target instead of the form's
/// normal swap target (htmx only swaps 2xx responses by default).
fn form_error(target: &'static str, msg: impl std::fmt::Display) -> Response {
    let mut resp = c::error_box(&msg.to_string()).into_response();
    resp.headers_mut()
        .insert("HX-Retarget", HeaderValue::from_static(target));
    resp.headers_mut()
        .insert("HX-Reswap", HeaderValue::from_static("innerHTML"));
    resp
}

#[derive(Deserialize)]
struct RunForm {
    prompt: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    extra_args: Option<String>,
}

impl RunForm {
    fn into_spec(self) -> anyhow::Result<RunSpec> {
        Ok(RunSpec {
            prompt: self.prompt,
            mode: match self.mode.as_deref() {
                None | Some("") => Mode::Local,
                Some(m) => m.parse()?,
            },
            image: self.image,
            repo: self.repo,
            model: self.model,
            extra_args: self.extra_args,
        })
    }
}

async fn dashboard(State(s): State<AppState>) -> Result<Html<String>, AppError> {
    let stats = s.db.status_counts().await?;
    let runs = s.db.list_runs(50, None).await?;
    Ok(c::render_page(
        "Runs",
        c::dashboard(stats, s.exec.active_count(), runs),
    ))
}

#[derive(Deserialize)]
struct RunsQuery {
    task_id: Option<i64>,
}

async fn runs_fragment(
    State(s): State<AppState>,
    Query(q): Query<RunsQuery>,
) -> Result<Html<String>, AppError> {
    let runs = s.db.list_runs(50, q.task_id).await?;
    Ok(c::render(c::run_rows(runs)))
}

async fn stats_fragment(State(s): State<AppState>) -> Result<Html<String>, AppError> {
    let stats = s.db.status_counts().await?;
    Ok(c::render(c::stats_view(stats, s.exec.active_count())))
}

async fn create_run(State(s): State<AppState>, Form(form): Form<RunForm>) -> Response {
    let spec = match form.into_spec() {
        Ok(spec) => spec,
        Err(e) => return form_error("#run-form-error", e),
    };
    match s.submit(spec, None).await {
        Ok(run) => hx_redirect(format!("/runs/{}", run.id)),
        Err(e) => form_error("#run-form-error", e),
    }
}

async fn load_run(s: &AppState, id: i64) -> Result<Run, AppError> {
    s.db.get_run(id).await?.ok_or(AppError::not_found("run"))
}

async fn run_page(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response, AppError> {
    match s.db.get_run(id).await? {
        Some(run) => Ok(c::render_page(&format!("Run #{id}"), c::run_detail(run)).into_response()),
        None => Ok((axum::http::StatusCode::NOT_FOUND, c::not_found("Run")).into_response()),
    }
}

async fn run_meta(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    Ok(c::render(c::run_meta(load_run(&s, id).await?)))
}

/// Server-sent events for the run page: rendered log lines, then `done`.
async fn run_events(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let run = load_run(&s, id).await?;
    let stream = s.exec.follow(run.id).await?.map(|ev| {
        Ok(match ev {
            LogEvent::Line { text, .. } => Event::default().event("line").data(c::log_line(&text)),
            LogEvent::Done(status) => Event::default().event("done").data(status.as_str()),
        })
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn cancel_run(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    s.exec.cancel(id).await?;
    // Give the process a moment to die so the refreshed meta is accurate.
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(c::render(c::run_meta(load_run(&s, id).await?)))
}

#[derive(Deserialize)]
struct FollowUpForm {
    prompt: String,
}

async fn follow_up(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<FollowUpForm>,
) -> Response {
    match s.follow_up(id, form.prompt).await {
        Ok(run) => hx_redirect(format!("/runs/{}", run.id)),
        Err(e) => form_error("#followup-error", e.message),
    }
}

async fn rerun(State(s): State<AppState>, Path(id): Path<i64>) -> Response {
    let run = match load_run(&s, id).await {
        Ok(r) => r,
        Err(e) => return form_error("#followup-error", e.message),
    };
    let spec = RunSpec {
        prompt: run.prompt,
        mode: run.mode,
        image: run.image,
        repo: run.repo,
        model: run.model,
        extra_args: run.extra_args,
    };
    match s.submit(spec, run.task_id).await {
        Ok(run) => hx_redirect(format!("/runs/{}", run.id)),
        Err(e) => form_error("#followup-error", e),
    }
}

async fn tasks_page(State(s): State<AppState>) -> Result<Html<String>, AppError> {
    Ok(c::render_page(
        "Scheduled tasks",
        c::tasks_page(s.db.list_tasks().await?),
    ))
}

#[derive(Deserialize)]
struct TaskForm {
    name: String,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    enabled: Option<String>,
    #[serde(flatten)]
    run: RunForm,
}

async fn create_task(State(s): State<AppState>, Form(form): Form<TaskForm>) -> Response {
    let spec = match form.run.into_spec() {
        Ok(spec) => spec,
        Err(e) => return form_error("#task-form-error", e),
    };
    let new = NewTask {
        name: form.name,
        cron: form.cron,
        enabled: form.enabled.is_some(),
        spec,
    };
    if let Err(e) = s.db.create_task(new).await {
        return form_error("#task-form-error", e);
    }
    match s.db.list_tasks().await {
        Ok(tasks) => c::render(c::task_rows(tasks)).into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}

async fn task_page(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response, AppError> {
    let Some(task) = s.db.get_task(id).await? else {
        return Ok((axum::http::StatusCode::NOT_FOUND, c::not_found("Task")).into_response());
    };
    let runs = s.db.list_runs(50, Some(id)).await?;
    Ok(c::render_page(&task.name.clone(), c::task_detail(task, runs)).into_response())
}

async fn delete_task(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    s.db.delete_task(id).await?;
    Ok(Html(String::new()))
}

async fn toggle_task(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let task =
        s.db.get_task(id)
            .await?
            .ok_or(AppError::not_found("task"))?;
    let task =
        s.db.set_task_enabled(id, !task.enabled)
            .await?
            .ok_or(AppError::not_found("task"))?;
    Ok(c::render(c::task_row(task)))
}

async fn trigger_task(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let run = s.trigger_task(id).await?;
    Ok(hx_redirect(format!("/runs/{}", run.id)))
}
