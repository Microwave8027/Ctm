use std::{fmt, str::FromStr};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::schedule::{Schedule, ScheduleKind};

macro_rules! text_enum {
    ($name:ident { $default:ident => $ds:literal, $($variant:ident => $s:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, Default)]
        #[sqlx(rename_all = "lowercase")]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            #[default]
            $default,
            $($variant),+
        }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $name::$default => $ds,
                    $($name::$variant => $s),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(s: &str) -> Result<Self> {
                match s {
                    "" | $ds => Ok($name::$default),
                    $($s => Ok($name::$variant),)+
                    other => bail!("invalid {} {other:?}", stringify!($name).to_lowercase()),
                }
            }
        }
    };
}

text_enum!(Mode { Local => "local", Docker => "docker", Cloud => "cloud" });
text_enum!(Overlap { Skip => "skip", Queue => "queue", Replace => "replace" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    /// Handed off to a Claude cloud session that keeps running remotely.
    Dispatched,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Dispatched => "dispatched",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, RunStatus::Queued | RunStatus::Running)
    }

    pub fn is_success(self) -> bool {
        matches!(self, RunStatus::Succeeded | RunStatus::Dispatched)
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Agent {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub mode: Mode,
    pub model: Option<String>,
    pub image: Option<String>,
    pub repo: Option<String>,
    pub environment: Option<String>,
    pub cloud_session: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<String>,
    pub extra_args: Option<String>,
    pub persistent_session: bool,
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Job {
    pub id: i64,
    pub name: String,
    pub agent_id: i64,
    pub prompt: String,
    pub schedule_kind: ScheduleKind,
    pub schedule: Option<String>,
    pub timezone: String,
    pub jitter_secs: i64,
    pub overlap: Overlap,
    pub timeout_secs: Option<i64>,
    pub max_retries: i64,
    pub retry_delay_secs: i64,
    pub then_job_id: Option<i64>,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_status: Option<RunStatus>,
    pub created_at: DateTime<Utc>,
}

impl Job {
    pub fn parsed_schedule(&self) -> Result<Schedule> {
        Schedule::parse(self.schedule_kind, self.schedule.as_deref(), &self.timezone)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Run {
    pub id: i64,
    pub agent_id: Option<i64>,
    pub job_id: Option<i64>,
    pub parent_run_id: Option<i64>,
    pub prompt: String,
    pub mode: Mode,
    pub model: Option<String>,
    pub image: Option<String>,
    pub repo: Option<String>,
    pub environment: Option<String>,
    pub cloud_session: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<String>,
    pub extra_args: Option<String>,
    pub resume_session: Option<String>,
    pub workspace: String,
    pub status: RunStatus,
    pub attempt: i64,
    pub timeout_secs: Option<i64>,
    pub not_before: Option<DateTime<Utc>>,
    pub exit_code: Option<i64>,
    pub session_id: Option<String>,
    pub session_url: Option<String>,
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

    /// The spec this run was created from (for re-runs and follow-ups).
    pub fn spec(&self) -> RunSpec {
        RunSpec {
            prompt: self.prompt.clone(),
            mode: self.mode,
            model: self.model.clone(),
            image: self.image.clone(),
            repo: self.repo.clone(),
            environment: self.environment.clone(),
            cloud_session: self.cloud_session.clone(),
            system_prompt: self.system_prompt.clone(),
            permission_mode: self.permission_mode.clone(),
            allowed_tools: self.allowed_tools.clone(),
            extra_args: self.extra_args.clone(),
            timeout_secs: self.timeout_secs,
        }
    }
}

/// Everything needed to start one Claude run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunSpec {
    pub prompt: String,
    pub mode: Mode,
    pub model: Option<String>,
    pub image: Option<String>,
    pub repo: Option<String>,
    /// cloud: self-hosted environment id for new sessions.
    pub environment: Option<String>,
    /// cloud: existing session id or URL to send the prompt to.
    pub cloud_session: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<String>,
    pub extra_args: Option<String>,
    pub timeout_secs: Option<i64>,
}

/// Turns empty strings (as submitted by HTML forms) into `None`.
fn clean(v: &mut Option<String>) {
    match v {
        Some(s) if s.trim().is_empty() => *v = None,
        Some(s) => *s = s.trim().to_string(),
        None => {}
    }
}

impl RunSpec {
    pub fn normalized(mut self) -> Self {
        for f in [
            &mut self.model,
            &mut self.image,
            &mut self.repo,
            &mut self.environment,
            &mut self.cloud_session,
            &mut self.system_prompt,
            &mut self.permission_mode,
            &mut self.allowed_tools,
            &mut self.extra_args,
        ] {
            clean(f);
        }
        self.timeout_secs = self.timeout_secs.filter(|t| *t > 0);
        self
    }

    pub fn validate(&self) -> Result<()> {
        if self.prompt.trim().is_empty() {
            bail!("prompt is required");
        }
        if self.mode == Mode::Cloud {
            if self.permission_mode.as_deref() == Some("bypassPermissions") {
                bail!("cloud sessions cannot bypass permissions");
            }
            if self.environment.is_some() && self.cloud_session.is_some() {
                bail!(
                    "set either an environment (new session) or an existing cloud session, not both"
                );
            }
        }
        Ok(())
    }
}

/// Where a run came from; decides its workspace and resumed session.
#[derive(Debug, Default, Clone)]
pub struct RunOrigin<'a> {
    pub agent: Option<&'a Agent>,
    pub job_id: Option<i64>,
    pub parent: Option<&'a Run>,
    pub attempt: i64,
    pub not_before: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NewAgent {
    pub name: String,
    pub description: Option<String>,
    pub mode: Mode,
    pub model: Option<String>,
    pub image: Option<String>,
    pub repo: Option<String>,
    pub environment: Option<String>,
    pub cloud_session: Option<String>,
    pub system_prompt: Option<String>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<String>,
    pub extra_args: Option<String>,
    pub persistent_session: bool,
}

impl NewAgent {
    fn normalized(mut self) -> Result<Self> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            bail!("agent name is required");
        }
        for f in [
            &mut self.description,
            &mut self.model,
            &mut self.image,
            &mut self.repo,
            &mut self.environment,
            &mut self.cloud_session,
            &mut self.system_prompt,
            &mut self.permission_mode,
            &mut self.allowed_tools,
            &mut self.extra_args,
        ] {
            clean(f);
        }
        // Reuse RunSpec's mode-specific checks.
        self.spec("x".into(), None).validate()?;
        Ok(self)
    }

    fn spec(&self, prompt: String, timeout_secs: Option<i64>) -> RunSpec {
        RunSpec {
            prompt,
            mode: self.mode,
            model: self.model.clone(),
            image: self.image.clone(),
            repo: self.repo.clone(),
            environment: self.environment.clone(),
            cloud_session: self.cloud_session.clone(),
            system_prompt: self.system_prompt.clone(),
            permission_mode: self.permission_mode.clone(),
            allowed_tools: self.allowed_tools.clone(),
            extra_args: self.extra_args.clone(),
            timeout_secs,
        }
    }
}

impl Agent {
    pub fn spec(&self, prompt: String, timeout_secs: Option<i64>) -> RunSpec {
        self.as_new().spec(prompt, timeout_secs)
    }

    pub fn as_new(&self) -> NewAgent {
        NewAgent {
            name: self.name.clone(),
            description: self.description.clone(),
            mode: self.mode,
            model: self.model.clone(),
            image: self.image.clone(),
            repo: self.repo.clone(),
            environment: self.environment.clone(),
            cloud_session: self.cloud_session.clone(),
            system_prompt: self.system_prompt.clone(),
            permission_mode: self.permission_mode.clone(),
            allowed_tools: self.allowed_tools.clone(),
            extra_args: self.extra_args.clone(),
            persistent_session: self.persistent_session,
        }
    }

    pub fn workspace(&self) -> String {
        format!("agent-{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NewJob {
    pub name: String,
    pub agent_id: i64,
    pub prompt: String,
    pub schedule_kind: ScheduleKind,
    pub schedule: Option<String>,
    pub timezone: String,
    pub jitter_secs: i64,
    pub overlap: Overlap,
    pub timeout_secs: Option<i64>,
    pub max_retries: i64,
    pub retry_delay_secs: i64,
    pub then_job_id: Option<i64>,
    pub enabled: bool,
}

impl Default for NewJob {
    fn default() -> Self {
        Self {
            name: String::new(),
            agent_id: 0,
            prompt: String::new(),
            schedule_kind: ScheduleKind::Manual,
            schedule: None,
            timezone: "UTC".into(),
            jitter_secs: 0,
            overlap: Overlap::Skip,
            timeout_secs: None,
            max_retries: 0,
            retry_delay_secs: 60,
            then_job_id: None,
            enabled: true,
        }
    }
}

impl NewJob {
    /// Validates the job and computes its first fire time.
    fn prepare(mut self, now: DateTime<Utc>) -> Result<(Self, Option<DateTime<Utc>>)> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            bail!("job name is required");
        }
        if self.prompt.trim().is_empty() {
            bail!("prompt is required");
        }
        clean(&mut self.schedule);
        self.timezone = self.timezone.trim().to_string();
        if self.timezone.is_empty() {
            self.timezone = "UTC".into();
        }
        self.jitter_secs = self.jitter_secs.clamp(0, 3600);
        self.max_retries = self.max_retries.clamp(0, 10);
        self.retry_delay_secs = self.retry_delay_secs.clamp(1, 86_400);
        self.timeout_secs = self.timeout_secs.filter(|t| *t > 0);
        let schedule =
            Schedule::parse(self.schedule_kind, self.schedule.as_deref(), &self.timezone)?;
        let next = if self.enabled {
            schedule.next_after(now)?
        } else {
            None
        };
        if self.schedule_kind == ScheduleKind::Once && self.enabled && next.is_none() {
            bail!("that time is already in the past");
        }
        Ok((self, next))
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct RunFilter {
    pub job_id: Option<i64>,
    pub agent_id: Option<i64>,
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
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    // ---- agents ------------------------------------------------------------

    pub async fn create_agent(&self, a: NewAgent) -> Result<Agent> {
        let a = a.normalized()?;
        let now = Utc::now();
        let id = sqlx::query(
            "INSERT INTO agents (name, description, mode, model, image, repo, environment, cloud_session,
                                 system_prompt, permission_mode, allowed_tools, extra_args, persistent_session,
                                 created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&a.name)
        .bind(&a.description)
        .bind(a.mode)
        .bind(&a.model)
        .bind(&a.image)
        .bind(&a.repo)
        .bind(&a.environment)
        .bind(&a.cloud_session)
        .bind(&a.system_prompt)
        .bind(&a.permission_mode)
        .bind(&a.allowed_tools)
        .bind(&a.extra_args)
        .bind(a.persistent_session)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(unique_violation("an agent with that name already exists"))?
        .last_insert_rowid();
        self.get_agent(id).await?.context("agent vanished")
    }

    pub async fn update_agent(&self, id: i64, a: NewAgent) -> Result<Option<Agent>> {
        let a = a.normalized()?;
        let r = sqlx::query(
            "UPDATE agents SET name = ?, description = ?, mode = ?, model = ?, image = ?, repo = ?,
                    environment = ?, cloud_session = ?, system_prompt = ?, permission_mode = ?,
                    allowed_tools = ?, extra_args = ?, persistent_session = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&a.name)
        .bind(&a.description)
        .bind(a.mode)
        .bind(&a.model)
        .bind(&a.image)
        .bind(&a.repo)
        .bind(&a.environment)
        .bind(&a.cloud_session)
        .bind(&a.system_prompt)
        .bind(&a.permission_mode)
        .bind(&a.allowed_tools)
        .bind(&a.extra_args)
        .bind(a.persistent_session)
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(unique_violation("an agent with that name already exists"))?;
        if r.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_agent(id).await
    }

    pub async fn get_agent(&self, id: i64) -> Result<Option<Agent>> {
        Ok(sqlx::query_as("SELECT * FROM agents WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Looks an agent up by numeric id or by name.
    pub async fn find_agent(&self, name_or_id: &str) -> Result<Option<Agent>> {
        if let Ok(id) = name_or_id.parse::<i64>() {
            return self.get_agent(id).await;
        }
        Ok(sqlx::query_as("SELECT * FROM agents WHERE name = ?")
            .bind(name_or_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn list_agents(&self) -> Result<Vec<Agent>> {
        Ok(
            sqlx::query_as("SELECT * FROM agents ORDER BY name COLLATE NOCASE")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn delete_agent(&self, id: i64) -> Result<bool> {
        let r = sqlx::query("DELETE FROM agents WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_agent_session(&self, id: i64, session_id: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE agents SET session_id = ? WHERE id = ?")
            .bind(session_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- jobs --------------------------------------------------------------

    pub async fn create_job(&self, j: NewJob) -> Result<Job> {
        let (j, next) = j.prepare(Utc::now())?;
        self.get_agent(j.agent_id)
            .await?
            .context("agent not found")?;
        let id = sqlx::query(
            "INSERT INTO jobs (name, agent_id, prompt, schedule_kind, schedule, timezone, jitter_secs, overlap,
                               timeout_secs, max_retries, retry_delay_secs, then_job_id, enabled, next_run_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&j.name)
        .bind(j.agent_id)
        .bind(&j.prompt)
        .bind(j.schedule_kind)
        .bind(&j.schedule)
        .bind(&j.timezone)
        .bind(j.jitter_secs)
        .bind(j.overlap)
        .bind(j.timeout_secs)
        .bind(j.max_retries)
        .bind(j.retry_delay_secs)
        .bind(j.then_job_id)
        .bind(j.enabled)
        .bind(next)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?
        .last_insert_rowid();
        self.get_job(id).await?.context("job vanished")
    }

    pub async fn update_job(&self, id: i64, j: NewJob) -> Result<Option<Job>> {
        let (j, next) = j.prepare(Utc::now())?;
        if j.then_job_id == Some(id) {
            bail!("a job cannot chain to itself");
        }
        self.get_agent(j.agent_id)
            .await?
            .context("agent not found")?;
        let r = sqlx::query(
            "UPDATE jobs SET name = ?, agent_id = ?, prompt = ?, schedule_kind = ?, schedule = ?, timezone = ?,
                    jitter_secs = ?, overlap = ?, timeout_secs = ?, max_retries = ?, retry_delay_secs = ?,
                    then_job_id = ?, enabled = ?, next_run_at = ?
             WHERE id = ?",
        )
        .bind(&j.name)
        .bind(j.agent_id)
        .bind(&j.prompt)
        .bind(j.schedule_kind)
        .bind(&j.schedule)
        .bind(&j.timezone)
        .bind(j.jitter_secs)
        .bind(j.overlap)
        .bind(j.timeout_secs)
        .bind(j.max_retries)
        .bind(j.retry_delay_secs)
        .bind(j.then_job_id)
        .bind(j.enabled)
        .bind(next)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_job(id).await
    }

    pub async fn get_job(&self, id: i64) -> Result<Option<Job>> {
        Ok(sqlx::query_as("SELECT * FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn list_jobs(&self, agent_id: Option<i64>) -> Result<Vec<Job>> {
        Ok(
            sqlx::query_as("SELECT * FROM jobs WHERE (?1 IS NULL OR agent_id = ?1) ORDER BY name COLLATE NOCASE, id")
                .bind(agent_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn delete_job(&self, id: i64) -> Result<bool> {
        let r = sqlx::query("DELETE FROM jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    pub async fn set_job_enabled(&self, id: i64, enabled: bool) -> Result<Option<Job>> {
        let Some(job) = self.get_job(id).await? else {
            return Ok(None);
        };
        // Re-arm from "now" so enabling a long-disabled job doesn't fire a backlog.
        let next = if enabled {
            job.parsed_schedule()?.next_after(Utc::now())?
        } else {
            None
        };
        sqlx::query("UPDATE jobs SET enabled = ?, next_run_at = ? WHERE id = ?")
            .bind(enabled)
            .bind(next)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.get_job(id).await
    }

    pub async fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<Job>> {
        Ok(sqlx::query_as(
            "SELECT * FROM jobs WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ? ORDER BY next_run_at",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Records a fire. One-shot jobs disable themselves once they have no next time.
    pub async fn set_job_fired(
        &self,
        id: i64,
        fired_at: Option<DateTime<Utc>>,
        next: Option<DateTime<Utc>>,
        disable: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET last_run_at = COALESCE(?, last_run_at), next_run_at = ?,
                    enabled = CASE WHEN ? THEN 0 ELSE enabled END
             WHERE id = ?",
        )
        .bind(fired_at)
        .bind(next)
        .bind(disable)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_job_last_status(&self, id: i64, status: RunStatus) -> Result<()> {
        sqlx::query("UPDATE jobs SET last_status = ? WHERE id = ?")
            .bind(status)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn active_runs_for_job(&self, job_id: i64) -> Result<Vec<Run>> {
        Ok(sqlx::query_as(
            "SELECT * FROM runs WHERE job_id = ? AND status IN ('queued', 'running')",
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn last_finished_run_for_job(&self, job_id: i64) -> Result<Option<Run>> {
        Ok(sqlx::query_as(
            "SELECT * FROM runs WHERE job_id = ? AND status NOT IN ('queued', 'running') ORDER BY id DESC LIMIT 1",
        )
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    // ---- runs --------------------------------------------------------------

    /// Inserts a queued run. Follow-ups continue in their parent's workspace
    /// and session; agent runs share the agent's workspace (and session, if
    /// the agent is persistent); ad-hoc prompts get a private workspace.
    pub async fn create_run(&self, spec: RunSpec, origin: RunOrigin<'_>) -> Result<Run> {
        let spec = spec.normalized();
        spec.validate()?;
        let workspace = match (origin.parent, origin.agent) {
            (Some(p), _) => p.workspace.clone(),
            (None, Some(a)) => a.workspace(),
            (None, None) => String::new(),
        };
        let resume = match (origin.parent, origin.agent) {
            (Some(p), _) => p.session_id.clone(),
            (None, Some(a)) if a.persistent_session => a.session_id.clone(),
            _ => None,
        };
        let agent_id = origin
            .agent
            .map(|a| a.id)
            .or(origin.parent.and_then(|p| p.agent_id));
        let id = sqlx::query(
            "INSERT INTO runs (agent_id, job_id, parent_run_id, prompt, mode, model, image, repo, environment,
                               cloud_session, system_prompt, permission_mode, allowed_tools, extra_args,
                               resume_session, workspace, status, attempt, timeout_secs, not_before, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?, ?)",
        )
        .bind(agent_id)
        .bind(origin.job_id)
        .bind(origin.parent.map(|p| p.id))
        .bind(&spec.prompt)
        .bind(spec.mode)
        .bind(&spec.model)
        .bind(&spec.image)
        .bind(&spec.repo)
        .bind(&spec.environment)
        .bind(&spec.cloud_session)
        .bind(&spec.system_prompt)
        .bind(&spec.permission_mode)
        .bind(&spec.allowed_tools)
        .bind(&spec.extra_args)
        .bind(&resume)
        .bind(&workspace)
        .bind(origin.attempt.max(1))
        .bind(spec.timeout_secs)
        .bind(origin.not_before)
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

    pub async fn list_runs(&self, limit: i64, filter: RunFilter) -> Result<Vec<Run>> {
        Ok(sqlx::query_as(
            "SELECT * FROM runs WHERE (?1 IS NULL OR job_id = ?1) AND (?2 IS NULL OR agent_id = ?2)
             ORDER BY id DESC LIMIT ?3",
        )
        .bind(filter.job_id)
        .bind(filter.agent_id)
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

    pub async fn set_session(&self, id: i64, session_id: &str, url: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE runs SET session_id = ?, session_url = COALESCE(?, session_url) WHERE id = ?",
        )
        .bind(session_id)
        .bind(url)
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

    // ---- settings ----------------------------------------------------------

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_setting(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn unique_violation(msg: &'static str) -> impl Fn(sqlx::Error) -> anyhow::Error {
    move |e| match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => anyhow::anyhow!(msg),
        _ => e.into(),
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

    async fn agent(db: &Db, persistent: bool) -> Agent {
        db.create_agent(NewAgent {
            name: format!("agent-{persistent}"),
            persistent_session: persistent,
            ..Default::default()
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn run_lifecycle_and_workspaces() {
        let db = Db::memory().await.unwrap();
        let adhoc = db
            .create_run(spec("hi"), RunOrigin::default())
            .await
            .unwrap();
        assert_eq!(adhoc.workspace, format!("run-{}", adhoc.id));
        assert_eq!(adhoc.status, RunStatus::Queued);
        db.set_session(adhoc.id, "s1", None).await.unwrap();
        let adhoc = db.get_run(adhoc.id).await.unwrap().unwrap();

        let origin = RunOrigin {
            parent: Some(&adhoc),
            ..Default::default()
        };
        let follow = db.create_run(spec("more"), origin).await.unwrap();
        assert_eq!(follow.workspace, adhoc.workspace);
        assert_eq!(follow.resume_session.as_deref(), Some("s1"));

        assert!(db.mark_running(adhoc.id).await.unwrap());
        assert!(!db.mark_running(adhoc.id).await.unwrap());
        db.mark_finished(adhoc.id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        // Finished runs are terminal.
        db.mark_finished(adhoc.id, RunStatus::Failed, Some(1), None)
            .await
            .unwrap();
        assert_eq!(
            db.get_run(adhoc.id).await.unwrap().unwrap().status,
            RunStatus::Succeeded
        );

        let cloud = RunSpec {
            mode: Mode::Cloud,
            permission_mode: Some("bypassPermissions".into()),
            ..spec("x")
        };
        assert!(db.create_run(cloud, RunOrigin::default()).await.is_err());
    }

    #[tokio::test]
    async fn persistent_agents_resume_their_session() {
        let db = Db::memory().await.unwrap();
        let a = agent(&db, true).await;
        db.set_agent_session(a.id, Some("sess-a")).await.unwrap();
        let a = db.get_agent(a.id).await.unwrap().unwrap();
        let origin = RunOrigin {
            agent: Some(&a),
            ..Default::default()
        };
        let r = db
            .create_run(a.spec("p".into(), None), origin)
            .await
            .unwrap();
        assert_eq!(r.workspace, format!("agent-{}", a.id));
        assert_eq!(r.resume_session.as_deref(), Some("sess-a"));

        let b = agent(&db, false).await;
        db.set_agent_session(b.id, Some("sess-b")).await.unwrap();
        let b = db.get_agent(b.id).await.unwrap().unwrap();
        let origin = RunOrigin {
            agent: Some(&b),
            ..Default::default()
        };
        let r = db
            .create_run(b.spec("p".into(), None), origin)
            .await
            .unwrap();
        assert_eq!(r.resume_session, None);

        let dup = NewAgent {
            name: "agent-true".into(),
            ..Default::default()
        };
        assert!(db.create_agent(dup).await.is_err());
        assert_eq!(db.find_agent("agent-true").await.unwrap().unwrap().id, a.id);
    }

    #[tokio::test]
    async fn jobs_schedule() {
        let db = Db::memory().await.unwrap();
        let a = agent(&db, false).await;
        let bad = db
            .create_job(NewJob {
                name: "x".into(),
                agent_id: a.id,
                prompt: "p".into(),
                schedule_kind: ScheduleKind::Cron,
                schedule: Some("not a cron".into()),
                ..Default::default()
            })
            .await;
        assert!(bad.is_err());

        let j = db
            .create_job(NewJob {
                name: "nightly".into(),
                agent_id: a.id,
                prompt: "p".into(),
                schedule_kind: ScheduleKind::Cron,
                schedule: Some("0 3 * * *".into()),
                timezone: "Europe/Berlin".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(j.next_run_at.unwrap() > Utc::now());
        assert!(db.due_jobs(Utc::now()).await.unwrap().is_empty());
        let later = Utc::now() + chrono::Duration::days(2);
        assert_eq!(db.due_jobs(later).await.unwrap().len(), 1);

        let off = db.set_job_enabled(j.id, false).await.unwrap().unwrap();
        assert!(!off.enabled && off.next_run_at.is_none());
        assert!(db.due_jobs(later).await.unwrap().is_empty());

        // Deleting the agent removes its jobs.
        db.delete_agent(a.id).await.unwrap();
        assert!(db.get_job(j.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let db = Db::memory().await.unwrap();
        assert_eq!(db.get_setting("k").await.unwrap(), None);
        db.set_setting("k", "v1").await.unwrap();
        db.set_setting("k", "v2").await.unwrap();
        assert_eq!(db.get_setting("k").await.unwrap().as_deref(), Some("v2"));
        db.delete_setting("k").await.unwrap();
        assert_eq!(db.get_setting("k").await.unwrap(), None);
    }
}
