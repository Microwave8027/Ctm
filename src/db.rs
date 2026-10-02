use std::{fmt, str::FromStr};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, Default)]
#[sqlx(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Local,
    Docker,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Local => "local",
            Mode::Docker => "docker",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "local" => Ok(Mode::Local),
            "docker" => Ok(Mode::Docker),
            other => bail!("unknown mode {other:?} (expected local or docker)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, RunStatus::Queued | RunStatus::Running)
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Task {
    pub id: i64,
    pub name: String,
    pub prompt: String,
    pub cron: Option<String>,
    pub mode: Mode,
    pub image: Option<String>,
    pub repo: Option<String>,
    pub model: Option<String>,
    pub extra_args: Option<String>,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Run {
    pub id: i64,
    pub task_id: Option<i64>,
    pub parent_run_id: Option<i64>,
    pub prompt: String,
    pub mode: Mode,
    pub image: Option<String>,
    pub repo: Option<String>,
    pub model: Option<String>,
    pub extra_args: Option<String>,
    pub workspace: String,
    pub status: RunStatus,
    pub exit_code: Option<i64>,
    pub session_id: Option<String>,
    pub result: Option<String>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Run {
    pub fn duration_secs(&self) -> Option<i64> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Utc::now);
        Some((end - start).num_seconds().max(0))
    }
}

/// Parameters shared by tasks and ad-hoc runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSpec {
    pub prompt: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub extra_args: Option<String>,
}

