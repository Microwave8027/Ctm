CREATE TABLE IF NOT EXISTS tasks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT    NOT NULL,
    prompt       TEXT    NOT NULL,
    cron         TEXT,
    mode         TEXT    NOT NULL DEFAULT 'local',
    image        TEXT,
    repo         TEXT,
    model        TEXT,
    extra_args   TEXT,
    enabled      INTEGER NOT NULL DEFAULT 1,
    next_run_at  TEXT,
    last_run_at  TEXT,
    created_at   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id        INTEGER REFERENCES tasks(id) ON DELETE SET NULL,
    parent_run_id  INTEGER REFERENCES runs(id) ON DELETE SET NULL,
    prompt         TEXT    NOT NULL,
    mode           TEXT    NOT NULL,
    image          TEXT,
    repo           TEXT,
    model          TEXT,
    extra_args     TEXT,
    workspace      TEXT    NOT NULL,
    status         TEXT    NOT NULL DEFAULT 'queued',
    exit_code      INTEGER,
    session_id     TEXT,
    result         TEXT,
    cost_usd       REAL,
    error          TEXT,
    created_at     TEXT    NOT NULL,
    started_at     TEXT,
    finished_at    TEXT
);

CREATE INDEX IF NOT EXISTS runs_status_idx ON runs(status);
CREATE INDEX IF NOT EXISTS runs_task_idx ON runs(task_id);
