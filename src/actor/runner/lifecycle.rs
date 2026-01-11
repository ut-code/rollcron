use super::{GetJobActors, RespawnJob};
use crate::actor::job::JobActor;
use std::time::Duration;
use tracing::warn;
use xtra::prelude::*;
use xtra::refcount::Weak;

/// Supervision interval - how often to check if job actors are alive
const SUPERVISION_INTERVAL: Duration = Duration::from_secs(5);

pub async fn supervise<A>(addr: Address<A, Weak>)
where
    A: Handler<GetJobActors, Return = std::collections::HashMap<String, Address<JobActor>>>
        + Handler<RespawnJob>,
{
    let mut interval = tokio::time::interval(SUPERVISION_INTERVAL);

    loop {
        interval.tick().await;

        // Get current job actors
        let job_actors = match addr.send(GetJobActors).await {
            Ok(actors) => actors,
            Err(_) => break, // Runner actor stopped
        };

        // Check each actor's health
        for (job_id, job_addr) in job_actors {
            if !job_addr.is_connected() {
                warn!(target: "rollcron::runner", job_id = %job_id, "Job actor disconnected, respawning");
                if addr.send(RespawnJob { job_id }).await.is_err() {
                    break;
                }
            }
        }
    }
}
