//! Terminal client: talks to a (possibly remote) ctm server over its JSON API.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures::StreamExt;
use reqwest::{Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::db::{Mode, Run, Task};

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

#[derive(Args, Debug)]
pub struct RunOpts {
    /// Run inside a Docker container instead of a local process.
    #[arg(long)]
    pub docker: bool,
    /// Docker image (docker mode only).
    #[arg(long)]
    pub image: Option<String>,
    /// Git repository to clone into the workspace.
    #[arg(long)]
    pub repo: Option<String>,
    /// Claude model alias or id.
    #[arg(long)]
    pub model: Option<String>,
    /// Extra arguments for `claude`, as one string.
    #[arg(long, allow_hyphen_values = true)]
    pub extra_args: Option<String>,
}

impl RunOpts {
    fn spec(&self, prompt: String) -> serde_json::Value {
        json!({
            "prompt": prompt,
            "mode": if self.docker { Mode::Docker } else { Mode::Local },
            "image": self.image,
            "repo": self.repo,
            "model": self.model,
            "extra_args": self.extra_args,
        })
    }
}

#[derive(Subcommand, Debug)]
pub enum TaskCmd {
    /// List scheduled tasks.
    Ls,
    /// Create a task. Prompt is read from stdin when omitted.
    Add {
        #[arg(long)]
        name: String,
        /// Cron expression in UTC (omit for a manual-only task).
        #[arg(long)]
        cron: Option<String>,
        /// Create the task disabled.
        #[arg(long)]
        disabled: bool,
        #[command(flatten)]
        opts: RunOpts,
        prompt: Option<String>,
    },
    /// Delete a task (its runs are kept).
    Rm {
        id: i64,
    },
    /// Queue a run of the task right now.
    Trigger {
        id: i64,
        /// Stream the run's output.
        #[arg(short, long)]
        follow: bool,
    },
    Enable {
        id: i64,
    },
    Disable {
        id: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Queue a prompt. Reads the prompt from stdin when omitted.
    Run {
        #[command(flatten)]
        opts: RunOpts,
        /// Stream the output until the run finishes.
        #[arg(short, long)]
        follow: bool,
        prompt: Option<String>,
    },
    /// List recent runs.
    Ps {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: i64,
        /// Only runs of this task.
        #[arg(long)]
        task: Option<i64>,
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
    /// Manage scheduled tasks.
    #[command(subcommand)]
    Task(TaskCmd),
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
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or(body);
            bail!("{status}: {msg}");
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(serde_json::from_value(serde_json::Value::Null)?);
        }
        Ok(resp.json().await?)
    }

    /// Streams the log to stdout; returns the final status line.
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
}

async fn prompt_or_stdin(prompt: Option<String>) -> Result<String> {
    if let Some(p) = prompt {
        return Ok(p);
    }
    use tokio::io::AsyncReadExt;
    let mut s = String::new();
    tokio::io::stdin().read_to_string(&mut s).await?;
    if s.trim().is_empty() {
        bail!("no prompt given (pass it as an argument or on stdin)");
    }
    Ok(s)
}

fn print_runs(runs: &[Run]) {
    println!(
        "{:>6}  {:<10} {:<7} {:<20} PROMPT",
        "ID", "STATUS", "MODE", "CREATED"
    );
    for r in runs {
        let prompt: String = r
            .prompt
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        println!(
            "{:>6}  {:<10} {:<7} {:<20} {}",
            r.id,
            r.status.as_str(),
            r.mode.as_str(),
            r.created_at.format("%Y-%m-%d %H:%M:%S"),
            prompt
        );
    }
}

fn print_tasks(tasks: &[Task]) {
    println!(
        "{:>4}  {:<20} {:<16} {:<7} {:<3} NEXT RUN",
        "ID", "NAME", "CRON", "MODE", "ON"
    );
    for t in tasks {
        println!(
            "{:>4}  {:<20} {:<16} {:<7} {:<3} {}",
            t.id,
            t.name,
            t.cron.as_deref().unwrap_or("manual"),
            t.mode.as_str(),
            if t.enabled { "yes" } else { "no" },
            t.next_run_at
                .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "-".into())
        );
    }
}

