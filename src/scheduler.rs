//! Cron scheduler: periodically turns due tasks into queued runs.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use chrono::Utc;

use crate::{
    db::{Db, next_fire},
    executor::Executor,
};

pub fn spawn(db: Db, exec: Arc<Executor>, tick: Duration) {
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

/// Fires every task whose `next_run_at` has passed. A task that still has
/// a queued or running run is skipped for this slot rather than piling up
/// overlapping runs. Missed slots (e.g. while ctm was down) collapse into a
/// single run.
pub async fn tick_once(db: &Db, exec: &Executor) -> Result<usize> {
    let now = Utc::now();
    let mut fired = 0;
    for task in db.due_tasks(now).await? {
        let next = task.cron.as_deref().and_then(|c| next_fire(c, now).ok());
        if db.task_has_active_run(task.id).await? {
            tracing::info!(task = task.id, "previous run still active; skipping slot");
            db.set_task_schedule(task.id, None, next).await?;
            continue;
        }
        let run = db.create_run(task.spec(), Some(task.id), None).await?;
        db.set_task_schedule(task.id, Some(now), next).await?;
        exec.enqueue(run.id);
        tracing::info!(task = task.id, run = run.id, "scheduled run queued");
        fired += 1;
    }
    Ok(fired)
}
