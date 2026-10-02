//! Runs queued Claude Code jobs as local processes, inside throw-away Docker
//! containers, or as Claude cloud sessions, and fans their output out to
//! log files and live subscribers (dashboard SSE, `ctm logs -f`).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use regex::Regex;
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Notify, Semaphore, broadcast, mpsc},
    time::Instant,
};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    claude_auth::{ClaudeAuth, Credentials},
    config::{Config, split_args},
    db::{Db, Mode, Run, RunStatus},
    pty::{PtyProcess, ScreenLines, first_hyperlink, strip_ansi},
};

/// Prefix for lines ctm itself writes into a run log.
pub const CTM_PREFIX: &str = "[ctm] ";
/// Prefix for lines the process wrote to stderr.
pub const STDERR_PREFIX: &str = "[stderr] ";
/// Prefix for text captured from an interactive (PTY) client.
pub const TTY_PREFIX: &str = "[tty] ";

/// Named volume that holds Claude's session state for docker runs, so
/// follow-ups (`--resume`) work across containers.
const CLAUDE_HOME_VOLUME: &str = "ctm-claude-home";

/// How long `claude --cloud` may take to report its new session.
const CLOUD_CREATE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub enum LogEvent {
    Line { seq: u64, text: String },
    Done(RunStatus),
}

struct LiveRun {
    events: broadcast::Sender<LogEvent>,
    cancel: Arc<Notify>,
}

struct Outcome {
    status: RunStatus,
    exit_code: Option<i64>,
    error: Option<String>,
}

impl Outcome {
    fn ok(status: RunStatus, exit_code: Option<i64>) -> Self {
        Self {
            status,
            exit_code,
            error: None,
        }
    }

    fn failed(exit_code: Option<i64>, error: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Failed,
            exit_code,
            error: Some(error.into()),
        }
    }

    fn cancelled(exit_code: Option<i64>) -> Self {
        Self {
            status: RunStatus::Cancelled,
            exit_code,
            error: Some("cancelled".into()),
        }
    }
}

