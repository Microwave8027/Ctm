//! Terminal client: talks to a (possibly remote) ctm server over its JSON API.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures::StreamExt;
use reqwest::{Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use crate::db::{Agent, Job, Mode, Run};

#[derive(Args, Debug, Clone)]
pub struct Remote {
    /// Base URL of the ctm server.
    #[arg(
        long,
        env = "CTM_URL",
        default_value = "http://127.0.0.1:7878",
        global = true
    )]
    pub url: String,
    /// Access token (same value as the server's CTM_TOKEN).
    #[arg(long, env = "CTM_TOKEN", hide_env_values = true, global = true)]
    pub token: Option<String>,
}

/// How and where Claude runs (shared by `run` and `agent add`).
#[derive(Args, Debug, Default)]
pub struct ClaudeOpts {
    /// Run inside a Docker container.
    #[arg(long, conflicts_with = "cloud")]
    pub docker: bool,
    /// Run as a Claude Code cloud session (`claude --cloud`).
    #[arg(long)]
    pub cloud: bool,
    /// Cloud: send the prompt to this existing session (id or URL) instead of creating one.
    #[arg(long, requires = "cloud")]
    pub session: Option<String>,
    /// Cloud: self-hosted environment for new sessions (ccpool_...).
    #[arg(long, requires = "cloud")]
    pub environment: Option<String>,
    /// Docker image (docker mode only).
    #[arg(long)]
    pub image: Option<String>,
    /// Git repository to clone into the workspace.
    #[arg(long)]
    pub repo: Option<String>,
    /// Claude model alias or id.
    #[arg(long)]
    pub model: Option<String>,
    /// default, acceptEdits, plan or bypassPermissions.
    #[arg(long)]
    pub permission_mode: Option<String>,
    /// Tools Claude may use without asking, e.g. "Bash(npm test) Edit".
    #[arg(long)]
    pub allowed_tools: Option<String>,
    /// Extra instructions appended to Claude's system prompt.
    #[arg(long)]
    pub system_prompt: Option<String>,
    /// Extra arguments for `claude`, as one string.
    #[arg(long, allow_hyphen_values = true)]
    pub extra_args: Option<String>,
}

impl ClaudeOpts {
    fn mode(&self) -> Mode {
        if self.cloud {
            Mode::Cloud
        } else if self.docker {
            Mode::Docker
        } else {
            Mode::Local
        }
    }

    fn json(&self) -> Value {
        json!({
            "mode": self.mode(),
            "cloud_session": self.session,
            "environment": self.environment,
            "image": self.image,
            "repo": self.repo,
            "model": self.model,
            "permission_mode": self.permission_mode,
            "allowed_tools": self.allowed_tools,
            "system_prompt": self.system_prompt,
            "extra_args": self.extra_args,
        })
    }
}

#[derive(Args, Debug)]
pub struct ScheduleOpts {
    /// Cron expression (5 or 6 fields, or @hourly/@daily/...).
    #[arg(long, group = "when")]
    pub cron: Option<String>,
    /// Fixed interval, e.g. 15m, 1h30m, 2d.
    #[arg(long, group = "when")]
    pub every: Option<String>,
    /// One-shot time: RFC 3339 or "YYYY-MM-DD HH:MM" in --tz.
    #[arg(long, group = "when")]
    pub at: Option<String>,
    /// IANA time zone for cron and --at.
    #[arg(long, default_value = "UTC")]
    pub tz: String,
}

