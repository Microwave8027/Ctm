//! Claude credentials managed from the dashboard.
//!
//! The dashboard can sign in to Claude for the server, so runs don't depend
//! on whoever started ctm having exported keys:
//!
//! * **Claude account login** drives `claude auth login` in a pseudo terminal
//!   against a ctm-owned `CLAUDE_CONFIG_DIR`. The person at the dashboard
//!   opens the sign-in URL, pastes back the code, and the CLI stores (and
//!   later refreshes) the credentials itself. This is the only method whose
//!   scopes cover cloud sessions.
//! * **Long-lived token** drives `claude setup-token` the same way and keeps
//!   the resulting `CLAUDE_CODE_OAUTH_TOKEN` (inference only, ~1 year).
//! * **API key / token** can also be pasted directly.
//! * **Environment** (default) leaves whatever the server process has.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::{
    config::Config,
    db::Db,
    pty::{PtyProcess, first_hyperlink, strip_ansi},
};

const KEY_METHOD: &str = "claude.method";
const KEY_API_KEY: &str = "claude.api_key";
const KEY_OAUTH_TOKEN: &str = "claude.oauth_token";

pub const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";
pub const OAUTH_TOKEN_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// How long a sign-in may wait for the user before it's abandoned.
const FLOW_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Environment,
    ApiKey,
    OauthToken,
    Login,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Environment => "environment",
            AuthMethod::ApiKey => "api_key",
            AuthMethod::OauthToken => "oauth_token",
            AuthMethod::Login => "login",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AuthMethod::Environment => "Server environment",
            AuthMethod::ApiKey => "Anthropic API key",
            AuthMethod::OauthToken => "Long-lived OAuth token",
            AuthMethod::Login => "Claude account login",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "api_key" => AuthMethod::ApiKey,
            "oauth_token" => AuthMethod::OauthToken,
            "login" => AuthMethod::Login,
            _ => AuthMethod::Environment,
        }
    }
}

/// What a run needs to be authenticated.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    /// Variables to set for the Claude process.
    pub env: Vec<(String, String)>,
    /// Variables to clear so they can't shadow the chosen method.
    pub remove: Vec<&'static str>,
    /// `CLAUDE_CONFIG_DIR` for local and cloud runs (account login).
    pub config_dir: Option<PathBuf>,
    /// For docker runs with account login: the current access token.
    pub docker_token: Option<String>,
}

