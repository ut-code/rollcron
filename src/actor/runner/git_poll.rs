use super::ConfigUpdate;
use crate::config;
use crate::git;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::interval;
use tracing::{error, info};
use xtra::prelude::*;
use xtra::refcount::Weak;

const CONFIG_FILE: &str = "rollcron.yaml";

pub async fn run<A>(source: String, pull_interval: Duration, addr: Address<A, Weak>)
where
    A: Handler<ConfigUpdate>,
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
            }
        }
    }
}

fn load_config(sot_path: &PathBuf) -> anyhow::Result<(config::RunnerConfig, Vec<config::Job>)> {
    let config_path = sot_path.join(CONFIG_FILE);
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", config_path.display(), e))?;
    config::parse_config(&content)
}
