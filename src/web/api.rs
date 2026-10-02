//! JSON API used by the `ctm` CLI (and any other headless client).

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::{AppError, AppState};
use crate::{
    db::{NewTask, Run, RunSpec, Task},
    executor::LogEvent,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/cancel", post(cancel_run))
        .route("/runs/{id}/followup", post(follow_up))
        .route("/runs/{id}/log", get(run_log))
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/{id}", get(get_task).delete(delete_task))
        .route("/tasks/{id}/trigger", post(trigger_task))
        .route("/tasks/{id}/enable", post(enable_task))
        .route("/tasks/{id}/disable", post(disable_task))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    task_id: Option<i64>,
}

fn default_limit() -> i64 {
    50
}

async fn list_runs(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Run>>, AppError> {
    Ok(Json(
        s.db.list_runs(q.limit.clamp(1, 1000), q.task_id).await?,
    ))
}

async fn create_run(
    State(s): State<AppState>,
    Json(spec): Json<RunSpec>,
) -> Result<(StatusCode, Json<Run>), AppError> {
    let run = s.submit(spec, None).await.map_err(AppError::bad_request)?;
    Ok((StatusCode::CREATED, Json(run)))
}

async fn get_run(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Json<Run>, AppError> {
    s.db.get_run(id)
        .await?
        .map(Json)
        .ok_or(AppError::not_found("run"))
}

#[derive(Serialize)]
struct Cancelled {
    cancelled: bool,
}

async fn cancel_run(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Cancelled>, AppError> {
    Ok(Json(Cancelled {
        cancelled: s.exec.cancel(id).await?,
    }))
}

#[derive(Deserialize)]
struct FollowUp {
    prompt: String,
}

async fn follow_up(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<FollowUp>,
) -> Result<(StatusCode, Json<Run>), AppError> {
    Ok((
        StatusCode::CREATED,
        Json(s.follow_up(id, body.prompt).await?),
    ))
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

async fn list_tasks(State(s): State<AppState>) -> Result<Json<Vec<Task>>, AppError> {
    Ok(Json(s.db.list_tasks().await?))
}

async fn create_task(
    State(s): State<AppState>,
    Json(t): Json<NewTask>,
) -> Result<(StatusCode, Json<Task>), AppError> {
    let task = s.db.create_task(t).await.map_err(AppError::bad_request)?;
    Ok((StatusCode::CREATED, Json(task)))
}

async fn get_task(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Json<Task>, AppError> {
    s.db.get_task(id)
        .await?
        .map(Json)
        .ok_or(AppError::not_found("task"))
}

async fn delete_task(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    if s.db.delete_task(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found("task"))
    }
}

async fn trigger_task(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<Run>), AppError> {
    Ok((StatusCode::CREATED, Json(s.trigger_task(id).await?)))
}

async fn enable_task(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Task>, AppError> {
    set_enabled(s, id, true).await
}

async fn disable_task(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Task>, AppError> {
    set_enabled(s, id, false).await
}

async fn set_enabled(s: AppState, id: i64, enabled: bool) -> Result<Json<Task>, AppError> {
    s.db.set_task_enabled(id, enabled)
        .await?
        .map(Json)
        .ok_or(AppError::not_found("task"))
}