pub struct Executor {
    cfg: Arc<Config>,
    db: Db,
    auth: Arc<ClaudeAuth>,
    queue: mpsc::UnboundedSender<i64>,
    live: Mutex<HashMap<i64, LiveRun>>,
    /// One run at a time per workspace: two Claudes editing the same
    /// checkout (or resuming the same session) would trample each other.
    workspace_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Executor {
    /// Creates the executor and starts its dispatcher. Runs left `running`
    /// by a previous process are failed; `queued` ones are picked up again.
    pub async fn start(cfg: Arc<Config>, db: Db, auth: Arc<ClaudeAuth>) -> Result<Arc<Self>> {
        fs::create_dir_all(cfg.logs_dir()).await?;
        fs::create_dir_all(cfg.workspaces_dir()).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let exec = Arc::new(Self {
            cfg,
            db,
            auth,
            queue: tx,
            live: Mutex::new(HashMap::new()),
            workspace_locks: Mutex::new(HashMap::new()),
        });

        let failed = exec.db.fail_interrupted().await?;
        if failed > 0 {
            tracing::warn!(failed, "marked interrupted runs as failed");
        }
        for run in exec.db.runs_with_status(RunStatus::Queued).await? {
            exec.enqueue(run.id);
        }

        tokio::spawn(exec.clone().dispatch(rx));
        Ok(exec)
    }

    /// Hands a freshly created (queued) run to the dispatcher.
    pub fn enqueue(&self, run_id: i64) {
        let (events, _) = broadcast::channel(1024);
        self.live.lock().unwrap().insert(
            run_id,
            LiveRun {
                events,
                cancel: Arc::new(Notify::new()),
            },
        );
        let _ = self.queue.send(run_id);
    }

    pub fn active_count(&self) -> usize {
        self.live.lock().unwrap().len()
    }

    /// Cancels a queued or running run. Returns false if it already ended.
    pub async fn cancel(&self, run_id: i64) -> Result<bool> {
        let Some(run) = self.db.get_run(run_id).await? else {
            return Ok(false);
        };
        let cancel = self
            .live
            .lock()
            .unwrap()
            .get(&run_id)
            .map(|l| l.cancel.clone());
        match run.status {
            RunStatus::Queued => {
                self.db
                    .mark_finished(
                        run_id,
                        RunStatus::Cancelled,
                        None,
                        Some("cancelled before start".into()),
                    )
                    .await?;
                if let Some(c) = cancel {
                    c.notify_one();
                }
                self.finish_live(run_id, RunStatus::Cancelled);
                Ok(true)
            }
            RunStatus::Running => match cancel {
                Some(c) => {
                    c.notify_one();
                    Ok(true)
                }
                None => Ok(false),
            },
            _ => Ok(false),
        }
    }

    /// Streams a run's log: everything written so far, then live lines,
    /// then a final `Done` event carrying the terminal status.
    pub async fn follow(self: &Arc<Self>, run_id: i64) -> Result<ReceiverStream<LogEvent>> {
        let (tx, rx) = mpsc::channel(256);
        // Subscribe before reading the file so nothing falls in between;
        // duplicates are dropped by sequence number.
        let sub = self
            .live
            .lock()
            .unwrap()
            .get(&run_id)
            .map(|l| l.events.subscribe());
        let path = self.cfg.log_path(run_id);
        let db = self.db.clone();

        tokio::spawn(async move {
            let mut next: u64 = 0;
            macro_rules! send_file_from_next {
                () => {
                    for text in read_lines_from(&path, next).await {
                        if tx.send(LogEvent::Line { seq: next, text }).await.is_err() {
                            return;
                        }
                        next += 1;
                    }
                };
            }
            let final_status = || async {
                db.get_run(run_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.status)
                    .unwrap_or(RunStatus::Failed)
            };
            send_file_from_next!();
            let Some(mut sub) = sub else {
                let _ = tx.send(LogEvent::Done(final_status().await)).await;
                return;
            };
            loop {
                match sub.recv().await {
                    Ok(LogEvent::Line { seq, text }) => {
                        if seq > next {
                            // We lagged: catch up from the file.
                            send_file_from_next!();
                        }
                        if seq < next {
                            continue;
                        }
                        if tx.send(LogEvent::Line { seq, text }).await.is_err() {
                            return;
                        }
                        next = seq + 1;
                    }
                    Ok(done @ LogEvent::Done(_)) => {
                        send_file_from_next!();
                        let _ = tx.send(done).await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        send_file_from_next!();
                        let _ = tx.send(LogEvent::Done(final_status().await)).await;
                        return;
                    }
                }
            }
        });
        Ok(ReceiverStream::new(rx))
    }

    fn finish_live(&self, run_id: i64, status: RunStatus) {
        if let Some(live) = self.live.lock().unwrap().remove(&run_id) {
            let _ = live.events.send(LogEvent::Done(status));
        }
    }

    fn workspace_lock(&self, workspace: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.workspace_locks
            .lock()
            .unwrap()
            .entry(workspace.to_string())
            .or_default()
            .clone()
    }

    async fn dispatch(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<i64>) {
        let slots = Arc::new(Semaphore::new(self.cfg.max_concurrent.max(1)));
        while let Some(run_id) = rx.recv().await {
            let exec = self.clone();
            let slots = slots.clone();
            tokio::spawn(async move {
                let Ok(Some(run)) = exec.db.get_run(run_id).await else {
                    return;
                };
                let cancel = exec
                    .live
                    .lock()
                    .unwrap()
                    .get(&run_id)
                    .map(|l| l.cancel.clone());
                let cancel = cancel.unwrap_or_default();

                // Delayed runs (retries) wait here, still cancellable.
                if let Some(at) = run.not_before {
                    let wait = (at - Utc::now()).to_std().unwrap_or_default();
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = cancel.notified() => return,
                    }
                }
                let lock = exec.workspace_lock(&run.workspace);
                let _guard = tokio::select! {
                    g = lock.lock_owned() => g,
                    _ = cancel.notified() => return,
                };
                let _permit = tokio::select! {
                    p = slots.acquire_owned() => p.expect("semaphore closed"),
                    _ = cancel.notified() => return,
                };
                exec.execute(run_id, cancel).await;
            });
        }
    }

