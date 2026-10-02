//! Runs queued Claude Code jobs, either as local processes or inside
//! throw-away Docker containers, and fans their output out to log files
//! and live subscribers (dashboard SSE, `ctm logs -f`).

use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Notify, Semaphore, broadcast, mpsc},
};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    config::{Config, split_args},
    db::{Db, Mode, Run, RunStatus},
};

/// Prefix for lines ctm itself writes into a run log.
pub const CTM_PREFIX: &str = "[ctm] ";
/// Prefix for lines the process wrote to stderr.
pub const STDERR_PREFIX: &str = "[stderr] ";

/// Named volume that holds Claude's session state for docker runs, so
/// follow-ups (`--resume`) work across containers.
const CLAUDE_HOME_VOLUME: &str = "ctm-claude-home";

#[derive(Debug, Clone)]
pub enum LogEvent {
    Line { seq: u64, text: String },
    Done(RunStatus),
}

struct LiveRun {
    events: broadcast::Sender<LogEvent>,
    cancel: Arc<Notify>,
}

pub struct Executor {
    cfg: Arc<Config>,
    db: Db,
    queue: mpsc::UnboundedSender<i64>,
    live: Mutex<HashMap<i64, LiveRun>>,
}

impl Executor {
    /// Creates the executor and starts its dispatcher. Runs left `running`
    /// by a previous process are failed; `queued` ones are picked up again.
    pub async fn start(cfg: Arc<Config>, db: Db) -> Result<Arc<Self>> {
        fs::create_dir_all(cfg.logs_dir()).await?;
        fs::create_dir_all(cfg.workspaces_dir()).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let exec = Arc::new(Self {
            cfg,
            db,
            queue: tx,
            live: Mutex::new(HashMap::new()),
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
                self.finish_live(run_id, RunStatus::Cancelled);
                Ok(true)
            }
            RunStatus::Running => {
                let cancel = self
                    .live
                    .lock()
                    .unwrap()
                    .get(&run_id)
                    .map(|l| l.cancel.clone());
                match cancel {
                    Some(c) => {
                        c.notify_one();
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
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
        let exec = self.clone();

        tokio::spawn(async move {
            let mut next: u64 = 0;
            let send_from_file = |next: u64| {
                let path = path.clone();
                async move { read_lines_from(&path, next).await }
            };
            for text in send_from_file(0).await {
                if tx.send(LogEvent::Line { seq: next, text }).await.is_err() {
                    return;
                }
                next += 1;
            }
            let Some(mut sub) = sub else {
                let status = exec
                    .db
                    .get_run(run_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|r| r.status)
                    .unwrap_or(RunStatus::Failed);
                let _ = tx.send(LogEvent::Done(status)).await;
                return;
            };
            loop {
                match sub.recv().await {
                    Ok(LogEvent::Line { seq, text }) => {
                        if seq < next {
                            continue;
                        }
                        if seq > next {
                            // We lagged: catch up from the file.
                            for text in send_from_file(next)
                                .await
                                .into_iter()
                                .take((seq - next) as usize)
                            {
                                if tx.send(LogEvent::Line { seq: next, text }).await.is_err() {
                                    return;
                                }
                                next += 1;
                            }
                        }
                        if tx.send(LogEvent::Line { seq, text }).await.is_err() {
                            return;
                        }
                        next = seq + 1;
                    }
                    Ok(done @ LogEvent::Done(_)) => {
                        for text in send_from_file(next).await {
                            if tx.send(LogEvent::Line { seq: next, text }).await.is_err() {
                                return;
                            }
                            next += 1;
                        }
                        let _ = tx.send(done).await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        let status = exec
                            .db
                            .get_run(run_id)
                            .await
                            .ok()
                            .flatten()
                            .map(|r| r.status)
                            .unwrap_or(RunStatus::Failed);
                        let _ = tx.send(LogEvent::Done(status)).await;
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

    async fn dispatch(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<i64>) {
        let slots = Arc::new(Semaphore::new(self.cfg.max_concurrent.max(1)));
        while let Some(run_id) = rx.recv().await {
            let permit = slots
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore closed");
            let exec = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                exec.execute(run_id).await;
            });
        }
    }

    async fn execute(&self, run_id: i64) {
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

        let (cancel, events) = {
            let live = self.live.lock().unwrap();
            match live.get(&run_id) {
                Some(l) => (l.cancel.clone(), l.events.clone()),
                None => {
                    let (events, _) = broadcast::channel(1024);
                    (Arc::new(Notify::new()), events)
                }
            }
        };

        let mut log = match RunLog::create(&self.cfg.log_path(run_id), events).await {
            Ok(l) => l,
            Err(e) => {
                let _ = self
                    .db
                    .mark_finished(run_id, RunStatus::Failed, None, Some(format!("{e:#}")))
                    .await;
                self.finish_live(run_id, RunStatus::Failed);
                return;
            }
        };

        let outcome = self.execute_inner(run_id, &mut log, &cancel).await;
        let (status, code, error) = match outcome {
            Ok(o) => o,
            Err(e) => {
                log.line(format!("{CTM_PREFIX}error: {e:#}")).await;
                (RunStatus::Failed, None, Some(format!("{e:#}")))
            }
        };
        log.line(format!("{CTM_PREFIX}finished: {status}")).await;
        if let Err(e) = self.db.mark_finished(run_id, status, code, error).await {
            tracing::error!(run_id, "mark_finished: {e:#}");
        }
        self.finish_live(run_id, status);
        tracing::info!(run_id, %status, "run finished");
    }

    async fn execute_inner(
        &self,
        run_id: i64,
        log: &mut RunLog,
        cancel: &Notify,
    ) -> Result<(RunStatus, Option<i64>, Option<String>)> {
        let run = self.db.get_run(run_id).await?.context("run disappeared")?;
        let resume = match run.parent_run_id {
            Some(pid) => self.db.get_run(pid).await?.and_then(|p| p.session_id),
            None => None,
        };

        let claude_args = self.claude_args(&run, resume.as_deref());
        let mut cmd = match run.mode {
            Mode::Local => self.local_command(&run, &claude_args, log).await?,
            Mode::Docker => self.docker_command(&run, &claude_args),
        };
        log.line(format!(
            "{CTM_PREFIX}starting {} run in workspace {}{}",
            run.mode,
            run.workspace,
            resume
                .as_deref()
                .map(|s| format!(" (resuming session {s})"))
                .unwrap_or_default()
        ))
        .await;

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

        let (line_tx, mut line_rx) = mpsc::channel::<String>(256);
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        tokio::spawn(pump(stdout, "", line_tx.clone()));
        tokio::spawn(pump(stderr, STDERR_PREFIX, line_tx));

        let mut summary = StreamSummary::default();
        let mut cancelled = false;
        let mut exit = None;
        loop {
            tokio::select! {
                line = line_rx.recv() => match line {
                    Some(line) => {
                        if let Some(session) = summary.observe(&line) {
                            let _ = self.db.set_session(run_id, &session).await;
                        }
                        log.line(line).await;
                    }
                    None => break, // both pipes closed
                },
                status = child.wait(), if exit.is_none() => {
                    exit = Some(status?);
                }
                _ = cancel.notified(), if !cancelled => {
                    cancelled = true;
                    log.line(format!("{CTM_PREFIX}cancelling")).await;
                    if run.mode == Mode::Docker {
                        let _ = Command::new(&self.cfg.docker_bin)
                            .args(["kill", &container_name(run.id)])
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .await;
                    }
                    let _ = child.start_kill();
                }
            }
        }
        let exit = match exit {
            Some(e) => e,
            None => child.wait().await?,
        };

        if summary.result.is_some() || summary.cost.is_some() {
            self.db
                .set_result(run_id, summary.result.as_deref(), summary.cost)
                .await?;
        }

        let code = exit.code().map(i64::from);
        Ok(if cancelled {
            (RunStatus::Cancelled, code, Some("cancelled".into()))
        } else if exit.success() && !summary.is_error {
            (RunStatus::Succeeded, code, None)
        } else if exit.success() {
            (
                RunStatus::Failed,
                code,
                Some("claude reported an error result".into()),
            )
        } else {
            (
                RunStatus::Failed,
                code,
                Some(format!("process exited with {exit}")),
            )
        })
    }

    fn claude_args(&self, run: &Run, resume: Option<&str>) -> Vec<String> {
        let mut args = vec!["-p".to_string()];
        args.extend(split_args(&self.cfg.claude_args));
        if let Some(model) = &run.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(session) = resume {
            args.extend(["--resume".into(), session.to_string()]);
        }
        if let Some(extra) = &run.extra_args {
            args.extend(split_args(extra));
        }
        args
    }

    async fn local_command(
        &self,
        run: &Run,
        claude_args: &[String],
        log: &mut RunLog,
    ) -> Result<Command> {
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
        let mut cmd = Command::new(&self.cfg.claude_bin);
        cmd.args(claude_args).current_dir(&dir);
        cmd.env("CTM_RUN_ID", run.id.to_string());
        Ok(cmd)
    }

    fn docker_command(&self, run: &Run, claude_args: &[String]) -> Command {
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
        for name in self.cfg.forwarded_env() {
            // `-e NAME` copies the value from our environment without
            // putting secrets on the command line.
            if std::env::var_os(&name).is_some() {
                cmd.args(["-e", &name]);
            }
        }
        cmd.args(split_args(&self.cfg.docker_args));
        cmd.arg(run.image.as_deref().unwrap_or(&self.cfg.docker_image));
        cmd.arg("claude").args(claude_args);
        cmd
    }
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

async fn pump(
    reader: impl tokio::io::AsyncRead + Unpin,
    prefix: &'static str,
    tx: mpsc::Sender<String>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(format!("{prefix}{line}")).await.is_err() {
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
        assert_eq!(volume_name("task-3"), "ctm-ws-task-3");
        assert_eq!(volume_name("a/b c"), "ctm-ws-a-b-c");
    }
}
