//! Optional shared-token auth. When `CTM_TOKEN` is set, API clients send
//! `Authorization: Bearer <token>` and browsers log in once to get a cookie.

use axum::{
    Form, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;

use super::{AppState, components};

const COOKIE: &str = "ctm_token";

pub fn routes() -> Router<AppState> {
    Router::new().route("/login", get(login_page).post(login))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn require(
    State(state): State<AppState>,
    jar: CookieJar,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.cfg.token.as_deref() else {
        return next.run(req).await;
    };
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let cookie = jar.get(COOKIE).map(|c| c.value());
    let ok = [bearer, cookie]
        .into_iter()
        .flatten()
        .any(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()));
    if ok {
        return next.run(req).await;
    }
    if req.uri().path().starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response();
    }
    if req.headers().contains_key("hx-request") {
        return ([("HX-Redirect", "/login")], StatusCode::UNAUTHORIZED).into_response();
    }
    Redirect::to("/login").into_response()
}

async fn login_page() -> Html<String> {
    components::render_page("Log in", components::login_view(None))
}

#[derive(Deserialize)]
struct LoginForm {
    token: String,
}

async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    let valid = match state.cfg.token.as_deref() {
        Some(t) => constant_time_eq(form.token.as_bytes(), t.as_bytes()),
        None => true,
    };
    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            components::render_page("Log in", components::login_view(Some("Invalid token"))),
        )
            .into_response();
    }
    let cookie = Cookie::build((COOKIE, form.token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .permanent()
        .build();
    (jar.add(cookie), Redirect::to("/")).into_response()
}
