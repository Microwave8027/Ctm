//! The "Claude account" page: lets the dashboard sign the server in to
//! Claude (see `claude_auth`).

use axum::{
    Form, Router,
    extract::{Query, State},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

use super::{
    AppError, AppState, components as c,
    forms::{CodeForm, ValueForm},
    pages::{form_error, hx_refresh},
};
use crate::claude_auth::{FlowKind, FlowState};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/claude", get(page))
        .route(
            "/settings/claude/flow",
            get(flow).post(start_flow).delete(cancel_flow),
        )
        .route("/settings/claude/flow/code", post(submit_code))
        .route("/settings/claude/api-key", post(set_api_key))
        .route("/settings/claude/oauth-token", post(set_oauth_token))
        .route("/settings/claude/use-environment", post(use_environment))
        .route("/settings/claude/sign-out", post(sign_out))
}

async fn page(State(s): State<AppState>) -> Result<Html<String>, AppError> {
    Ok(c::render_page(
        "Claude account",
        c::claude_page(s.auth.status(true).await?),
    ))
}

/// Polled while the CLI works; reloads the page once sign-in succeeds so the
/// status card reflects the new credentials.
async fn flow(State(s): State<AppState>) -> Response {
    let flow = s.auth.flow();
    if matches!(
        flow.as_ref().map(|f| &f.state),
        Some(FlowState::Succeeded { .. })
    ) {
        return hx_refresh();
    }
    c::render(c::flow_view(flow)).into_response()
}

#[derive(Deserialize)]
struct StartQuery {
    kind: FlowKind,
}

async fn start_flow(State(s): State<AppState>, Query(q): Query<StartQuery>) -> Response {
    match s.auth.start_flow(q.kind).await {
        Ok(flow) => c::render(c::flow_view(Some(flow))).into_response(),
        Err(e) => c::error_box(&format!("could not start sign-in: {e:#}")).into_response(),
    }
}

async fn cancel_flow(State(s): State<AppState>) -> Html<String> {
    s.auth.cancel_flow();
    c::render(c::flow_view(None))
}

async fn submit_code(State(s): State<AppState>, Form(form): Form<CodeForm>) -> Response {
    match s.auth.submit_code(&form.code) {
        Ok(()) => c::render(c::flow_view(s.auth.flow())).into_response(),
        Err(e) => form_error("#auth-flow", e),
    }
}

async fn set_api_key(State(s): State<AppState>, Form(form): Form<ValueForm>) -> Response {
    match s.auth.set_api_key(&form.value).await {
        Ok(()) => hx_refresh(),
        Err(e) => form_error("#cred-error", e),
    }
}

async fn set_oauth_token(State(s): State<AppState>, Form(form): Form<ValueForm>) -> Response {
    match s.auth.set_oauth_token(&form.value).await {
        Ok(()) => hx_refresh(),
        Err(e) => form_error("#cred-error", e),
    }
}

async fn use_environment(State(s): State<AppState>) -> Result<Response, AppError> {
    s.auth.use_environment().await?;
    Ok(hx_refresh())
}

async fn sign_out(State(s): State<AppState>) -> Result<Response, AppError> {
    s.auth.sign_out().await?;
    Ok(hx_refresh())
}