    async fn execute(self: &Arc<Self>, run_id: i64, cancel: Arc<Notify>) {
        match self.db.mark_running(run_id).await {
            Ok(true) => {}
            Ok(false) => {
                // Cancelled while queued.
                let status = self
                    .db
                    .get_run(run_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.status)
                    .unwrap_or(RunStatus::Cancelled);
                self.finish_live(run_id, status);
                return;
            }
            Err(e) => {
                tracing::error!(run_id, "mark_running: {e:#}");
                return;
            }
        }

        let events = {
            let live = self.live.lock().unwrap();
            match live.get(&run_id) {
                Some(l) => l.events.clone(),
                None => broadcast::channel(1024).0,
            }
        };

        let outcome = match RunLog::create(&self.cfg.log_path(run_id), events).await {
            Ok(mut log) => {
                let outcome = match self.execute_inner(run_id, &mut log, &cancel).await {
                    Ok(o) => o,
                    Err(e) => {
                        log.line(format!("{CTM_PREFIX}error: {e:#}")).await;
                        Outcome::failed(None, format!("{e:#}"))
                    }
                };
                log.line(format!("{CTM_PREFIX}finished: {}", outcome.status))
                    .await;
                outcome
            }
            Err(e) => Outcome::failed(None, format!("{e:#}")),
        };

        if let Err(e) = self
            .db
            .mark_finished(run_id, outcome.status, outcome.exit_code, outcome.error)
            .await
        {
            tracing::error!(run_id, "mark_finished: {e:#}");
        }
        self.finish_live(run_id, outcome.status);
        tracing::info!(run_id, status = %outcome.status, "run finished");

        if let Ok(Some(run)) = self.db.get_run(run_id).await
            && let Err(e) = crate::jobs::after_run(&self.db, self, &run).await
        {
            tracing::error!(run_id, "after-run hooks: {e:#}");
        }
    }

    async fn execute_inner(
        &self,
        run_id: i64,
        log: &mut RunLog,
        cancel: &Notify,
    ) -> Result<Outcome> {
        let run = self.db.get_run(run_id).await?.context("run disappeared")?;
        let creds = self.auth.credentials().await?;
        let deadline = run
            .timeout_secs
            .map(|s| Instant::now() + Duration::from_secs(s.max(1) as u64));

        if run.mode == Mode::Cloud {
            return self
                .execute_cloud(&run, &creds, log, cancel, deadline)
                .await;
        }

        let claude_args = claude_args(&self.cfg, &run);
        let mut cmd = match run.mode {
            Mode::Docker => self.docker_command(&run, &claude_args, &creds),
            _ => {
                let dir = self.prepare_workspace(&run, log).await?;
                let mut cmd = Command::new(&self.cfg.claude_bin);
                cmd.args(&claude_args).current_dir(&dir);
                cmd.env("CTM_RUN_ID", run.id.to_string());
                creds.apply(&mut cmd);
                cmd
            }
        };
        log.line(format!(
            "{CTM_PREFIX}starting {} run in workspace {}{}",
            run.mode,
            run.workspace,
            run.resume_session
                .as_deref()
                .map(|s| format!(" (resuming session {s})"))
                .unwrap_or_default()
        ))
        .await;

        let sup = self
            .supervise(&run, &mut cmd, log, cancel, deadline, Parse::StreamJson)
            .await?;
        if sup.summary.result.is_some() || sup.summary.cost.is_some() {
            self.db
                .set_result(run.id, sup.summary.result.as_deref(), sup.summary.cost)
                .await?;
        }
        let code = sup.exit.code().map(i64::from);
        Ok(if sup.cancelled {
            Outcome::cancelled(code)
        } else if sup.timed_out {
            Outcome::failed(
                code,
                format!("timed out after {}s", run.timeout_secs.unwrap_or(0)),
            )
        } else if sup.exit.success() && !sup.summary.is_error {
            Outcome::ok(RunStatus::Succeeded, code)
        } else if sup.exit.success() {
            Outcome::failed(code, "claude reported an error result")
        } else {
            Outcome::failed(code, format!("process exited with {}", sup.exit))
        })
    }