impl ScheduleOpts {
    fn json(&self) -> Value {
        let (kind, expr) = match (&self.cron, &self.every, &self.at) {
            (Some(c), _, _) => ("cron", Some(c)),
            (_, Some(e), _) => ("interval", Some(e)),
            (_, _, Some(a)) => ("once", Some(a)),
            _ => ("manual", None),
        };
        json!({ "schedule_kind": kind, "schedule": expr, "timezone": self.tz })
    }
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // parsed once per process
pub enum AgentCmd {
    /// List agents.
    Ls,
    /// Create an agent.
    Add {
        name: String,
        #[arg(long)]
        description: Option<String>,
        /// Every run resumes the agent's previous session.
        #[arg(long)]
        persistent: bool,
        #[command(flatten)]
        opts: ClaudeOpts,
    },
    /// Show an agent as JSON.
    Show { agent: String },
    /// Delete an agent and its schedules.
    Rm { agent: String },
    /// Run an agent now. Prompt is read from stdin when omitted.
    Run {
        agent: String,
        #[arg(short, long)]
        follow: bool,
        prompt: Option<String>,
    },
    /// Forget the agent's remembered session.
    Reset { agent: String },
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // parsed once per process
pub enum JobCmd {
    /// List schedules.
    Ls {
        #[arg(long)]
        agent: Option<String>,
    },
    /// Schedule an agent. Prompt is read from stdin when omitted; it may use
    /// {{date}}, {{time}}, {{last_result}}, {{upstream_result}} and more.
    Add {
        #[arg(long)]
        name: String,
        /// Agent name or id.
        #[arg(long)]
        agent: String,
        #[command(flatten)]
        schedule: ScheduleOpts,
        /// When the previous run is still active: skip, queue or replace.
        #[arg(long, default_value = "skip")]
        overlap: String,
        /// Kill runs after this many seconds.
        #[arg(long)]
        timeout: Option<i64>,
        /// Retry failed runs this many times (with doubling delays).
        #[arg(long, default_value_t = 0)]
        retries: i64,
        /// Seconds before the first retry.
        #[arg(long, default_value_t = 60)]
        retry_delay: i64,
        /// Random delay of up to this many seconds per fire.
        #[arg(long, default_value_t = 0)]
        jitter: i64,
        /// Job id to fire after each successful run.
        #[arg(long)]
        then: Option<i64>,
        #[arg(long)]
        disabled: bool,
        prompt: Option<String>,
    },
    /// Show a schedule as JSON.
    Show {
        id: i64,
    },
    /// Delete a schedule (its runs are kept).
    Rm {
        id: i64,
    },
    /// Fire a schedule now.
    Trigger {
        id: i64,
        #[arg(short, long)]
        follow: bool,
    },
    Enable {
        id: i64,
    },
    Disable {
        id: i64,
    },
    /// Show the next fire times of a schedule expression.
    Preview {
        #[command(flatten)]
        schedule: ScheduleOpts,
        #[arg(short = 'n', long, default_value_t = 5)]
        count: usize,
    },
}

#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Show which Claude credentials the server uses.
    Status,
    /// Sign the server in to Claude (opens a URL, asks for the code).
    Login {
        /// Create a long-lived inference-only token instead of a full login.
        #[arg(long)]
        token_only: bool,
    },
    /// Store an Anthropic API key (read from stdin).
    SetKey,
    /// Store a long-lived OAuth token from `claude setup-token` (read from stdin).
    SetToken,
    /// Use the server process's own environment.
    UseEnv,
    /// Forget stored credentials and log the managed login out.
    Logout,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // parsed once per process
pub enum Cmd {
    /// Queue a prompt. Reads the prompt from stdin when omitted.
    Run {
        #[command(flatten)]
        opts: ClaudeOpts,
        /// Kill the run after this many seconds.
        #[arg(long)]
        timeout: Option<i64>,
        /// Stream the output until the run finishes.
        #[arg(short, long)]
        follow: bool,
        prompt: Option<String>,
    },
    /// List recent runs.
    Ps {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
        #[arg(long)]
        job: Option<i64>,
        #[arg(long)]
        agent: Option<i64>,
    },
    /// Show one run as JSON.
    Show { id: i64 },
    /// Print a run's log.
    Logs {
        id: i64,
        #[arg(short, long)]
        follow: bool,
    },
    /// Cancel a queued or running run.
    Cancel { id: i64 },
    /// Continue a finished run's session with a new prompt.
    Resume {
        id: i64,
        #[arg(short, long)]
        follow: bool,
        prompt: Option<String>,
    },
    /// Run a finished run's prompt again with the same settings.
    Rerun {
        id: i64,
        #[arg(short, long)]
        follow: bool,
    },
    /// Manage agents (reusable Claude setups).
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Manage schedules that fire agents.
    #[command(subcommand)]
    Job(JobCmd),
    /// Manage the Claude credentials the server uses.
    #[command(subcommand)]
    Auth(AuthCmd),
}

struct Client {
    http: reqwest::Client,
    remote: Remote,
}

impl Client {
    fn req(&self, method: Method, path: &str) -> RequestBuilder {
        let url = format!("{}/api{}", self.remote.url.trim_end_matches('/'), path);
        let mut rb = self.http.request(method, url);
        if let Some(t) = &self.remote.token {
            rb = rb.bearer_auth(t);
        }
        rb
    }

