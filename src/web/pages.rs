//! HTML pages and htmx fragments for runs, agents and schedules.

use std::{convert::Infallible, time::Duration};

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use chrono::Utc;
use futures::{Stream, StreamExt};
use serde::Deserialize;

use super::{
    AppError, AppState, components as c,
    forms::{AgentForm, JobForm, PromptForm, RunForm, ScheduleForm},
};
use crate::{
    db::{Run, RunFilter},
    executor::LogEvent,
    schedule::Schedule,
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
        .route("/agents", get(agents_page).post(create_agent))
        .route(
            "/agents/{id}",
            get(agent_page).post(update_agent).delete(delete_agent),
        )
        .route("/agents/{id}/run", post(run_agent))
        .route("/agents/{id}/reset-session", post(reset_agent_session))
        .route("/jobs", get(jobs_page).post(create_job))
        .route("/jobs/preview", get(preview_schedule))
        .route(
            "/jobs/{id}",
            get(job_page).post(update_job).delete(delete_job),
        )
        .route("/jobs/{id}/toggle", post(toggle_job))
        .route("/jobs/{id}/trigger", post(trigger_job))
}

/// Tells htmx to navigate to `url`.
pub fn hx_redirect(url: String) -> Response {
    let mut resp = Html(String::new()).into_response();
    if let Ok(v) = HeaderValue::from_str(&url) {
        resp.headers_mut().insert("HX-Redirect", v);
    }
    resp
}

/// Tells htmx to reload the current page.
pub fn hx_refresh() -> Response {
    let mut resp = Html(String::new()).into_response();
    resp.headers_mut()
        .insert("HX-Refresh", HeaderValue::from_static("true"));
    resp
}

/// Renders a validation error into the form's error slot (htmx only swaps
/// 2xx responses by default, so this stays a 200).
pub fn form_error(target: &'static str, msg: impl std::fmt::Display) -> Response {
    let mut resp = c::error_box(&format!("{msg:#}")).into_response();
    resp.headers_mut()
        .insert("HX-Retarget", HeaderValue::from_static(target));
    resp.headers_mut()
        .insert("HX-Reswap", HeaderValue::from_static("innerHTML"));
    resp
}

fn not_found_page(what: &str) -> Response {
    (StatusCode::NOT_FOUND, c::not_found(what)).into_response()
}

// ---- runs ---------------------------------------------------------------

async fn dashboard(State(s): State<AppState>) -> Result<Html<String>, AppError> {
    let stats = s.db.status_counts().await?;
    let runs = s.db.list_runs(50, RunFilter::default()).await?;
    Ok(c::render_page(
        "Runs",
        c::dashboard(stats, s.exec.active_count(), runs),
    ))
}

async fn runs_fragment(
    State(s): State<AppState>,
    Query(f): Query<RunFilter>,
) -> Result<Html<String>, AppError> {
    Ok(c::render(c::run_rows(s.db.list_runs(50, f).await?)))
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
    match s.submit(spec).await {
        Ok(run) => hx_redirect(format!("/runs/{}", run.id)),
        Err(e) => form_error("#run-form-error", e.message),
    }
}

async fn load_run(s: &AppState, id: i64) -> Result<Run, AppError> {
    s.db.get_run(id).await?.ok_or(AppError::not_found("run"))
}