    /// Cloud mode. With a target session (an explicit one, or the parent's /
    /// agent's previous session) the prompt is sent to it headlessly with
    /// `claude -p --cloud <id>`. Otherwise a new session is created with
    /// `claude --cloud`, which is interactive-only, so it runs in a pseudo
    /// terminal until the session URL appears; the run is then `dispatched`
    /// and the session carries on on Anthropic's infrastructure.
    async fn execute_cloud(
        &self,
        run: &Run,
        creds: &Credentials,
        log: &mut RunLog,
        cancel: &Notify,
        deadline: Option<Instant>,
    ) -> Result<Outcome> {
        // Claude keys per-folder state by real path.
        let dir = fs::canonicalize(self.prepare_workspace(run, log).await?).await?;
        let target = run
            .resume_session
            .as_deref()
            .or(run.cloud_session.as_deref())
            .map(|t| cloud_session_id(t).unwrap_or_else(|| t.to_string()));

        if let Some(target) = target {
            log.line(format!(
                "{CTM_PREFIX}sending prompt to cloud session {target}"
            ))
            .await;
            self.note_session(run, &target, Some(&cloud_session_url(&target)))
                .await;
            let mut cmd = Command::new(&self.cfg.claude_bin);
            cmd.args(["-p", "--cloud", &target]).current_dir(&dir);
            if let Some(extra) = &run.extra_args {
                cmd.args(split_args(extra));
            }
            creds.apply(&mut cmd);
            let sup = self
                .supervise(run, &mut cmd, log, cancel, deadline, Parse::Text)
                .await?;
            let text = sup.stdout.trim();
            if !text.is_empty() {
                let text: String = text.chars().take(20_000).collect();
                self.db.set_result(run.id, Some(&text), None).await?;
            }
            let code = sup.exit.code().map(i64::from);
            return Ok(if sup.cancelled {
                Outcome::cancelled(code)
            } else if sup.timed_out {
                Outcome::failed(code, "timed out")
            } else if sup.exit.success() {
                Outcome::ok(RunStatus::Succeeded, code)
            } else {
                Outcome::failed(code, format!("claude -p --cloud exited with {}", sup.exit))
            });
        }

        let mut args = vec![format!("--cloud={}", run.prompt)];
        if let Some(env) = &run.environment {
            args.extend(["--environment".into(), env.clone()]);
        }
        if let Some(extra) = &run.extra_args {
            args.extend(split_args(extra));
        }
        log.line(format!(
            "{CTM_PREFIX}creating a cloud session{}",
            run.repo
                .as_deref()
                .map(|r| format!(" for {r}"))
                .unwrap_or_default()
        ))
        .await;
        self.auth.prepare_interactive(&dir).await?;
        let mut spawned = PtyProcess::spawn(
            &self.cfg.claude_bin,
            &args,
            &dir,
            &creds.env_pairs(),
            &creds.remove,
        )?;

        let create_deadline = Instant::now() + CLOUD_CREATE_TIMEOUT;
        let deadline = deadline.map_or(create_deadline, |d| d.min(create_deadline));
        let mut screen = ScreenLines::default();
        let mut raw_tail = String::new();
        let mut found = false;
        let mut grace: Option<Instant> = None;
        let outcome = loop {
            tokio::select! {
                chunk = spawned.output.recv() => {
                    let Some(chunk) = chunk else {
                        break if found {
                            Outcome::ok(RunStatus::Dispatched, None)
                        } else {
                            Outcome::failed(None, "claude --cloud exited before a session was created")
                        };
                    };
                    raw_tail.push_str(&chunk);
                    if raw_tail.len() > 64 * 1024 {
                        let mut cut = raw_tail.len() - 32 * 1024;
                        while !raw_tail.is_char_boundary(cut) {
                            cut += 1;
                        }
                        raw_tail.drain(..cut);
                    }
                    for line in screen.push(&chunk) {
                        log.line(format!("{TTY_PREFIX}{line}")).await;
                    }
                    if !found
                        && let Some((id, url)) = find_cloud_session(&raw_tail) {
                            log.line(format!("{CTM_PREFIX}cloud session started: {url}")).await;
                            self.note_session(run, &id, Some(&url)).await;
                            self.db.set_result(run.id, Some(&format!("Cloud session: {url}")), None).await?;
                            found = true;
                            // Let the client finish handing off, then detach.
                            grace = Some(Instant::now() + Duration::from_secs(5));
                        }
                }
                _ = tokio::time::sleep_until(grace.unwrap_or(deadline)) => {
                    break if found {
                        log.line(format!("{CTM_PREFIX}detaching; the session keeps running in the cloud")).await;
                        Outcome::ok(RunStatus::Dispatched, None)
                    } else {
                        Outcome::failed(
                            None,
                            "no cloud session was reported in time (see the [tty] lines for anything it was waiting on)",
                        )
                    };
                }
                _ = cancel.notified() => {
                    log.line(format!("{CTM_PREFIX}cancelling")).await;
                    break Outcome::cancelled(None);
                }
            }
        };
        spawned.process.kill();
        Ok(outcome)
    }

