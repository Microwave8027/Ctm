mod api;
mod auth;
mod components;
mod pages;

use std::sync::Arc;

use axum::{
    Json, Router,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    config::Config,
    db::{Db, Run, RunSpec},
    executor::Executor,
};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub exec: Arc<Executor>,
}

impl AppState {
    /// Creates and queues a run.
    pub async fn submit(&self, spec: RunSpec, task_id: Option<i64>) -> anyhow::Result<Run> {
        let run = self.db.create_run(spec, task_id, None).await?;
        self.exec.enqueue(run.id);
        Ok(run)
    }

    /// Continues a previous run's Claude session (and workspace) with a new prompt.
    pub async fn follow_up(&self, parent_id: i64, prompt: String) -> Result<Run, AppError> {
        let parent = self
            .db
            .get_run(parent_id)
            .await?
            .ok_or(AppError::not_found("run"))?;
        let spec = RunSpec {
            prompt,
            mode: parent.mode,
            image: parent.image.clone(),
            repo: parent.repo.clone(),
            model: parent.model.clone(),
            extra_args: parent.extra_args.clone(),
        };
        let run = self
            .db
            .create_run(spec, None, Some(&parent))
            .await
            .map_err(AppError::bad_request)?;
        self.exec.enqueue(run.id);
        Ok(run)
    }

    pub async fn trigger_task(&self, task_id: i64) -> Result<Run, AppError> {
        let task = self
            .db
            .get_task(task_id)
            .await?
            .ok_or(AppError::not_found("task"))?;
        let run = self.submit(task.spec(), Some(task.id)).await?;
        self.db
            .set_task_schedule(task.id, Some(run.created_at), task.next_run_at)
            .await?;
        Ok(run)
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(pages::routes())
        .nest("/api", api::routes())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require,
        ))
        .merge(auth::routes())
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/assets/htmx.min.js",
            get(|| asset("text/javascript", include_str!("../../assets/htmx.min.js"))),
        )
        .route(
            "/assets/sse.min.js",
            get(|| asset("text/javascript", include_str!("../../assets/sse.min.js"))),
        )
        .route(
            "/assets/app.css",
            get(|| asset("text/css", include_str!("../../assets/app.css"))),
        )
        .route(
            "/assets/app.js",
            get(|| asset("text/javascript", include_str!("../../assets/app.js"))),
        )
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

async fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

/// Error type shared by the page and API handlers.
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
}

impl AppError {
    pub fn not_found(what: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("{what} not found"),
        }
    }

    pub fn bad_request(e: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: format!("{e:#}"),
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{e:#}"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}
