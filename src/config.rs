use std::{net::SocketAddr, path::PathBuf};

use clap::Args;

/// Server configuration. Every option can also be set through the
/// environment so the container image can be configured without flags.
#[derive(Args, Debug, Clone)]
pub struct Config {
    /// Address the dashboard listens on.
    #[arg(long, env = "CTM_BIND", default_value = "127.0.0.1:7878")]
    pub bind: SocketAddr,

    /// Directory for the SQLite database, run logs and local workspaces.
    #[arg(long, env = "CTM_DATA_DIR", default_value = "./ctm-data")]
    pub data_dir: PathBuf,

    /// Shared secret for the dashboard and API. Leave unset to disable auth
    /// (only do that when bound to localhost).
    #[arg(long = "auth-token", env = "CTM_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// Maximum number of Claude processes running at the same time.
    #[arg(long, env = "CTM_MAX_CONCURRENT", default_value_t = 4)]
    pub max_concurrent: usize,

    /// Claude Code binary used for local runs.
    #[arg(long, env = "CTM_CLAUDE_BIN", default_value = "claude")]
    pub claude_bin: String,

    /// Arguments always passed to `claude` (before per-task extra args).
    #[arg(
        long,
        env = "CTM_CLAUDE_ARGS",
        default_value = "--output-format stream-json --verbose"
    )]
    pub claude_args: String,

    /// Docker binary used for docker-mode runs.
    #[arg(long, env = "CTM_DOCKER_BIN", default_value = "docker")]
    pub docker_bin: String,

    /// Default image for docker-mode runs (see docker/runner.Dockerfile).
    #[arg(long, env = "CTM_DOCKER_IMAGE", default_value = "ctm-runner:latest")]
    pub docker_image: String,

    /// Extra `docker run` arguments, e.g. "--network=host --cpus=2".
    #[arg(long, env = "CTM_DOCKER_ARGS", default_value = "")]
    pub docker_args: String,

    /// Environment variables forwarded from ctm into every run
    /// (comma separated names; values are read from ctm's own environment).
    #[arg(
        long,
        env = "CTM_FORWARD_ENV",
        default_value = "ANTHROPIC_API_KEY,CLAUDE_CODE_OAUTH_TOKEN,ANTHROPIC_BASE_URL,GH_TOKEN,GITHUB_TOKEN"
    )]
    pub forward_env: String,

    /// Seconds between scheduler ticks.
    #[arg(long, env = "CTM_SCHEDULER_TICK", default_value_t = 5)]
    pub scheduler_tick: u64,
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("ctm.sqlite")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn workspaces_dir(&self) -> PathBuf {
        self.data_dir.join("workspaces")
    }

    pub fn log_path(&self, run_id: i64) -> PathBuf {
        self.logs_dir().join(format!("{run_id}.log"))
    }

    pub fn forwarded_env(&self) -> Vec<String> {
        self.forward_env
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }
}

/// Splits a whitespace separated argument string, honouring simple
/// single/double quotes so `--append-system-prompt "be terse"` works.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                has_token = true;
            }
            (None, c) if c.is_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            (None, c) => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split_args;

    #[test]
    fn splits_quoted_args() {
        assert_eq!(
            split_args(r#"--a b --c "d e" 'f' """#),
            vec!["--a", "b", "--c", "d e", "f", ""]
        );
        assert!(split_args("   ").is_empty());
    }
}