    /// Records a session id on the run and, for agent runs, on the agent so
    /// persistent agents can resume it next time.
    async fn note_session(&self, run: &Run, session_id: &str, url: Option<&str>) {
        if let Err(e) = self.db.set_session(run.id, session_id, url).await {
            tracing::warn!(run = run.id, "saving session id: {e:#}");
        }
        if let Some(agent) = run.agent_id {
            let _ = self.db.set_agent_session(agent, Some(session_id)).await;
        }
    }

    /// Runs a child process to completion, logging its output.
    async fn supervise(
        &self,
        run: &Run,
        cmd: &mut Command,
        log: &mut RunLog,
        cancel: &Notify,
        deadline: Option<Instant>,
        parse: Parse,
    ) -> Result<Supervised> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn {:?}",
                cmd.as_std().get_program().to_string_lossy()
            )
        })?;

        // The prompt goes in over stdin: no quoting or argv-length issues.
        let mut stdin = child.stdin.take().context("no stdin")?;
        let prompt = run.prompt.clone();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });

        let (line_tx, mut line_rx) = mpsc::channel::<(bool, String)>(256);
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        tokio::spawn(pump(stdout, false, line_tx.clone()));
        tokio::spawn(pump(stderr, true, line_tx));

        let mut summary = StreamSummary::default();
        let mut stdout_text = String::new();
        let mut exit = None;
        let (mut cancelled, mut timed_out) = (false, false);
        loop {
            tokio::select! {
                line = line_rx.recv() => match line {
                    Some((true, line)) => log.line(format!("{STDERR_PREFIX}{line}")).await,
                    Some((false, line)) => {
                        match parse {
                            Parse::StreamJson => {
                                if let Some(session) = summary.observe(&line) {
                                    self.note_session(run, &session, None).await;
                                }
                            }
                            Parse::Text if stdout_text.len() < 1 << 20 => {
                                stdout_text.push_str(&line);
                                stdout_text.push('\n');
                            }
                            Parse::Text => {}
                        }
                        log.line(line).await;
                    }
                    None => break, // both pipes closed
                },
                status = child.wait(), if exit.is_none() => {
                    exit = Some(status?);
                }
                _ = cancel.notified(), if !cancelled && !timed_out => {
                    cancelled = true;
                    log.line(format!("{CTM_PREFIX}cancelling")).await;
                    self.kill(run, &mut child).await;
                }
                _ = sleep_until_opt(deadline), if !cancelled && !timed_out => {
                    timed_out = true;
                    log.line(format!("{CTM_PREFIX}timeout reached; stopping")).await;
                    self.kill(run, &mut child).await;
                }
            }
        }
        let exit = match exit {
            Some(e) => e,
            None => child.wait().await?,
        };
        Ok(Supervised {
            exit,
            cancelled,
            timed_out,
            summary,
            stdout: stdout_text,
        })
    }

    async fn kill(&self, run: &Run, child: &mut tokio::process::Child) {
        if run.mode == Mode::Docker {
            // Killing the docker client alone would leave the container running.
            let _ = Command::new(&self.cfg.docker_bin)
                .args(["kill", &container_name(run.id)])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        let _ = child.start_kill();
    }

    /// Creates the local workspace directory, cloning `repo` into it once.
    async fn prepare_workspace(&self, run: &Run, log: &mut RunLog) -> Result<PathBuf> {
        let dir = self.cfg.workspaces_dir().join(&run.workspace);
        fs::create_dir_all(&dir).await?;
        if let Some(repo) = run.repo.as_ref().filter(|_| !dir.join(".git").exists()) {
            log.line(format!("{CTM_PREFIX}cloning {repo}")).await;
            let out = Command::new("git")
                .arg("clone")
                .arg("--quiet")
                .arg(repo)
                .arg(&dir)
                .output()
                .await
                .context("running git clone")?;
            for l in String::from_utf8_lossy(&out.stderr).lines() {
                log.line(format!("{STDERR_PREFIX}{l}")).await;
            }
            anyhow::ensure!(out.status.success(), "git clone failed ({})", out.status);
        }
        Ok(dir)
    }

    fn docker_command(&self, run: &Run, claude_args: &[String], creds: &Credentials) -> Command {
        let mut cmd = Command::new(&self.cfg.docker_bin);
        cmd.args(["run", "--rm", "-i"])
            .args(["--name", &container_name(run.id)])
            .args(["--label", &format!("ctm.run={}", run.id)])
            .args(["-v", &format!("{}:/workspace", volume_name(&run.workspace))])
            .args(["-v", &format!("{CLAUDE_HOME_VOLUME}:/home/node/.claude")])
            .args(["-e", &format!("CTM_RUN_ID={}", run.id)]);
        if let Some(repo) = &run.repo {
            cmd.args(["-e", &format!("CTM_REPO={repo}")]);
        }
        // `-e NAME` copies the value from the docker client's environment,
        // so secrets never appear on a command line.
        let mut forwarded: Vec<String> = Vec::new();
        for (k, v) in creds.docker_env() {
            cmd.env(&k, v);
            forwarded.push(k);
        }
        for name in self.cfg.forwarded_env() {
            let shadowed = creds.remove.contains(&name.as_str());
            if !forwarded.contains(&name) && !shadowed && std::env::var_os(&name).is_some() {
                forwarded.push(name);
            }
        }
        for name in &forwarded {
            cmd.args(["-e", name]);
        }
        cmd.args(split_args(&self.cfg.docker_args));
        cmd.arg(run.image.as_deref().unwrap_or(&self.cfg.docker_image));
        cmd.arg("claude").args(claude_args);
        cmd
    }
}

