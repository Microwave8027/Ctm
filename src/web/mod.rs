mod api;
mod auth;
mod claude;
mod components;
mod forms;
mod pages;

use std::sync::Arc;

use axum::{
    Json, Router,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    claude_auth::ClaudeAuth,
    config::Config,
    db::{Agent, Db, Run, RunOrigin, RunSpec},
    executor::Executor,
    jobs::{self, Trigger},
};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub exec: Arc<Executor>,
    pub auth: Arc<ClaudeAuth>,
}

impl AppState {
    /// Creates and queues an ad-hoc run.
    pub async fn submit(&self, spec: RunSpec) -> Result<Run, AppError> {
        let run = self
            .db
            .create_run(spec, RunOrigin::default())
            .await
            .map_err(AppError::bad_request)?;
        self.exec.enqueue(run.id);
        Ok(run)
    }

    /// Runs an agent once with `prompt`.
    pub async fn run_agent(&self, agent: &Agent, prompt: String) -> Result<Run, AppError> {
        let origin = RunOrigin {
            agent: Some(agent),
            ..Default::default()
        };
        let run = self
            .db
            .create_run(agent.spec(prompt, None), origin)
            .await
            .map_err(AppError::bad_request)?;
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
        if parent.session_id.is_none() {
            return Err(AppError::bad_request("that run has no session to continue"));
        }
        let spec = RunSpec {
            prompt,
            ..parent.spec()
        };
        let origin = RunOrigin {
            parent: Some(&parent),
            ..Default::default()
        };
        let run = self
            .db
            .create_run(spec, origin)
            .await
            .map_err(AppError::bad_request)?;
        self.exec.enqueue(run.id);
        Ok(run)
    }

    /// Re-runs a finished run's prompt with the same settings.
    pub async fn rerun(&self, id: i64) -> Result<Run, AppError> {
        let run = self
            .db
            .get_run(id)
            .await?
            .ok_or(AppError::not_found("run"))?;
        let agent = match run.agent_id {
            Some(a) => self.db.get_agent(a).await?,
            None => None,
        };
        let origin = RunOrigin {
            agent: agent.as_ref(),
            job_id: run.job_id,
            ..Default::default()
        };
        let new = self
            .db
            .create_run(run.spec(), origin)
            .await
            .map_err(AppError::bad_request)?;
        self.exec.enqueue(new.id);
        Ok(new)
    }

    pub async fn trigger_job(&self, job_id: i64) -> Result<Run, AppError> {
        let job = self
            .db
            .get_job(job_id)
            .await?
            .ok_or(AppError::not_found("job"))?;
        jobs::fire_job(&self.db, &self.exec, &job, Trigger::Manual)
            .await
            .map_err(AppError::bad_request)?
            .ok_or_else(|| AppError::bad_request("the job's overlap policy skipped this run"))
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(pages::routes())
        .merge(claude::routes())
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