impl RunSpec {
    /// Turns empty strings (as submitted by HTML forms) into `None`.
    pub fn normalized(mut self) -> Self {
        fn clean(v: &mut Option<String>) {
            if v.as_deref().is_some_and(|s| s.trim().is_empty()) {
                *v = None;
            } else if let Some(s) = v {
                *s = s.trim().to_string();
            }
        }
        clean(&mut self.image);
        clean(&mut self.repo);
        clean(&mut self.model);
        clean(&mut self.extra_args);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTask {
    pub name: String,
    #[serde(default)]
    pub cron: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub spec: RunSpec,
}

fn default_true() -> bool {
    true
}

/// Validates a cron expression and returns its next fire time after `after`.
pub fn next_fire(cron: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let parsed = croner::Cron::from_str(cron).with_context(|| format!("invalid cron {cron:?}"))?;
    parsed
        .find_next_occurrence(&after, false)
        .with_context(|| format!("cron {cron:?} never fires"))
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    pub async fn open(path: &std::path::Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    #[cfg(test)]
    pub async fn memory() -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    // ---- tasks -----------------------------------------------------------

    pub async fn create_task(&self, t: NewTask) -> Result<Task> {
        let spec = t.spec.normalized();
        if t.name.trim().is_empty() {
            bail!("task name is required");
        }
        if spec.prompt.trim().is_empty() {
            bail!("prompt is required");
        }
        let cron = t
            .cron
            .filter(|c| !c.trim().is_empty())
            .map(|c| c.trim().to_string());
        let now = Utc::now();
        let next = cron.as_deref().map(|c| next_fire(c, now)).transpose()?;
        let id = sqlx::query(
            "INSERT INTO tasks (name, prompt, cron, mode, image, repo, model, extra_args, enabled, next_run_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(t.name.trim())
        .bind(&spec.prompt)
        .bind(&cron)
        .bind(spec.mode)
        .bind(&spec.image)
        .bind(&spec.repo)
        .bind(&spec.model)
        .bind(&spec.extra_args)
        .bind(t.enabled)
        .bind(next)
        .bind(now)
        .execute(&self.pool)
        .await?
        .last_insert_rowid();
        self.get_task(id).await?.context("task vanished")
    }

    pub async fn get_task(&self, id: i64) -> Result<Option<Task>> {
        Ok(sqlx::query_as("SELECT * FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn list_tasks(&self) -> Result<Vec<Task>> {
        Ok(
            sqlx::query_as("SELECT * FROM tasks ORDER BY name COLLATE NOCASE, id")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn delete_task(&self, id: i64) -> Result<bool> {
        let r = sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_task_enabled(&self, id: i64, enabled: bool) -> Result<Option<Task>> {
        let Some(task) = self.get_task(id).await? else {
            return Ok(None);
        };
        // Re-arm from "now" so enabling a long-disabled task doesn't fire a backlog.
        let next = match (&task.cron, enabled) {
            (Some(c), true) => Some(next_fire(c, Utc::now())?),
            _ => None,
        };
        sqlx::query("UPDATE tasks SET enabled = ?, next_run_at = ? WHERE id = ?")
            .bind(enabled)
            .bind(next)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.get_task(id).await
    }

    pub async fn due_tasks(&self, now: DateTime<Utc>) -> Result<Vec<Task>> {
        Ok(sqlx::query_as(
            "SELECT * FROM tasks WHERE enabled = 1 AND cron IS NOT NULL
               AND next_run_at IS NOT NULL AND next_run_at <= ?",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn set_task_schedule(
        &self,
        id: i64,
        last: Option<DateTime<Utc>>,
        next: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE tasks SET last_run_at = COALESCE(?, last_run_at), next_run_at = ? WHERE id = ?",
        )
        .bind(last)
        .bind(next)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn task_has_active_run(&self, task_id: i64) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs WHERE task_id = ? AND status IN ('queued', 'running')",
        )
        .bind(task_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    // ---- runs ------------------------------------------------------------

    /// Inserts a queued run. The workspace is inherited from the parent run
    /// (follow-ups continue in the same checkout), shared per task, or
    /// private to the run for ad-hoc prompts.
    pub async fn create_run(
        &self,
        spec: RunSpec,
        task_id: Option<i64>,
        parent: Option<&Run>,
    ) -> Result<Run> {
        let spec = spec.normalized();
        if spec.prompt.trim().is_empty() {
            bail!("prompt is required");
        }
        let workspace = match (parent, task_id) {
            (Some(p), _) => p.workspace.clone(),
            (None, Some(t)) => format!("task-{t}"),
            (None, None) => String::new(),
        };
        let id = sqlx::query(
            "INSERT INTO runs (task_id, parent_run_id, prompt, mode, image, repo, model, extra_args, workspace, status, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?)",
        )
        .bind(task_id.or(parent.and_then(|p| p.task_id)))
        .bind(parent.map(|p| p.id))
        .bind(&spec.prompt)
        .bind(spec.mode)
        .bind(&spec.image)
        .bind(&spec.repo)
        .bind(&spec.model)
        .bind(&spec.extra_args)
        .bind(&workspace)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?
        .last_insert_rowid();
        if workspace.is_empty() {
            sqlx::query("UPDATE runs SET workspace = 'run-' || id WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        self.get_run(id).await?.context("run vanished")
    }

    pub async fn get_run(&self, id: i64) -> Result<Option<Run>> {
        Ok(sqlx::query_as("SELECT * FROM runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn list_runs(&self, limit: i64, task_id: Option<i64>) -> Result<Vec<Run>> {
        Ok(sqlx::query_as(
            "SELECT * FROM runs WHERE (?1 IS NULL OR task_id = ?1) ORDER BY id DESC LIMIT ?2",
        )
        .bind(task_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn runs_with_status(&self, status: RunStatus) -> Result<Vec<Run>> {
        Ok(
            sqlx::query_as("SELECT * FROM runs WHERE status = ? ORDER BY id")
                .bind(status)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn status_counts(&self) -> Result<Vec<(RunStatus, i64)>> {
        Ok(
            sqlx::query_as("SELECT status, COUNT(*) FROM runs GROUP BY status")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Moves a queued run to running. Returns false if it was cancelled
    /// while waiting in the queue.
    pub async fn mark_running(&self, id: i64) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE runs SET status = 'running', started_at = ? WHERE id = ? AND status = 'queued'",
        )
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn mark_finished(
        &self,
        id: i64,
        status: RunStatus,
        exit_code: Option<i64>,
        error: Option<String>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE runs SET status = ?, exit_code = ?, error = ?, finished_at = ?
             WHERE id = ? AND status IN ('queued', 'running')",
        )
        .bind(status)
        .bind(exit_code)
        .bind(error)
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_session(&self, id: i64, session_id: &str) -> Result<()> {
        sqlx::query("UPDATE runs SET session_id = ? WHERE id = ?")
            .bind(session_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_result(&self, id: i64, result: Option<&str>, cost: Option<f64>) -> Result<()> {
        sqlx::query("UPDATE runs SET result = ?, cost_usd = ? WHERE id = ?")
            .bind(result)
            .bind(cost)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Runs that were mid-flight when the server stopped can't be resumed;
    /// mark them failed so they don't look alive forever.
    pub async fn fail_interrupted(&self) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE runs SET status = 'failed', error = 'ctm restarted while the run was in progress', finished_at = ?
             WHERE status = 'running'",
        )
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

impl Task {
    pub fn spec(&self) -> RunSpec {
        RunSpec {
            prompt: self.prompt.clone(),
            mode: self.mode,
            image: self.image.clone(),
            repo: self.repo.clone(),
            model: self.model.clone(),
            extra_args: self.extra_args.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(prompt: &str) -> RunSpec {
        RunSpec {
            prompt: prompt.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn run_lifecycle_and_workspaces() {
        let db = Db::memory().await.unwrap();
        let adhoc = db.create_run(spec("hi"), None, None).await.unwrap();
        assert_eq!(adhoc.workspace, format!("run-{}", adhoc.id));
        assert_eq!(adhoc.status, RunStatus::Queued);

        let follow = db
            .create_run(spec("more"), None, Some(&adhoc))
            .await
            .unwrap();
        assert_eq!(follow.workspace, adhoc.workspace);
        assert_eq!(follow.parent_run_id, Some(adhoc.id));

        assert!(db.mark_running(adhoc.id).await.unwrap());
        assert!(!db.mark_running(adhoc.id).await.unwrap());
        db.mark_finished(adhoc.id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let done = db.get_run(adhoc.id).await.unwrap().unwrap();
        assert_eq!(done.status, RunStatus::Succeeded);
        // Finished runs are terminal.
        db.mark_finished(adhoc.id, RunStatus::Failed, Some(1), None)
            .await
            .unwrap();
        assert_eq!(
            db.get_run(adhoc.id).await.unwrap().unwrap().status,
            RunStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn tasks_schedule() {
        let db = Db::memory().await.unwrap();
        let bad = db
            .create_task(NewTask {
                name: "x".into(),
                cron: Some("not a cron".into()),
                enabled: true,
                spec: spec("p"),
            })
            .await;
        assert!(bad.is_err());

        let t = db
            .create_task(NewTask {
                name: "nightly".into(),
                cron: Some("0 3 * * *".into()),
                enabled: true,
                spec: spec("p"),
            })
            .await
            .unwrap();
        assert!(t.next_run_at.unwrap() > Utc::now());
        assert!(db.due_tasks(Utc::now()).await.unwrap().is_empty());
        let later = Utc::now() + chrono::Duration::days(2);
        assert_eq!(db.due_tasks(later).await.unwrap().len(), 1);

        let r = db.create_run(t.spec(), Some(t.id), None).await.unwrap();
        assert_eq!(r.workspace, format!("task-{}", t.id));
        assert!(db.task_has_active_run(t.id).await.unwrap());

        let off = db.set_task_enabled(t.id, false).await.unwrap().unwrap();
        assert!(!off.enabled && off.next_run_at.is_none());
        assert!(db.due_tasks(later).await.unwrap().is_empty());
    }
}