#[derive(Clone, Copy)]
enum Parse {
    /// Claude's `--output-format stream-json`.
    StreamJson,
    /// Plain text (collected as the result).
    Text,
}

struct Supervised {
    exit: ExitStatus,
    cancelled: bool,
    timed_out: bool,
    summary: StreamSummary,
    stdout: String,
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Arguments for a headless (`-p`) local or docker run.
pub fn claude_args(cfg: &Config, run: &Run) -> Vec<String> {
    let mut args = vec!["-p".to_string()];
    args.extend(split_args(&cfg.claude_args));
    for (flag, value) in [
        ("--model", &run.model),
        ("--resume", &run.resume_session),
        ("--append-system-prompt", &run.system_prompt),
        ("--permission-mode", &run.permission_mode),
        ("--allowedTools", &run.allowed_tools),
    ] {
        if let Some(v) = value {
            args.extend([flag.to_string(), v.clone()]);
        }
    }
    if let Some(extra) = &run.extra_args {
        args.extend(split_args(extra));
    }
    args
}

pub fn container_name(run_id: i64) -> String {
    format!("ctm-run-{run_id}")
}

pub fn volume_name(workspace: &str) -> String {
    let safe: String = workspace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("ctm-ws-{safe}")
}

fn session_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b((?:session|cse)_[A-Za-z0-9]{8,}[A-Za-z0-9_]*)").unwrap())
}

