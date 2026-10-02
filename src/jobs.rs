//! Agent scheduling: the scheduler loop, firing jobs (overlap policies,
//! prompt templates), and post-run hooks (retries with backoff, chaining).

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::{
    db::{Agent, Db, Job, Overlap, Run, RunOrigin, RunStatus},
    executor::Executor,
    schedule::{ScheduleKind, jitter},
};

/// Why a job is being fired.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// Its schedule came due.
    Schedule,
    /// Someone pressed "run now" (dashboard, API or CLI).
    Manual,
    /// The job it is chained to finished successfully.
    Chain(Box<Run>),
}

pub fn spawn_scheduler(db: Db, exec: Arc<Executor>, tick: Duration) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(e) = tick_once(&db, &exec).await {
                tracing::error!("scheduler tick failed: {e:#}");
            }
        }
    });
}

/// Fires every job whose `next_run_at` has passed. Missed slots (e.g. while
/// ctm was down) collapse into a single run.
pub async fn tick_once(db: &Db, exec: &Executor) -> Result<usize> {
    let now = Utc::now();
    let mut fired = 0;
    for job in db.due_jobs(now).await? {
        let next = match job.parsed_schedule().and_then(|s| s.next_after(now)) {
            Ok(next) => next.map(|n| n + jitter(job.jitter_secs)),
            Err(e) => {
                tracing::error!(job = job.id, "bad schedule, disabling: {e:#}");
                db.set_job_fired(job.id, None, None, true).await?;
                continue;
            }
        };
        let one_shot_done = job.schedule_kind == ScheduleKind::Once && next.is_none();
        match fire_job(db, exec, &job, Trigger::Schedule).await {
            Ok(Some(run)) => {
                db.set_job_fired(job.id, Some(now), next, one_shot_done)
                    .await?;
                tracing::info!(job = job.id, run = run.id, "scheduled run queued");
                fired += 1;
            }
            Ok(None) => db.set_job_fired(job.id, None, next, one_shot_done).await?,
            Err(e) => {
                tracing::error!(job = job.id, "firing job: {e:#}");
                db.set_job_fired(job.id, None, next, one_shot_done).await?;
            }
        }
    }
    Ok(fired)
}

/// Queues a run of `job`. Returns `None` when the overlap policy skips it.
pub async fn fire_job(
    db: &Db,
    exec: &Executor,
    job: &Job,
    trigger: Trigger,
) -> Result<Option<Run>> {
    let agent = db
        .get_agent(job.agent_id)
        .await?
        .context("the job's agent no longer exists")?;

    let active = db.active_runs_for_job(job.id).await?;
    if !active.is_empty() {
        match (job.overlap, &trigger) {
            // Manual runs are explicit requests; queue them regardless.
            (_, Trigger::Manual) | (Overlap::Queue, _) => {}
            (Overlap::Skip, _) => {
                tracing::info!(job = job.id, "previous run still active; skipping");
                return Ok(None);
            }
            (Overlap::Replace, _) => {
                for run in &active {
                    exec.cancel(run.id).await?;
                }
            }
        }
    }

    let last = db.last_finished_run_for_job(job.id).await?;
    let prompt = render_prompt(job, &agent, last.as_ref(), &trigger, Utc::now());
    let run = db
        .create_run(
            agent.spec(prompt, job.timeout_secs),
            RunOrigin {
                agent: Some(&agent),
                job_id: Some(job.id),
                attempt: 1,
                ..Default::default()
            },
        )
        .await?;
    exec.enqueue(run.id);
    if !matches!(trigger, Trigger::Schedule) {
        db.set_job_fired(job.id, Some(run.created_at), job.next_run_at, false)
            .await?;
    }
    Ok(Some(run))
}