/// Follows a run and turns a failed outcome into a non-zero exit.
async fn follow_and_exit(c: &Client, id: i64) -> Result<()> {
    let status = c.follow(id).await?;
    if status != "succeeded" {
        bail!("run {id} {status}");
    }
    Ok(())
}

pub async fn main(remote: Remote, cmd: Cmd) -> Result<()> {
    let c = Client {
        http: reqwest::Client::new(),
        remote,
    };
    match cmd {
        Cmd::Run {
            opts,
            follow,
            prompt,
        } => {
            let prompt = prompt_or_stdin(prompt).await?;
            let run: Run = c
                .send(c.req(Method::POST, "/runs").json(&opts.spec(prompt)))
                .await?;
            eprintln!("queued run {}", run.id);
            if follow {
                follow_and_exit(&c, run.id).await?;
            } else {
                println!("{}", run.id);
            }
        }
        Cmd::Ps { limit, task } => {
            let mut path = format!("/runs?limit={limit}");
            if let Some(t) = task {
                path.push_str(&format!("&task_id={t}"));
            }
            let runs: Vec<Run> = c.send(c.req(Method::GET, &path)).await?;
            print_runs(&runs);
        }
        Cmd::Show { id } => {
            let run: Run = c.send(c.req(Method::GET, &format!("/runs/{id}"))).await?;
            println!("{}", serde_json::to_string_pretty(&run)?);
        }
        Cmd::Logs { id, follow } => {
            if follow {
                follow_and_exit(&c, id).await?;
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
            let v: serde_json::Value = c
                .send(c.req(Method::POST, &format!("/runs/{id}/cancel")))
                .await?;
            if v["cancelled"].as_bool() == Some(true) {
                eprintln!("cancelled run {id}");
            } else {
                bail!("run {id} is not active");
            }
        }
        Cmd::Resume { id, follow, prompt } => {
            let prompt = prompt_or_stdin(prompt).await?;
            let run: Run = c
                .send(
                    c.req(Method::POST, &format!("/runs/{id}/followup"))
                        .json(&json!({ "prompt": prompt })),
                )
                .await?;
            eprintln!("queued run {} (follow-up of {id})", run.id);
            if follow {
                follow_and_exit(&c, run.id).await?;
            } else {
                println!("{}", run.id);
            }
        }
        Cmd::Task(t) => match t {
            TaskCmd::Ls => {
                let tasks: Vec<Task> = c.send(c.req(Method::GET, "/tasks")).await?;
                print_tasks(&tasks);
            }
            TaskCmd::Add {
                name,
                cron,
                disabled,
                opts,
                prompt,
            } => {
                let prompt = prompt_or_stdin(prompt).await?;
                let mut body = opts.spec(prompt);
                body["name"] = json!(name);
                body["cron"] = json!(cron);
                body["enabled"] = json!(!disabled);
                let task: Task = c.send(c.req(Method::POST, "/tasks").json(&body)).await?;
                print_tasks(&[task]);
            }
            TaskCmd::Rm { id } => {
                let _: serde_json::Value = c
                    .send(c.req(Method::DELETE, &format!("/tasks/{id}")))
                    .await?;
                eprintln!("deleted task {id}");
            }
            TaskCmd::Trigger { id, follow } => {
                let run: Run = c
                    .send(c.req(Method::POST, &format!("/tasks/{id}/trigger")))
                    .await?;
                eprintln!("queued run {}", run.id);
                if follow {
                    follow_and_exit(&c, run.id).await?;
                } else {
                    println!("{}", run.id);
                }
            }
            TaskCmd::Enable { id } | TaskCmd::Disable { id } => {
                let action = if matches!(t, TaskCmd::Enable { .. }) {
                    "enable"
                } else {
                    "disable"
                };
                let task: Task = c
                    .send(c.req(Method::POST, &format!("/tasks/{id}/{action}")))
                    .await?;
                print_tasks(&[task]);
            }
        },
    }
    Ok(())
}
