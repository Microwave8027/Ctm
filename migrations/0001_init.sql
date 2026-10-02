-- Agents: reusable Claude configurations (where and how Claude runs).
CREATE TABLE IF NOT EXISTS agents (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    name                TEXT    NOT NULL UNIQUE,
    description         TEXT,
    mode                TEXT    NOT NULL DEFAULT 'local',
    model               TEXT,
    image               TEXT,
    repo                TEXT,
    environment         TEXT,   -- cloud: self-hosted environment id (ccpool_...)
    cloud_session       TEXT,   -- cloud: existing session to message instead of creating one
    system_prompt       TEXT,   -- passed as --append-system-prompt
    permission_mode     TEXT,
    allowed_tools       TEXT,
    extra_args          TEXT,
    persistent_session  INTEGER NOT NULL DEFAULT 0, -- each run resumes the previous session
    session_id          TEXT,   -- last session id seen for this agent
    created_at          TEXT    NOT NULL,
    updated_at          TEXT    NOT NULL
);

-- Jobs: schedules that fire an agent with a prompt.
CREATE TABLE IF NOT EXISTS jobs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT    NOT NULL,
    agent_id          INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    prompt            TEXT    NOT NULL,
    schedule_kind     TEXT    NOT NULL DEFAULT 'manual', -- cron | interval | once | manual
    schedule          TEXT,
    timezone          TEXT    NOT NULL DEFAULT 'UTC',
    jitter_secs       INTEGER NOT NULL DEFAULT 0,
    overlap           TEXT    NOT NULL DEFAULT 'skip',   -- skip | queue | replace
    timeout_secs      INTEGER,
    max_retries       INTEGER NOT NULL DEFAULT 0,
    retry_delay_secs  INTEGER NOT NULL DEFAULT 60,
    then_job_id       INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
    enabled           INTEGER NOT NULL DEFAULT 1,
    next_run_at       TEXT,
    last_run_at       TEXT,
    last_status       TEXT,
    created_at        TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id         INTEGER REFERENCES agents(id) ON DELETE SET NULL,
    job_id           INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
    parent_run_id    INTEGER REFERENCES runs(id) ON DELETE SET NULL,
    prompt           TEXT    NOT NULL,
    mode             TEXT    NOT NULL,
    model            TEXT,
    image            TEXT,
    repo             TEXT,
    environment      TEXT,
    cloud_session    TEXT,
    system_prompt    TEXT,
    permission_mode  TEXT,
    allowed_tools    TEXT,
    extra_args       TEXT,
    resume_session   TEXT,
    workspace        TEXT    NOT NULL,
    status           TEXT    NOT NULL DEFAULT 'queued',
    attempt          INTEGER NOT NULL DEFAULT 1,
    timeout_secs     INTEGER,
    not_before       TEXT,
    exit_code        INTEGER,
    session_id       TEXT,
    session_url      TEXT,
    result           TEXT,
    cost_usd         REAL,
    error            TEXT,
    created_at       TEXT    NOT NULL,
    started_at       TEXT,
    finished_at      TEXT
);

CREATE INDEX IF NOT EXISTS runs_status_idx ON runs(status);
CREATE INDEX IF NOT EXISTS runs_job_idx ON runs(job_id);
CREATE INDEX IF NOT EXISTS runs_agent_idx ON runs(agent_id);

-- Small key/value store (Claude credentials managed from the dashboard).
CREATE TABLE IF NOT EXISTS settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