impl Credentials {
    /// Applies the credentials to a local `claude` invocation.
    pub fn apply(&self, cmd: &mut Command) {
        for name in &self.remove {
            cmd.env_remove(name);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if let Some(dir) = &self.config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
    }

    /// Variables for the docker client process plus the names to forward
    /// with `-e NAME` (values never appear on the command line).
    pub fn docker_env(&self) -> Vec<(String, String)> {
        let mut env = self.env.clone();
        if let Some(t) = &self.docker_token {
            env.push((OAUTH_TOKEN_VAR.into(), t.clone()));
        }
        env
    }

    pub fn env_pairs(&self) -> Vec<(String, String)> {
        let mut env = self.env.clone();
        if let Some(dir) = &self.config_dir {
            env.push(("CLAUDE_CONFIG_DIR".into(), dir.display().to_string()));
        }
        env
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    /// `claude auth login`: full account login (local, docker and cloud).
    Login,
    /// `claude setup-token`: long-lived inference-only token.
    SetupToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum FlowState {
    Starting,
    AwaitingCode { url: String },
    Verifying,
    Succeeded { message: String },
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowSnapshot {
    pub id: u64,
    pub kind: FlowKind,
    #[serde(flatten)]
    pub state: FlowState,
    pub started_at: DateTime<Utc>,
}

struct Flow {
    id: u64,
    kind: FlowKind,
    state: FlowState,
    started_at: DateTime<Utc>,
    process: Option<Arc<PtyProcess>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub method: AuthMethod,
    pub method_label: &'static str,
    /// Masked credential for display, e.g. `sk-ant-oat…a1b2`.
    pub credential: Option<String>,
    /// Output of `claude auth status --json` with these credentials.
    pub cli: Option<serde_json::Value>,
    pub cli_error: Option<String>,
    pub flow: Option<FlowSnapshot>,
}

pub struct ClaudeAuth {
    cfg: Arc<Config>,
    db: Db,
    flow: Mutex<Option<Flow>>,
    next_flow_id: std::sync::atomic::AtomicU64,
}

pub fn mask(secret: &str) -> String {
    let n = secret.chars().count();
    if n <= 16 {
        return "•".repeat(n.min(8));
    }
    let head: String = secret.chars().take(10).collect();
    let tail: String = secret.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

impl ClaudeAuth {
    pub fn new(cfg: Arc<Config>, db: Db) -> Self {
        Self {
            cfg,
            db,
            flow: Mutex::new(None),
            next_flow_id: 1.into(),
        }
    }

    /// The ctm-owned Claude config directory used by account login.
    pub fn config_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("claude-config")
    }

    pub async fn method(&self) -> Result<AuthMethod> {
        Ok(self
            .db
            .get_setting(KEY_METHOD)
            .await?
            .map(|s| AuthMethod::parse(&s))
            .unwrap_or(AuthMethod::Environment))
    }

    pub async fn credentials(&self) -> Result<Credentials> {
        Ok(match self.method().await? {
            AuthMethod::Environment => Credentials::default(),
            AuthMethod::ApiKey => Credentials {
                env: vec![(API_KEY_VAR.into(), self.secret(KEY_API_KEY).await?)],
                remove: vec![OAUTH_TOKEN_VAR],
                ..Default::default()
            },
            AuthMethod::OauthToken => Credentials {
                env: vec![(OAUTH_TOKEN_VAR.into(), self.secret(KEY_OAUTH_TOKEN).await?)],
                remove: vec![API_KEY_VAR],
                ..Default::default()
            },
            AuthMethod::Login => Credentials {
                remove: vec![API_KEY_VAR, OAUTH_TOKEN_VAR],
                config_dir: Some(self.config_dir()),
                docker_token: self.login_access_token().await,
                ..Default::default()
            },
        })
    }

    async fn secret(&self, key: &str) -> Result<String> {
        self.db.get_setting(key).await?.with_context(|| {
            format!("{key} is selected but not stored; set it again from the dashboard")
        })
    }

    /// Current access token from the managed login, for containers that
    /// can't share the config directory. The CLI refreshes it on use.
    async fn login_access_token(&self) -> Option<String> {
        let raw = tokio::fs::read_to_string(self.config_dir().join(".credentials.json"))
            .await
            .ok()?;
        let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
        v.pointer("/claudeAiOauth/accessToken")?
            .as_str()
            .map(String::from)
    }

    pub async fn status(&self, check_cli: bool) -> Result<AuthStatus> {
        let method = self.method().await?;
        let credential = match method {
            AuthMethod::ApiKey => self.db.get_setting(KEY_API_KEY).await?.map(|s| mask(&s)),
            AuthMethod::OauthToken => self
                .db
                .get_setting(KEY_OAUTH_TOKEN)
                .await?
                .map(|s| mask(&s)),
            AuthMethod::Login => Some(self.config_dir().display().to_string()),
            AuthMethod::Environment => [API_KEY_VAR, OAUTH_TOKEN_VAR]
                .iter()
                .find(|v| std::env::var_os(v).is_some())
                .map(|v| format!("${v}")),
        };
        let (cli, cli_error) = if check_cli {
            match self.cli_status().await {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        } else {
            (None, None)
        };
        Ok(AuthStatus {
            method,
            method_label: method.label(),
            credential,
            cli,
            cli_error,
            flow: self.flow(),
        })
    }

    /// Asks the CLI itself whether these credentials log it in.
    async fn cli_status(&self) -> Result<serde_json::Value> {
        let creds = self.credentials().await?;
        let mut cmd = Command::new(&self.cfg.claude_bin);
        cmd.args(["auth", "status", "--json"]).kill_on_drop(true);
        creds.apply(&mut cmd);
        let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
            .await
            .context("claude auth status timed out")?
            .with_context(|| format!("running {} auth status", self.cfg.claude_bin))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(stdout.trim()).with_context(|| {
            format!(
                "unexpected output from claude auth status: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })
    }

    pub async fn use_environment(&self) -> Result<()> {
        self.db
            .set_setting(KEY_METHOD, AuthMethod::Environment.as_str())
            .await
    }

    pub async fn set_api_key(&self, key: &str) -> Result<()> {
        let key = key.trim();
        if !key.starts_with("sk-ant-") || key.len() < 20 {
            bail!("that doesn't look like an Anthropic API key (sk-ant-…)");
        }
        self.db.set_setting(KEY_API_KEY, key).await?;
        self.db
            .set_setting(KEY_METHOD, AuthMethod::ApiKey.as_str())
            .await
    }

    pub async fn set_oauth_token(&self, token: &str) -> Result<()> {
        let token = token.trim();
        if !token.starts_with("sk-ant-oat") || token.len() < 20 {
            bail!(
                "that doesn't look like a Claude OAuth token (sk-ant-oat…); run `claude setup-token` to get one"
            );
        }
        self.db.set_setting(KEY_OAUTH_TOKEN, token).await?;
        self.db
            .set_setting(KEY_METHOD, AuthMethod::OauthToken.as_str())
            .await
    }

    /// Forgets every stored credential and logs the managed config out.
    pub async fn sign_out(&self) -> Result<()> {
        self.cancel_flow();
        self.db.delete_setting(KEY_API_KEY).await?;
        self.db.delete_setting(KEY_OAUTH_TOKEN).await?;
        self.db
            .set_setting(KEY_METHOD, AuthMethod::Environment.as_str())
            .await?;
        let dir = self.config_dir();
        if dir.exists() {
            let mut cmd = Command::new(&self.cfg.claude_bin);
            cmd.args(["auth", "logout"])
                .env("CLAUDE_CONFIG_DIR", &dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let _ = tokio::time::timeout(Duration::from_secs(20), cmd.status()).await;
            let _ = tokio::fs::remove_file(dir.join(".credentials.json")).await;
        }
        Ok(())
    }

    /// Interactive `claude` (used for `--cloud`) shows first-run onboarding
    /// and a folder-trust prompt that nobody can answer on a server. For the
    /// ctm-managed login, mark both as done for `workspace`. A config owned
    /// by the server's user (environment credentials) is left untouched.
    pub async fn prepare_interactive(&self, workspace: &std::path::Path) -> Result<()> {
        if self.method().await? != AuthMethod::Login {
            return Ok(());
        }
        tokio::fs::create_dir_all(self.config_dir()).await?;
        let path = self.config_dir().join(".claude.json");
        let mut cfg: serde_json::Value = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({})),
            Err(_) => serde_json::json!({}),
        };
        if !cfg.is_object() {
            cfg = serde_json::json!({});
        }
        cfg["hasCompletedOnboarding"] = true.into();
        if cfg.get("theme").is_none() {
            cfg["theme"] = "dark".into();
        }
        if !cfg["projects"].is_object() {
            cfg["projects"] = serde_json::json!({});
        }
        let project = &mut cfg["projects"][workspace.display().to_string()];
        if !project.is_object() {
            *project = serde_json::json!({});
        }
        project["hasTrustDialogAccepted"] = true.into();
        project["hasCompletedProjectOnboarding"] = true.into();
        let tmp = path.with_extension("json.ctm-tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(&cfg)?)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    // ---- interactive sign-in --------------------------------------------

    pub fn flow(&self) -> Option<FlowSnapshot> {
        let mut guard = self.flow.lock().unwrap();
        let flow = guard.as_mut()?;
        let expired = (Utc::now() - flow.started_at).to_std().unwrap_or_default() > FLOW_TTL;
        if expired
            && matches!(
                flow.state,
                FlowState::Starting | FlowState::AwaitingCode { .. }
            )
        {
            if let Some(p) = flow.process.take() {
                p.kill();
            }
            flow.state = FlowState::Failed {
                message: "sign-in timed out; start again".into(),
            };
        }
        Some(FlowSnapshot {
            id: flow.id,
            kind: flow.kind,
            state: flow.state.clone(),
            started_at: flow.started_at,
        })
    }

    pub fn cancel_flow(&self) {
        if let Some(flow) = self.flow.lock().unwrap().take()
            && let Some(p) = flow.process
        {
            p.kill();
        }
    }

    fn set_state(&self, id: u64, state: FlowState) {
        if let Some(flow) = self.flow.lock().unwrap().as_mut().filter(|f| f.id == id) {
            if matches!(
                state,
                FlowState::Succeeded { .. } | FlowState::Failed { .. }
            ) {
                flow.process = None;
            }
            flow.state = state;
        }
    }

    /// Starts `claude auth login` or `claude setup-token` and returns once
    /// the sign-in URL is known (or the attempt failed).
    pub async fn start_flow(self: &Arc<Self>, kind: FlowKind) -> Result<FlowSnapshot> {
        self.cancel_flow();
        let id = self
            .next_flow_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let (args, config_dir) = match kind {
            FlowKind::Login => (
                vec!["auth".into(), "login".into(), "--claudeai".into()],
                self.config_dir(),
            ),
            // setup-token only prints a token; keep its scratch config away
            // from the managed login.
            FlowKind::SetupToken => (
                vec!["setup-token".into()],
                self.cfg.data_dir.join(format!("claude-setup-{id}")),
            ),
        };
        tokio::fs::create_dir_all(&config_dir).await?;
        restrict_permissions(&config_dir).await;

        let mut spawned = PtyProcess::spawn(
            &self.cfg.claude_bin,
            &args,
            &self.cfg.data_dir,
            &[("CLAUDE_CONFIG_DIR".into(), config_dir.display().to_string())],
            &[API_KEY_VAR, OAUTH_TOKEN_VAR],
        )?;
        *self.flow.lock().unwrap() = Some(Flow {
            id,
            kind,
            state: FlowState::Starting,
            started_at: Utc::now(),
            process: Some(spawned.process.clone()),
        });

        let (url_tx, url_rx) = tokio::sync::oneshot::channel::<()>();
        let this = self.clone();
        tokio::spawn(async move {
            let mut raw = String::new();
            let mut url_tx = Some(url_tx);
            let mut exit = None;
            loop {
                tokio::select! {
                    chunk = spawned.output.recv() => match chunk {
                        Some(c) => {
                            raw.push_str(&c);
                            if url_tx.is_some()
                                && let Some(url) = sign_in_url(&raw) {
                                    this.set_state(id, FlowState::AwaitingCode { url });
                                    let _ = url_tx.take().map(|t| t.send(()));
                                }
                        }
                        None => break,
                    },
                    code = &mut spawned.exit, if exit.is_none() => {
                        exit = Some(code.unwrap_or(1));
                        // Let the reader deliver the final output.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        while let Ok(c) = spawned.output.try_recv() {
                            raw.push_str(&c);
                        }
                        break;
                    }
                }
            }
            let exit = match exit {
                Some(e) => e,
                None => spawned.exit.await.unwrap_or(1),
            };
            let outcome = this.finish_flow(kind, &raw, exit, &config_dir).await;
            this.set_state(
                id,
                match outcome {
                    Ok(message) => FlowState::Succeeded { message },
                    Err(e) => FlowState::Failed {
                        message: format!("{e:#}"),
                    },
                },
            );
            if kind == FlowKind::SetupToken {
                let _ = tokio::fs::remove_dir_all(&config_dir).await;
            }
            drop(url_tx);
        });

        // Wait briefly for the URL so the first response can show it.
        let _ = tokio::time::timeout(Duration::from_secs(20), url_rx).await;
        self.flow().context("sign-in was cancelled")
    }

    /// Sends the code the user copied from the sign-in page.
    pub fn submit_code(&self, code: &str) -> Result<()> {
        let code = code.trim();
        if code.is_empty() {
            bail!("paste the code shown after signing in");
        }
        let mut guard = self.flow.lock().unwrap();
        let flow = guard.as_mut().context("no sign-in in progress")?;
        if !matches!(flow.state, FlowState::AwaitingCode { .. }) {
            bail!("this sign-in is not waiting for a code");
        }
        let process = flow.process.clone().context("sign-in process has exited")?;
        process.type_line(code)?;
        flow.state = FlowState::Verifying;
        Ok(())
    }

    async fn finish_flow(
        &self,
        kind: FlowKind,
        raw: &str,
        exit: u32,
        config_dir: &std::path::Path,
    ) -> Result<String> {
        let text = strip_ansi(raw);
        let tail = || {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .rev()
                .take(2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" · ")
        };
        match kind {
            FlowKind::SetupToken => {
                let token = oauth_token(&text)
                    .with_context(|| format!("no token in the CLI output: {}", tail()))?;
                self.set_oauth_token(&token).await?;
                Ok(format!("Stored long-lived token {}", mask(&token)))
            }
            FlowKind::Login => {
                if exit != 0 || !config_dir.join(".credentials.json").exists() {
                    bail!(
                        "claude auth login did not complete (exit {exit}): {}",
                        tail()
                    );
                }
                restrict_permissions(config_dir).await;
                self.db
                    .set_setting(KEY_METHOD, AuthMethod::Login.as_str())
                    .await?;
                Ok("Signed in with your Claude account".into())
            }
        }
    }
}

fn sign_in_url(raw: &str) -> Option<String> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"https://\S+/oauth/authorize\?\S+").unwrap());
    first_hyperlink(raw)
        .filter(|u| u.contains("/oauth/authorize"))
        .or_else(|| re.find(&strip_ansi(raw)).map(|m| m.as_str().to_string()))
}

fn oauth_token(text: &str) -> Option<String> {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"sk-ant-oat\d+-[A-Za-z0-9_\-]{20,}").unwrap());
    re.find(text).map(|m| m.as_str().to_string())
}

async fn restrict_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls_and_tokens() {
        let raw = "Visit: https://claude.com/cai/oauth/authorize?code=true&client_id=abc&state=xyz\r\nPaste code";
        assert_eq!(
            sign_in_url(raw).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&client_id=abc&state=xyz")
        );
        let osc = "\x1b]8;id=1;https://claude.com/cai/oauth/authorize?a=1\x1b\\https://claude.com/cai/oau\x1b]8;;\x1b\\";
        assert_eq!(
            sign_in_url(osc).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?a=1")
        );
        assert_eq!(
            oauth_token("Your token:\n\n  sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWx_yz-12\n")
                .as_deref(),
            Some("sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWx_yz-12")
        );
        assert_eq!(mask("sk-ant-api03-abcdefghijklmnop"), "sk-ant-api…mnop");
    }

    #[tokio::test]
    async fn seeds_interactive_config_for_managed_login() {
        use clap::Parser;
        #[derive(Parser)]
        struct P {
            #[command(flatten)]
            c: Config,
        }
        let dir = std::env::temp_dir().join(format!("ctm-auth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("claude-config")).unwrap();
        std::fs::write(
            dir.join("claude-config/.claude.json"),
            r#"{"theme":"light","userID":"u1"}"#,
        )
        .unwrap();
        let cfg = P::parse_from(["x", "--data-dir", dir.to_str().unwrap()]).c;
        let db = Db::memory().await.unwrap();
        let auth = ClaudeAuth::new(Arc::new(cfg), db.clone());
        let ws = dir.join("workspaces/agent-1");

        // Not the managed login: leave config alone.
        auth.prepare_interactive(&ws).await.unwrap();
        let raw = std::fs::read_to_string(dir.join("claude-config/.claude.json")).unwrap();
        assert!(!raw.contains("hasCompletedOnboarding"));

        db.set_setting(KEY_METHOD, "login").await.unwrap();
        auth.prepare_interactive(&ws).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("claude-config/.claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["theme"], "light");
        assert_eq!(v["userID"], "u1");
        assert_eq!(
            v["projects"][ws.display().to_string()]["hasTrustDialogAccepted"],
            true
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