/// Extracts a cloud session id from an id or a claude.ai/code URL.
pub fn cloud_session_id(s: &str) -> Option<String> {
    session_re().captures(s).map(|c| c[1].to_string())
}

pub fn cloud_session_url(id: &str) -> String {
    format!("https://claude.ai/code/{id}")
}

/// Finds the session the `claude --cloud` client reports, preferring a
/// claude.ai/code link (hyperlink or plain text).
fn find_cloud_session(raw: &str) -> Option<(String, String)> {
    static URL: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let url_re = URL.get_or_init(|| {
        Regex::new(r#"https://claude\.ai/code/[^\s"'<>\x1b\x07]*?(?:session|cse)_[A-Za-z0-9_]+"#)
            .unwrap()
    });
    let text = strip_ansi(raw);
    let url = first_hyperlink(raw)
        .filter(|u| url_re.is_match(u))
        .or_else(|| url_re.find(&text).map(|m| m.as_str().to_string()))?;
    let id = cloud_session_id(&url)?;
    Some((id, url))
}

async fn pump(
    reader: impl tokio::io::AsyncRead + Unpin,
    is_err: bool,
    tx: mpsc::Sender<(bool, String)>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send((is_err, line)).await.is_err() {
            break;
        }
    }
}

async fn read_lines_from(path: &Path, skip: u64) -> Vec<String> {
    match fs::read_to_string(path).await {
        // Only count newline-terminated lines; a partial trailing line is
        // still being written and will arrive through the live channel.
        Ok(s) => s
            .split_inclusive('\n')
            .filter(|l| l.ends_with('\n'))
            .skip(skip as usize)
            .map(|l| l.trim_end_matches(['\n', '\r']).to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Append-only log file plus the live broadcast for one run.
struct RunLog {
    file: fs::File,
    events: broadcast::Sender<LogEvent>,
    seq: u64,
}

impl RunLog {
    async fn create(path: &Path, events: broadcast::Sender<LogEvent>) -> Result<Self> {
        let file = fs::File::create(path)
            .await
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self {
            file,
            events,
            seq: 0,
        })
    }

    async fn line(&mut self, text: String) {
        // One log entry per physical line keeps sequence numbers and the
        // file's line count in step.
        for text in text.split('\n') {
            let text = text.trim_end_matches('\r').to_string();
            let mut buf = text.clone().into_bytes();
            buf.push(b'\n');
            if let Err(e) = self.file.write_all(&buf).await {
                tracing::warn!("writing run log: {e}");
            }
            let _ = self.file.flush().await;
            let _ = self.events.send(LogEvent::Line {
                seq: self.seq,
                text,
            });
            self.seq += 1;
        }
    }
}

/// Extracts the interesting bits from Claude's `stream-json` output.
#[derive(Default, Debug)]
pub struct StreamSummary {
    pub session_id: Option<String>,
    pub result: Option<String>,
    pub cost: Option<f64>,
    pub is_error: bool,
}

impl StreamSummary {
    /// Feeds one output line; returns a session id the first time one is seen.
    pub fn observe(&mut self, line: &str) -> Option<String> {
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("type").and_then(Value::as_str) == Some("result") {
            self.result = v.get("result").and_then(Value::as_str).map(String::from);
            self.cost = v.get("total_cost_usd").and_then(Value::as_f64);
            self.is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        }
        let sid = v.get("session_id").and_then(Value::as_str)?;
        if self.session_id.as_deref() == Some(sid) {
            return None;
        }
        self.session_id = Some(sid.to_string());
        self.session_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reads_stream_json() {
        let mut s = StreamSummary::default();
        assert_eq!(s.observe("not json"), None);
        assert_eq!(
            s.observe(r#"{"type":"system","subtype":"init","session_id":"abc"}"#),
            Some("abc".into())
        );
        assert_eq!(
            s.observe(r#"{"type":"assistant","session_id":"abc"}"#),
            None
        );
        s.observe(r#"{"type":"result","result":"done!","total_cost_usd":0.12,"is_error":false,"session_id":"abc"}"#);
        assert_eq!(s.result.as_deref(), Some("done!"));
        assert_eq!(s.cost, Some(0.12));
        assert!(!s.is_error);
    }

    #[test]
    fn volume_names_are_safe() {
        assert_eq!(volume_name("agent-3"), "ctm-ws-agent-3");
        assert_eq!(volume_name("a/b c"), "ctm-ws-a-b-c");
    }

    #[test]
    fn cloud_session_parsing() {
        assert_eq!(
            cloud_session_id("https://claude.ai/code/session_01AbCdEfGhIjK?x=1").as_deref(),
            Some("session_01AbCdEfGhIjK")
        );
        assert_eq!(
            cloud_session_id("cse_0123456789ab").as_deref(),
            Some("cse_0123456789ab")
        );
        assert_eq!(cloud_session_id("my session_x"), None);

        let raw = "\x1b[2mStarting…\x1b[0m\r\nMonitor the cloud session at \x1b]8;;https://claude.ai/code/session_01ZZZyyyXXXwww\x1b\\link\x1b]8;;\x1b\\\r\n";
        let (id, url) = find_cloud_session(raw).unwrap();
        assert_eq!(id, "session_01ZZZyyyXXXwww");
        assert_eq!(url, "https://claude.ai/code/session_01ZZZyyyXXXwww");
        assert!(find_cloud_session("Starting a cloud session…").is_none());
    }

    #[test]
    fn args_include_agent_settings() {
        use clap::Parser;
        #[derive(Parser)]
        struct P {
            #[command(flatten)]
            c: Config,
        }
        let cfg = P::parse_from(["x"]).c;
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": 1, "agent_id": null, "job_id": null, "parent_run_id": null, "prompt": "p",
            "mode": "local", "model": "sonnet", "image": null, "repo": null, "environment": null,
            "cloud_session": null, "system_prompt": "be terse", "permission_mode": "acceptEdits",
            "allowed_tools": "Bash(git *) Read", "extra_args": "--max-turns 5",
            "resume_session": "s1", "workspace": "run-1", "status": "queued", "attempt": 1,
            "timeout_secs": null, "not_before": null, "exit_code": null, "session_id": null,
            "session_url": null, "result": null, "cost_usd": null, "error": null,
            "created_at": "2026-01-01T00:00:00Z", "started_at": null, "finished_at": null
        }))
        .unwrap();
        let args = claude_args(&cfg, &run).join(" ");
        assert!(
            args.starts_with("-p --output-format stream-json --verbose --model sonnet --resume s1")
        );
        assert!(args.contains("--append-system-prompt be terse --permission-mode acceptEdits"));
        assert!(args.ends_with("--allowedTools Bash(git *) Read --max-turns 5"));
    }
}