/// Called by the executor whenever a run finishes.
pub async fn after_run(db: &Db, exec: &Executor, run: &Run) -> Result<()> {
    let Some(job_id) = run.job_id else {
        return Ok(());
    };
    db.set_job_last_status(job_id, run.status).await?;
    let Some(job) = db.get_job(job_id).await? else {
        return Ok(());
    };

    if run.status == RunStatus::Failed && run.attempt <= job.max_retries {
        let delay = retry_delay(job.retry_delay_secs, run.attempt);
        let agent = db.get_agent(job.agent_id).await?;
        let retry = db
            .create_run(
                run.spec(),
                RunOrigin {
                    agent: agent.as_ref(),
                    job_id: Some(job.id),
                    attempt: run.attempt + 1,
                    not_before: Some(Utc::now() + delay),
                    ..Default::default()
                },
            )
            .await?;
        exec.enqueue(retry.id);
        tracing::info!(
            job = job.id,
            run = retry.id,
            attempt = retry.attempt,
            delay_secs = delay.num_seconds(),
            "retry queued"
        );
        return Ok(());
    }

    if run.status.is_success()
        && let Some(next_id) = job.then_job_id
    {
        match db.get_job(next_id).await? {
            Some(next) => {
                if let Some(r) =
                    fire_job(db, exec, &next, Trigger::Chain(Box::new(run.clone()))).await?
                {
                    tracing::info!(
                        job = job.id,
                        next = next.id,
                        run = r.id,
                        "chained job queued"
                    );
                }
            }
            None => tracing::warn!(job = job.id, "chained job {next_id} no longer exists"),
        }
    }
    Ok(())
}

/// Exponential backoff: delay, 2×delay, 4×delay… capped at one day.
pub fn retry_delay(base_secs: i64, attempt: i64) -> chrono::Duration {
    let factor = 1i64 << (attempt - 1).clamp(0, 16);
    chrono::Duration::seconds(base_secs.max(1).saturating_mul(factor).min(86_400))
}

/// Expands `{{placeholders}}` in a job prompt. Unknown names are left as-is.
///
/// | placeholder | value |
/// |---|---|
/// | `{{date}}`, `{{time}}`, `{{datetime}}`, `{{weekday}}` | now, in the job's time zone |
/// | `{{job}}`, `{{agent}}` | names |
/// | `{{last_status}}`, `{{last_result}}` | the job's previous finished run |
/// | `{{trigger}}` | `schedule`, `manual` or `chain` |
/// | `{{upstream_result}}` | result of the run that triggered a chained job |
pub fn render_prompt(
    job: &Job,
    agent: &Agent,
    last: Option<&Run>,
    trigger: &Trigger,
    now: DateTime<Utc>,
) -> String {
    let tz: Tz = job.timezone.parse().unwrap_or(Tz::UTC);
    let local = now.with_timezone(&tz);
    let (trigger_name, upstream) = match trigger {
        Trigger::Schedule => ("schedule", None),
        Trigger::Manual => ("manual", None),
        Trigger::Chain(run) => ("chain", run.result.clone()),
    };
    let vars: [(&str, String); 11] = [
        ("date", local.format("%Y-%m-%d").to_string()),
        ("time", local.format("%H:%M").to_string()),
        ("datetime", local.to_rfc3339()),
        ("weekday", local.format("%A").to_string()),
        ("job", job.name.clone()),
        ("agent", agent.name.clone()),
        ("trigger", trigger_name.to_string()),
        (
            "last_status",
            last.map(|r| r.status.to_string())
                .unwrap_or_else(|| "none".into()),
        ),
        (
            "last_result",
            last.and_then(|r| r.result.clone()).unwrap_or_default(),
        ),
        ("upstream_result", upstream.unwrap_or_default()),
        ("timezone", tz.to_string()),
    ];
    let mut out = String::with_capacity(job.prompt.len());
    let mut rest = job.prompt.as_str();
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewAgent, NewJob};
    use chrono::TimeZone;

    #[test]
    fn backoff() {
        assert_eq!(retry_delay(60, 1).num_seconds(), 60);
        assert_eq!(retry_delay(60, 3).num_seconds(), 240);
        assert_eq!(retry_delay(60, 40).num_seconds(), 86_400);
    }

    #[tokio::test]
    async fn templates() {
        let db = Db::memory().await.unwrap();
        let agent = db
            .create_agent(NewAgent {
                name: "triage".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let job = db
            .create_job(NewJob {
                name: "standup".into(),
                agent_id: agent.id,
                prompt: "{{agent}}/{{job}} on {{weekday}} {{date}} {{time}} ({{trigger}}): {{upstream_result}} {{nope}} {{".into(),
                timezone: "Asia/Tokyo".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 4, 23, 30, 0).unwrap(); // Tue 08:30 in Tokyo
        let mut upstream = db
            .create_run(agent.spec("x".into(), None), RunOrigin::default())
            .await
            .unwrap();
        upstream.result = Some("3 issues".into());
        let out = render_prompt(&job, &agent, None, &Trigger::Chain(Box::new(upstream)), now);
        assert_eq!(
            out,
            "triage/standup on Tuesday 2026-05-05 08:30 (chain): 3 issues {{nope}} {{"
        );
    }
}
