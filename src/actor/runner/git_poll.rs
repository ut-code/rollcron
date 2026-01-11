use super::{ConfigUpdate, GetRunnerConfig};
use crate::config::{self, RunnerConfig};
use crate::{env, git, webhook};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::interval;
use tracing::{error, info, warn};
use xtra::prelude::*;
use xtra::refcount::Weak;

const CONFIG_FILE: &str = "rollcron.yaml";

pub async fn run<A>(source: String, pull_interval: Duration, addr: Address<A, Weak>)
where
    A: Handler<ConfigUpdate> + Handler<GetRunnerConfig, Return = RunnerConfig>,
{
    let mut ticker = interval(pull_interval);

    loop {
        ticker.tick().await;

        let (sot_path, update_info) = match git::ensure_repo(&source) {
            Ok(r) => r,
            Err(e) => {
                error!(target: "rollcron::runner", error = %e, "Git sync failed");
                continue;
            }
        };

        let Some(range) = update_info else {
            continue;
        };

        info!(target: "rollcron::runner", range = %range, "Pulled updates");

        match load_config(&sot_path) {
            Ok((runner, jobs)) => {
                if let Err(e) = addr
                    .send(ConfigUpdate {
                        sot_path,
                        runner,
                        jobs,
                    })
                    .await
                {
                    error!(target: "rollcron::runner", error = %e, "Failed to send config update");
                }
            }
            Err(e) => {
                error!(target: "rollcron::runner", error = %e, "Failed to reload config");
                notify_config_error(&addr, &sot_path, &e.to_string()).await;
            }
        }
    }
}

async fn notify_config_error<A>(addr: &Address<A, Weak>, sot_path: &PathBuf, error: &str)
where
    A: Handler<GetRunnerConfig, Return = RunnerConfig>,
{
    let runner = match addr.send(GetRunnerConfig).await {
        Ok(r) => r,
        Err(_) => return, // Runner stopped
    };

    if runner.webhook.is_empty() {
        return;
    }

    let runner_env = load_runner_env(sot_path, &runner);
    for wh in &runner.webhook {
        let url = wh.to_url(runner_env.as_ref());
        if url.contains('$') {
            warn!(target: "rollcron::webhook", url = %url, "Webhook URL contains unexpanded variable, skipping");
            continue;
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            warn!(target: "rollcron::webhook", url = %url, "Webhook URL must start with http:// or https://, skipping");
            continue;
        }
        webhook::send_config_error(&url, error).await;
    }
}

fn load_runner_env(sot_path: &PathBuf, runner: &RunnerConfig) -> Option<HashMap<String, String>> {
    let mut env_vars = HashMap::new();

    if let Some(env_file_path) = &runner.env_file {
        let expanded = env::expand_string(env_file_path);
        let full_path = sot_path.join(&expanded);
        match env::load_env_from_path(&full_path) {
            Ok(vars) => env_vars.extend(vars),
            Err(e) => {
                warn!(target: "rollcron::webhook", error = %e, "Failed to load runner env_file");
                return None;
            }
        }
    }

    if let Some(runner_env) = &runner.env {
        for (k, v) in runner_env {
            env_vars.insert(k.clone(), env::expand_string(v));
        }
    }

    Some(env_vars)
}

fn load_config(sot_path: &PathBuf) -> anyhow::Result<(config::RunnerConfig, Vec<config::Job>)> {
    let config_path = sot_path.join(CONFIG_FILE);
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", config_path.display(), e))?;
    config::parse_config(&content)
}