    async fn send<T: DeserializeOwned>(&self, rb: RequestBuilder) -> Result<T> {
        let resp = rb.send().await.context("connecting to ctm server")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or(body);
            bail!("{status}: {msg}");
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(serde_json::from_value(Value::Null)?);
        }
        Ok(resp.json().await?)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.req(Method::GET, path)).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        self.send(self.req(Method::POST, path).json(&body)).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let _: Value = self.send(self.req(Method::DELETE, path)).await?;
        Ok(())
    }

    /// Streams the log to stdout; returns the final status.
    async fn follow(&self, id: i64) -> Result<String> {
        let resp = self
            .req(Method::GET, &format!("/runs/{id}/log?follow=true"))
            .send()
            .await?
            .error_for_status()?;
        let mut out = tokio::io::stdout();
        let mut body = resp.bytes_stream();
        let mut last = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            out.write_all(&chunk).await?;
            out.flush().await?;
            last.extend_from_slice(&chunk);
            if last.len() > 4096 {
                last.drain(..last.len() - 4096);
            }
        }
        let tail = String::from_utf8_lossy(&last);
        Ok(tail
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("[ctm] status: "))
            .unwrap_or("unknown")
            .to_string())
    }

    /// Prints the run id, or follows it and turns failure into an error.
    async fn finish(&self, run: &Run, follow: bool) -> Result<()> {
        eprintln!("queued run {}", run.id);
        if !follow {
            println!("{}", run.id);
            return Ok(());
        }
        let status = self.follow(run.id).await?;
        if status != "succeeded" && status != "dispatched" {
            bail!("run {} {status}", run.id);
        }
        Ok(())
    }
}

async fn prompt_or_stdin(prompt: Option<String>) -> Result<String> {
    if let Some(p) = prompt {
        return Ok(p);
    }
    let mut s = String::new();
    tokio::io::stdin().read_to_string(&mut s).await?;
    if s.trim().is_empty() {
        bail!("no prompt given (pass it as an argument or on stdin)");
    }
    Ok(s)
}

async fn read_secret() -> Result<String> {
    eprintln!("reading from stdin…");
    let mut s = String::new();
    tokio::io::stdin().read_to_string(&mut s).await?;
    Ok(s.trim().to_string())
}

fn first_line(s: &str, n: usize) -> String {
    s.lines().next().unwrap_or("").chars().take(n).collect()
}

fn fmt_time(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    t.map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".into())
}

fn print_runs(runs: &[Run]) {
    println!(
        "{:>6}  {:<10} {:<6} {:<19}  PROMPT",
        "ID", "STATUS", "MODE", "CREATED"
    );
    for r in runs {
        println!(
            "{:>6}  {:<10} {:<6} {:<19}  {}",
            r.id,
            r.status.as_str(),
            r.mode.as_str(),
            fmt_time(Some(r.created_at)),
            first_line(&r.prompt, 60)
        );
    }
}

fn print_agents(agents: &[Agent]) {
    println!(
        "{:>4}  {:<20} {:<6} {:<10} REPO",
        "ID", "NAME", "MODE", "SESSION"
    );
    for a in agents {
        println!(
            "{:>4}  {:<20} {:<6} {:<10} {}",
            a.id,
            a.name,
            a.mode.as_str(),
            if a.persistent_session {
                "persistent"
            } else {
                "fresh"
            },
            a.repo.as_deref().unwrap_or("-")
        );
    }
}

fn print_jobs(jobs: &[Job]) {
    println!(
        "{:>4}  {:<20} {:>5}  {:<9} {:<18} {:<3} {:<19}  LAST",
        "ID", "NAME", "AGENT", "KIND", "SCHEDULE", "ON", "NEXT RUN (UTC)"
    );
    for j in jobs {
        println!(
            "{:>4}  {:<20} {:>5}  {:<9} {:<18} {:<3} {:<19}  {}",
            j.id,
            j.name,
            j.agent_id,
            j.schedule_kind.as_str(),
            j.schedule.as_deref().unwrap_or("-"),
            if j.enabled { "yes" } else { "no" },
            fmt_time(j.next_run_at),
            j.last_status.map(|s| s.as_str()).unwrap_or("-")
        );
    }
}

fn print_auth(v: &Value) {
    println!("method:     {}", v["method_label"].as_str().unwrap_or("?"));
    println!("credential: {}", v["credential"].as_str().unwrap_or("none"));
    match (
        v["cli"].get("loggedIn").and_then(Value::as_bool),
        v["cli_error"].as_str(),
    ) {
        (Some(true), _) => println!("cli check:  logged in"),
        (Some(false), _) => println!("cli check:  NOT logged in"),
        (None, Some(e)) => println!("cli check:  {e}"),
        _ => {}
    }
    if let Some(obj) = v["cli"].as_object() {
        for (k, val) in obj {
            if let Some(s) = val.as_str().filter(|_| !k.ends_with("Directory")) {
                println!("  {k}: {s}");
            }
        }
    }
}

