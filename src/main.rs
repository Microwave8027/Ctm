mod cli;
mod config;
mod db;
mod executor;
mod scheduler;
mod web;

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// ctm — a multiplexer for headless Claude Code runs.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    remote: cli::Remote,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the dashboard, API, scheduler and executor.
    Serve(config::Config),
    #[command(flatten)]
    Client(cli::Cmd),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CTM_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn,sqlx=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve(cfg) => serve(cfg).await,
        Command::Client(cmd) => cli::main(cli.remote, cmd).await,
    }
}

async fn serve(cfg: config::Config) -> Result<()> {
    if cfg.token.is_none() && !cfg.bind.ip().is_loopback() {
        tracing::warn!(
            "CTM_TOKEN is not set and ctm is listening on {} — anyone who can reach it can run Claude",
            cfg.bind
        );
    }
    tokio::fs::create_dir_all(&cfg.data_dir).await?;
    let cfg = Arc::new(cfg);
    let db = db::Db::open(&cfg.db_path()).await?;
    let exec = executor::Executor::start(cfg.clone(), db.clone()).await?;
    scheduler::spawn(
        db.clone(),
        exec.clone(),
        Duration::from_secs(cfg.scheduler_tick.max(1)),
    );

    let app = web::router(web::AppState {
        cfg: cfg.clone(),
        db,
        exec,
    });
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!("ctm listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
