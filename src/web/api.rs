//! JSON API used by the `ctm` CLI (and any other headless client).

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::{AppError, AppState};
use crate::{
    claude_auth::{AuthStatus, FlowKind, FlowSnapshot},
    db::{Agent, Job, NewAgent, NewJob, Run, RunFilter, RunSpec},
    executor::LogEvent,
    schedule::{Schedule, ScheduleKind},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/runs/{id}/followup", post(follow_up))
        .route("/runs/{id}/rerun", post(rerun))
        .route("/runs/{id}/log", get(run_log))
        .route("/agents", get(list_agents).post(create_agent))
        .route(
            "/agents/{agent}",
            get(get_agent).put(update_agent).delete(delete_agent),
        )
        .route("/agents/{agent}/run", post(run_agent))
        .route("/agents/{agent}/reset-session", post(reset_agent_session))
        .route("/jobs", get(list_jobs).post(create_job))
        .route("/jobs/preview", post(preview_schedule))
        .route(
            "/jobs/{id}",
            get(get_job).put(update_job).delete(delete_job),
        )
        .route("/jobs/{id}/trigger", post(trigger_job))
        .route("/jobs/{id}/enable", post(enable_job))
        .route("/jobs/{id}/disable", post(disable_job))
        .route("/auth", get(auth_status))
        .route("/auth/api-key", post(set_api_key))
        .route("/auth/oauth-token", post(set_oauth_token))
        .route("/auth/use-environment", post(use_environment))
        .route("/auth/sign-out", post(sign_out))
        .route(
            "/auth/flow",
            get(get_flow).post(start_flow).delete(cancel_flow),
        )
        .route("/auth/flow/code", post(submit_code))
}

type ApiResult<T> = Result<Json<T>, AppError>;
type Created<T> = Result<(StatusCode, Json<T>), AppError>;

// ---- runs ---------------------------------------------------------------

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    job_id: Option<i64>,
    agent_id: Option<i64>,
}

fn default_limit() -> i64 {
    50
}

async fn list_runs(State(s): State<AppState>, Query(q): Query<ListQuery>) -> ApiResult<Vec<Run>> {
    let filter = RunFilter {
        job_id: q.job_id,
        agent_id: q.agent_id,
    };
    Ok(Json(s.db.list_runs(q.limit.clamp(1, 1000), filter).await?))
}

async fn create_run(State(s): State<AppState>, Json(spec): Json<RunSpec>) -> Created<Run> {
    Ok((StatusCode::CREATED, Json(s.submit(spec).await?)))
}