pub async fn main(remote: Remote, cmd: Cmd) -> Result<()> {
    let c = Client {
        http: reqwest::Client::new(),
        remote,
    };
    match cmd {
        Cmd::Run {
            opts,
            timeout,
            follow,
            prompt,
        } => {
            let mut body = opts.json();
            body["prompt"] = json!(prompt_or_stdin(prompt).await?);
            body["timeout_secs"] = json!(timeout);
            let run: Run = c.post("/runs", body).await?;
            c.finish(&run, follow).await?;
        }
        Cmd::Ps { limit, job, agent } => {
            let mut path = format!("/runs?limit={limit}");
            if let Some(j) = job {
                path.push_str(&format!("&job_id={j}"));
            }
            if let Some(a) = agent {
                path.push_str(&format!("&agent_id={a}"));
            }
            print_runs(&c.get::<Vec<Run>>(&path).await?);
        }
        Cmd::Show { id } => {
            let run: Value = c.get(&format!("/runs/{id}")).await?;
            println!("{}", serde_json::to_string_pretty(&run)?);
        }
        Cmd::Logs { id, follow } => {
            if follow {
                let status = c.follow(id).await?;
                if status != "succeeded" && status != "dispatched" {
                    bail!("run {id} {status}");
                }
            } else {
                let text = c
                    .req(Method::GET, &format!("/runs/{id}/log"))
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?;
                print!("{text}");
            }
        }
        Cmd::Cancel { id } => {
            let v: Value = c.post(&format!("/runs/{id}/cancel"), json!({})).await?;
            if v["cancelled"].as_bool() != Some(true) {
                bail!("run {id} is not active");
            }
            eprintln!("cancelled run {id}");
        }
        Cmd::Resume { id, follow, prompt } => {
            let prompt = prompt_or_stdin(prompt).await?;
            let run: Run = c
                .post(&format!("/runs/{id}/followup"), json!({ "prompt": prompt }))
                .await?;
            c.finish(&run, follow).await?;
        }
        Cmd::Rerun { id, follow } => {
            let run: Run = c.post(&format!("/runs/{id}/rerun"), json!({})).await?;
            c.finish(&run, follow).await?;
        }
        Cmd::Agent(cmd) => agent_cmd(&c, cmd).await?,
        Cmd::Job(cmd) => job_cmd(&c, cmd).await?,
        Cmd::Auth(cmd) => auth_cmd(&c, cmd).await?,
    }
    Ok(())
}

