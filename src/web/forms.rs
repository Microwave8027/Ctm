//! HTML form payloads. Browsers send every field as a (possibly empty)
//! string, so these parse into the typed structs the database expects.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::db::{NewAgent, NewJob, RunSpec};

fn int(v: &Option<String>, what: &str) -> Result<Option<i64>> {
    match v.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => s
            .parse()
            .map(Some)
            .with_context(|| format!("{what} must be a whole number")),
    }
}

/// Fields shared by ad-hoc runs and agents.
#[derive(Debug, Default, Deserialize)]
pub struct ClaudeForm {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub cloud_session: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub allowed_tools: Option<String>,
    #[serde(default)]
    pub extra_args: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RunForm {
    pub prompt: String,
    #[serde(default)]
    pub timeout_secs: Option<String>,
    #[serde(flatten)]
    pub claude: ClaudeForm,
}

impl RunForm {
    pub fn into_spec(self) -> Result<RunSpec> {
        let c = self.claude;
        Ok(RunSpec {
            prompt: self.prompt,
            mode: c.mode.as_deref().unwrap_or("").parse()?,
            model: c.model,
            image: c.image,
            repo: c.repo,
            environment: c.environment,
            cloud_session: c.cloud_session,
            system_prompt: c.system_prompt,
            permission_mode: c.permission_mode,
            allowed_tools: c.allowed_tools,
            extra_args: c.extra_args,
            timeout_secs: int(&self.timeout_secs, "timeout")?,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentForm {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub persistent_session: Option<String>,
    #[serde(flatten)]
    pub claude: ClaudeForm,
}

impl AgentForm {
    pub fn into_new(self) -> Result<NewAgent> {
        let c = self.claude;
        Ok(NewAgent {
            name: self.name,
            description: self.description,
            mode: c.mode.as_deref().unwrap_or("").parse()?,
            model: c.model,
            image: c.image,
            repo: c.repo,
            environment: c.environment,
            cloud_session: c.cloud_session,
            system_prompt: c.system_prompt,
            permission_mode: c.permission_mode,
            allowed_tools: c.allowed_tools,
            extra_args: c.extra_args,
            persistent_session: self.persistent_session.is_some(),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct JobForm {
    pub name: String,
    pub agent_id: String,
    pub prompt: String,
    #[serde(default)]
    pub schedule_kind: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub jitter_secs: Option<String>,
    #[serde(default)]
    pub overlap: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<String>,
    #[serde(default)]
    pub max_retries: Option<String>,
    #[serde(default)]
    pub retry_delay_secs: Option<String>,
    #[serde(default)]
    pub then_job_id: Option<String>,
    #[serde(default)]
    pub enabled: Option<String>,
}

impl JobForm {
    pub fn into_new(self) -> Result<NewJob> {
        Ok(NewJob {
            agent_id: self.agent_id.trim().parse().context("choose an agent")?,
            schedule_kind: self.schedule_kind.as_deref().unwrap_or("").parse()?,
            timezone: self.timezone.clone().unwrap_or_default(),
            jitter_secs: int(&self.jitter_secs, "jitter")?.unwrap_or(0),
            overlap: self.overlap.as_deref().unwrap_or("").parse()?,
            timeout_secs: int(&self.timeout_secs, "timeout")?,
            max_retries: int(&self.max_retries, "retries")?.unwrap_or(0),
            retry_delay_secs: int(&self.retry_delay_secs, "retry delay")?.unwrap_or(60),
            then_job_id: int(&self.then_job_id, "chained job")?,
            enabled: self.enabled.is_some(),
            name: self.name,
            prompt: self.prompt,
            schedule: self.schedule,
        })
    }
}

/// Just the schedule inputs, for the live preview.
#[derive(Debug, Default, Deserialize)]
pub struct ScheduleForm {
    #[serde(default)]
    pub schedule_kind: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PromptForm {
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
pub struct ValueForm {
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct CodeForm {
    pub code: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_parse_from_urlencoded() {
        let f: AgentForm = serde_urlencoded::from_str(
            "name=a&description=&mode=cloud&model=&repo=r&persistent_session=on&cloud_session=&environment=ccpool_x",
        )
        .unwrap();
        let a = f.into_new().unwrap();
        assert!(a.persistent_session);
        assert_eq!(a.mode, crate::db::Mode::Cloud);
        assert_eq!(a.environment.as_deref(), Some("ccpool_x"));

        let j: JobForm = serde_urlencoded::from_str(
            "name=n&agent_id=3&prompt=p&schedule_kind=interval&schedule=15m&timezone=UTC&jitter_secs=&overlap=queue&timeout_secs=600&max_retries=2&retry_delay_secs=30&then_job_id=&enabled=on",
        )
        .unwrap();
        let j = j.into_new().unwrap();
        assert_eq!(
            (j.agent_id, j.timeout_secs, j.max_retries, j.then_job_id),
            (3, Some(600), 2, None)
        );
        assert_eq!(j.overlap, crate::db::Overlap::Queue);

        let bad: JobForm =
            serde_urlencoded::from_str("name=n&agent_id=1&prompt=p&max_retries=lots").unwrap();
        assert!(bad.into_new().is_err());
    }
}