async fn get_run(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Run> {
    s.db.get_run(id)
        .await?
        .map(Json)
        .ok_or(AppError::not_found("run"))
}

#[derive(Serialize)]
struct Cancelled {
    cancelled: bool,
}

async fn cancel_run(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Cancelled> {
    Ok(Json(Cancelled {
        cancelled: s.exec.cancel(id).await?,
    }))
}

#[derive(Deserialize)]
struct PromptBody {
    prompt: String,
}

async fn follow_up(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PromptBody>,
) -> Created<Run> {
    Ok((
        StatusCode::CREATED,
        Json(s.follow_up(id, body.prompt).await?),
    ))
}

async fn rerun(State(s): State<AppState>, Path(id): Path<i64>) -> Created<Run> {
    Ok((StatusCode::CREATED, Json(s.rerun(id).await?)))
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    follow: bool,
}

/// Plain-text log. With `?follow=true` the response stays open and streams
/// new lines until the run ends, then a final `[ctm] status: <status>`.
async fn run_log(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LogQuery>,
) -> Result<Response, AppError> {
    let run = s.db.get_run(id).await?.ok_or(AppError::not_found("run"))?;
    let headers = [(header::CONTENT_TYPE, "text/plain; charset=utf-8")];
    if !q.follow {
        let text = tokio::fs::read_to_string(s.cfg.log_path(run.id))
            .await
            .unwrap_or_default();
        return Ok((headers, text).into_response());
    }
    let stream = s.exec.follow(run.id).await?.map(|ev| {
        Ok::<_, std::convert::Infallible>(match ev {
            LogEvent::Line { text, .. } => format!("{text}\n"),
            LogEvent::Done(status) => format!("[ctm] status: {status}\n"),
        })
    });
    Ok((headers, Body::from_stream(stream)).into_response())
}

// ---- agents -------------------------------------------------------------

async fn find_agent(s: &AppState, name_or_id: &str) -> Result<Agent, AppError> {
    s.db.find_agent(name_or_id)
        .await?
        .ok_or(AppError::not_found("agent"))
}

async fn list_agents(State(s): State<AppState>) -> ApiResult<Vec<Agent>> {
    Ok(Json(s.db.list_agents().await?))
}

async fn create_agent(State(s): State<AppState>, Json(a): Json<NewAgent>) -> Created<Agent> {
    let agent = s.db.create_agent(a).await.map_err(AppError::bad_request)?;
    Ok((StatusCode::CREATED, Json(agent)))
}

async fn get_agent(State(s): State<AppState>, Path(agent): Path<String>) -> ApiResult<Agent> {
    Ok(Json(find_agent(&s, &agent).await?))
}

async fn update_agent(
    State(s): State<AppState>,
    Path(agent): Path<String>,
    Json(a): Json<NewAgent>,
) -> ApiResult<Agent> {
    let id = find_agent(&s, &agent).await?.id;
    s.db.update_agent(id, a)
        .await
        .map_err(AppError::bad_request)?
        .map(Json)
        .ok_or(AppError::not_found("agent"))
}

async fn delete_agent(
    State(s): State<AppState>,
    Path(agent): Path<String>,
) -> Result<StatusCode, AppError> {
    let id = find_agent(&s, &agent).await?.id;
    s.db.delete_agent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_agent(
    State(s): State<AppState>,
    Path(agent): Path<String>,
    Json(body): Json<PromptBody>,
) -> Created<Run> {
    let agent = find_agent(&s, &agent).await?;
    Ok((
        StatusCode::CREATED,
        Json(s.run_agent(&agent, body.prompt).await?),
    ))
}

async fn reset_agent_session(
    State(s): State<AppState>,
    Path(agent): Path<String>,
) -> ApiResult<Agent> {
    let id = find_agent(&s, &agent).await?.id;
    s.db.set_agent_session(id, None).await?;
    s.db.get_agent(id)
        .await?
        .map(Json)
        .ok_or(AppError::not_found("agent"))
}

// ---- jobs ---------------------------------------------------------------

#[derive(Deserialize)]
struct JobsQuery {
    agent_id: Option<i64>,
}

async fn list_jobs(State(s): State<AppState>, Query(q): Query<JobsQuery>) -> ApiResult<Vec<Job>> {
    Ok(Json(s.db.list_jobs(q.agent_id).await?))
}

async fn create_job(State(s): State<AppState>, Json(j): Json<NewJob>) -> Created<Job> {
    let job = s.db.create_job(j).await.map_err(AppError::bad_request)?;
    Ok((StatusCode::CREATED, Json(job)))
}

async fn get_job(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Job> {
    s.db.get_job(id)
        .await?
        .map(Json)
        .ok_or(AppError::not_found("job"))
}

async fn update_job(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(j): Json<NewJob>,
) -> ApiResult<Job> {
    s.db.update_job(id, j)
        .await
        .map_err(AppError::bad_request)?
        .map(Json)
        .ok_or(AppError::not_found("job"))
}

async fn delete_job(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    if s.db.delete_job(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found("job"))
    }
}

async fn trigger_job(State(s): State<AppState>, Path(id): Path<i64>) -> Created<Run> {
    Ok((StatusCode::CREATED, Json(s.trigger_job(id).await?)))
}

async fn enable_job(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Job> {
    set_enabled(s, id, true).await
}

async fn disable_job(State(s): State<AppState>, Path(id): Path<i64>) -> ApiResult<Job> {
    set_enabled(s, id, false).await
}

async fn set_enabled(s: AppState, id: i64, enabled: bool) -> ApiResult<Job> {
    s.db.set_job_enabled(id, enabled)
        .await?
        .map(Json)
        .ok_or(AppError::not_found("job"))
}

#[derive(Deserialize)]
struct PreviewBody {
    #[serde(default)]
    schedule_kind: ScheduleKind,
    schedule: Option<String>,
    #[serde(default = "utc")]
    timezone: String,
    #[serde(default = "five")]
    count: usize,
}

fn utc() -> String {
    "UTC".into()
}

fn five() -> usize {
    5
}

#[derive(Serialize)]
struct Preview {
    description: String,
    next: Vec<DateTime<Utc>>,
}

async fn preview_schedule(Json(b): Json<PreviewBody>) -> ApiResult<Preview> {
    let s = Schedule::parse(b.schedule_kind, b.schedule.as_deref(), &b.timezone)
        .map_err(AppError::bad_request)?;
    Ok(Json(Preview {
        description: s.describe(),
        next: s.upcoming(Utc::now(), b.count.clamp(1, 50)),
    }))
}

// ---- Claude auth ----------------------------------------------------------

async fn auth_status(State(s): State<AppState>) -> ApiResult<AuthStatus> {
    Ok(Json(s.auth.status(true).await?))
}

#[derive(Deserialize)]
struct ValueBody {
    value: String,
}

async fn set_api_key(State(s): State<AppState>, Json(b): Json<ValueBody>) -> ApiResult<AuthStatus> {
    s.auth
        .set_api_key(&b.value)
        .await
        .map_err(AppError::bad_request)?;
    Ok(Json(s.auth.status(false).await?))
}

async fn set_oauth_token(
    State(s): State<AppState>,
    Json(b): Json<ValueBody>,
) -> ApiResult<AuthStatus> {
    s.auth
        .set_oauth_token(&b.value)
        .await
        .map_err(AppError::bad_request)?;
    Ok(Json(s.auth.status(false).await?))
}

async fn use_environment(State(s): State<AppState>) -> ApiResult<AuthStatus> {
    s.auth.use_environment().await?;
    Ok(Json(s.auth.status(false).await?))
}

async fn sign_out(State(s): State<AppState>) -> ApiResult<AuthStatus> {
    s.auth.sign_out().await?;
    Ok(Json(s.auth.status(false).await?))
}

#[derive(Deserialize)]
struct StartFlow {
    kind: FlowKind,
}

async fn start_flow(
    State(s): State<AppState>,
    Json(b): Json<StartFlow>,
) -> ApiResult<FlowSnapshot> {
    Ok(Json(s.auth.start_flow(b.kind).await?))
}

async fn get_flow(State(s): State<AppState>) -> ApiResult<Option<FlowSnapshot>> {
    Ok(Json(s.auth.flow()))
}

async fn cancel_flow(State(s): State<AppState>) -> StatusCode {
    s.auth.cancel_flow();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

async fn submit_code(
    State(s): State<AppState>,
    Json(b): Json<CodeBody>,
) -> ApiResult<Option<FlowSnapshot>> {
    s.auth.submit_code(&b.code).map_err(AppError::bad_request)?;
    Ok(Json(s.auth.flow()))
}