async fn agent_cmd(c: &Client, cmd: AgentCmd) -> Result<()> {
    match cmd {
        AgentCmd::Ls => print_agents(&c.get::<Vec<Agent>>("/agents").await?),
        AgentCmd::Add {
            name,
            description,
            persistent,
            opts,
        } => {
            let mut body = opts.json();
            body["name"] = json!(name);
            body["description"] = json!(description);
            body["persistent_session"] = json!(persistent);
            let agent: Agent = c.post("/agents", body).await?;
            print_agents(&[agent]);
        }
        AgentCmd::Show { agent } => {
            let v: Value = c.get(&format!("/agents/{agent}")).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        AgentCmd::Rm { agent } => {
            c.delete(&format!("/agents/{agent}")).await?;
            eprintln!("deleted agent {agent}");
        }
        AgentCmd::Run {
            agent,
            follow,
            prompt,
        } => {
            let prompt = prompt_or_stdin(prompt).await?;
            let run: Run = c
                .post(&format!("/agents/{agent}/run"), json!({ "prompt": prompt }))
                .await?;
            c.finish(&run, follow).await?;
        }
        AgentCmd::Reset { agent } => {
            let _: Agent = c
                .post(&format!("/agents/{agent}/reset-session"), json!({}))
                .await?;
            eprintln!("agent {agent} will start a fresh session next run");
        }
    }
    Ok(())
}

async fn job_cmd(c: &Client, cmd: JobCmd) -> Result<()> {
    match cmd {
        JobCmd::Ls { agent } => {
            let path = match agent {
                Some(a) => {
                    let agent: Agent = c.get(&format!("/agents/{a}")).await?;
                    format!("/jobs?agent_id={}", agent.id)
                }
                None => "/jobs".into(),
            };
            print_jobs(&c.get::<Vec<Job>>(&path).await?);
        }
        JobCmd::Add {
            name,
            agent,
            schedule,
            overlap,
            timeout,
            retries,
            retry_delay,
            jitter,
            then,
            disabled,
            prompt,
        } => {
            let agent: Agent = c.get(&format!("/agents/{agent}")).await?;
            let mut body = schedule.json();
            body["name"] = json!(name);
            body["agent_id"] = json!(agent.id);
            body["prompt"] = json!(prompt_or_stdin(prompt).await?);
            body["overlap"] = json!(overlap);
            body["timeout_secs"] = json!(timeout);
            body["max_retries"] = json!(retries);
            body["retry_delay_secs"] = json!(retry_delay);
            body["jitter_secs"] = json!(jitter);
            body["then_job_id"] = json!(then);
            body["enabled"] = json!(!disabled);
            let job: Job = c.post("/jobs", body).await?;
            print_jobs(&[job]);
        }
        JobCmd::Show { id } => {
            let v: Value = c.get(&format!("/jobs/{id}")).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        JobCmd::Rm { id } => {
            c.delete(&format!("/jobs/{id}")).await?;
            eprintln!("deleted job {id}");
        }
        JobCmd::Trigger { id, follow } => {
            let run: Run = c.post(&format!("/jobs/{id}/trigger"), json!({})).await?;
            c.finish(&run, follow).await?;
        }
        JobCmd::Enable { id } => print_jobs(&[c
            .post::<Job>(&format!("/jobs/{id}/enable"), json!({}))
            .await?]),
        JobCmd::Disable { id } => print_jobs(&[c
            .post::<Job>(&format!("/jobs/{id}/disable"), json!({}))
            .await?]),
        JobCmd::Preview { schedule, count } => {
            let mut body = schedule.json();
            body["count"] = json!(count);
            let v: Value = c.post("/jobs/preview", body).await?;
            println!("{}", v["description"].as_str().unwrap_or(""));
            for t in v["next"].as_array().into_iter().flatten() {
                println!("  {}", t.as_str().unwrap_or(""));
            }
        }
    }
    Ok(())
}

async fn auth_cmd(c: &Client, cmd: AuthCmd) -> Result<()> {
    match cmd {
        AuthCmd::Status => print_auth(&c.get::<Value>("/auth").await?),
        AuthCmd::SetKey => print_auth(
            &c.post("/auth/api-key", json!({ "value": read_secret().await? }))
                .await?,
        ),
        AuthCmd::SetToken => print_auth(
            &c.post(
                "/auth/oauth-token",
                json!({ "value": read_secret().await? }),
            )
            .await?,
        ),
        AuthCmd::UseEnv => print_auth(&c.post("/auth/use-environment", json!({})).await?),
        AuthCmd::Logout => print_auth(&c.post("/auth/sign-out", json!({})).await?),
        AuthCmd::Login { token_only } => {
            let kind = if token_only { "setup_token" } else { "login" };
            let flow: Value = c.post("/auth/flow", json!({ "kind": kind })).await?;
            let url = match flow["state"].as_str() {
                Some("awaiting_code") => flow["url"].as_str().unwrap_or_default().to_string(),
                Some("failed") => bail!("{}", flow["message"].as_str().unwrap_or("sign-in failed")),
                _ => bail!("the server did not produce a sign-in URL; check its logs"),
            };
            eprintln!("1. Open this URL and approve access:\n\n   {url}\n");
            eprint!("2. Paste the code shown after signing in: ");
            let mut line = String::new();
            tokio::io::BufReader::new(tokio::io::stdin())
                .read_line(&mut line)
                .await?;
            let _: Value = c
                .post("/auth/flow/code", json!({ "code": line.trim() }))
                .await?;
            for _ in 0..120 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let flow: Value = c.get("/auth/flow").await?;
                match flow["state"].as_str() {
                    Some("succeeded") => {
                        eprintln!("✓ {}", flow["message"].as_str().unwrap_or("signed in"));
                        print_auth(&c.get::<Value>("/auth").await?);
                        return Ok(());
                    }
                    Some("failed") => {
                        bail!("{}", flow["message"].as_str().unwrap_or("sign-in failed"))
                    }
                    _ => {}
                }
            }
            bail!("timed out waiting for sign-in to finish");
        }
    }
    Ok(())
}