async fn run_page(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response, AppError> {
    Ok(match s.db.get_run(id).await? {
        Some(run) => c::render_page(&format!("Run #{id}"), c::run_detail(run)).into_response(),
        None => not_found_page("Run"),
    })
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

async fn follow_up(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<PromptForm>,
) -> Response {
    match s.follow_up(id, form.prompt).await {
        Ok(run) => hx_redirect(format!("/runs/{}", run.id)),
        Err(e) => form_error("#followup-error", e.message),
    }
}

async fn rerun(State(s): State<AppState>, Path(id): Path<i64>) -> Response {
    match s.rerun(id).await {
        Ok(run) => hx_redirect(format!("/runs/{}", run.id)),
        Err(e) => form_error("#followup-error", e.message),
    }
}

// ---- agents -------------------------------------------------------------

async fn agents_page(State(s): State<AppState>) -> Result<Html<String>, AppError> {
    Ok(c::render_page(
        "Agents",
        c::agents_page(s.db.list_agents().await?),
    ))
}

async fn create_agent(State(s): State<AppState>, Form(form): Form<AgentForm>) -> Response {
    let result = async { s.db.create_agent(form.into_new()?).await }.await;
    match result {
        Ok(a) => hx_redirect(format!("/agents/{}", a.id)),
        Err(e) => form_error("#agent-form-error", e),
    }
}

async fn agent_page(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response, AppError> {
    let Some(agent) = s.db.get_agent(id).await? else {
        return Ok(not_found_page("Agent"));
    };
    let jobs = s.db.list_jobs(Some(id)).await?;
    let runs =
        s.db.list_runs(
            50,
            RunFilter {
                agent_id: Some(id),
                ..Default::default()
            },
        )
        .await?;
    Ok(c::render_page(&agent.name.clone(), c::agent_detail(agent, jobs, runs)).into_response())
}

async fn update_agent(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<AgentForm>,
) -> Response {
    let result = async { s.db.update_agent(id, form.into_new()?).await }.await;
    match result {
        Ok(Some(_)) => hx_refresh(),
        Ok(None) => form_error("#agent-form-error", "agent not found"),
        Err(e) => form_error("#agent-form-error", e),
    }
}

async fn delete_agent(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    s.db.delete_agent(id).await?;
    Ok(Html(String::new()))
}

async fn run_agent(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<PromptForm>,
) -> Response {
    let agent = match s.db.get_agent(id).await {
        Ok(Some(a)) => a,
        Ok(None) => return form_error("#agent-run-error", "agent not found"),
        Err(e) => return form_error("#agent-run-error", e),
    };
    match s.run_agent(&agent, form.prompt).await {
        Ok(run) => hx_redirect(format!("/runs/{}", run.id)),
        Err(e) => form_error("#agent-run-error", e.message),
    }
}

async fn reset_agent_session(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    s.db.set_agent_session(id, None).await?;
    Ok(hx_refresh())
}

// ---- jobs ---------------------------------------------------------------

#[derive(Deserialize)]
struct JobsQuery {
    agent_id: Option<i64>,
}

async fn jobs_page(
    State(s): State<AppState>,
    Query(q): Query<JobsQuery>,
) -> Result<Html<String>, AppError> {
    let jobs = s.db.list_jobs(None).await?;
    let agents = s.db.list_agents().await?;
    Ok(c::render_page(
        "Schedules",
        c::jobs_page(jobs, agents, q.agent_id),
    ))
}

async fn create_job(State(s): State<AppState>, Form(form): Form<JobForm>) -> Response {
    let result = async { s.db.create_job(form.into_new()?).await }.await;
    match result {
        Ok(j) => hx_redirect(format!("/jobs/{}", j.id)),
        Err(e) => form_error("#job-form-error", e),
    }
}

async fn preview_schedule(Query(f): Query<ScheduleForm>) -> Html<String> {
    let preview = f
        .schedule_kind
        .as_deref()
        .unwrap_or("")
        .parse()
        .and_then(|kind| {
            Schedule::parse(
                kind,
                f.schedule.as_deref(),
                f.timezone.as_deref().unwrap_or("UTC"),
            )
        })
        .map(|s| (s.describe(), s.upcoming(Utc::now(), 3)))
        .map_err(|e| format!("{e:#}"));
    c::render(c::schedule_preview(preview))
}

async fn job_page(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response, AppError> {
    let Some(job) = s.db.get_job(id).await? else {
        return Ok(not_found_page("Schedule"));
    };
    let agent = s.db.get_agent(job.agent_id).await?;
    let agents = s.db.list_agents().await?;
    let jobs = s.db.list_jobs(None).await?;
    let runs =
        s.db.list_runs(
            50,
            RunFilter {
                job_id: Some(id),
                ..Default::default()
            },
        )
        .await?;
    Ok(c::render_page(
        &job.name.clone(),
        c::job_detail(job, agent, agents, jobs, runs),
    )
    .into_response())
}

async fn update_job(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<JobForm>,
) -> Response {
    let result = async { s.db.update_job(id, form.into_new()?).await }.await;
    match result {
        Ok(Some(_)) => hx_refresh(),
        Ok(None) => form_error("#job-form-error", "schedule not found"),
        Err(e) => form_error("#job-form-error", e),
    }
}

async fn delete_job(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    s.db.delete_job(id).await?;
    Ok(Html(String::new()))
}

async fn toggle_job(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let job = s.db.get_job(id).await?.ok_or(AppError::not_found("job"))?;
    let job =
        s.db.set_job_enabled(id, !job.enabled)
            .await?
            .ok_or(AppError::not_found("job"))?;
    Ok(c::render(c::job_row(job)))
}

async fn trigger_job(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response, AppError> {
    let run = s.trigger_job(id).await?;
    Ok(hx_redirect(format!("/runs/{}", run.id)))
}
