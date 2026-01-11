//! Discord webhook notifications.

use serde::Serialize;
use tracing::{error, info};

/// Information about a failed job.
pub struct JobFailure<'a> {
    pub job_id: &'a str,
    pub job_name: &'a str,
    pub error: String,
    pub stderr: String,
    pub attempts: u32,
}

/// Send a Discord notification for a job failure.
pub async fn send_job_failure(url: &str, failure: &JobFailure<'_>) {
    let payload = build_job_failure_payload(failure);
    send_discord(url, &payload).await;
}

/// Send a Discord notification for a config parse error.
pub async fn send_config_error(url: &str, error: &str) {
    let payload = build_config_error_payload(error);
    send_discord(url, &payload).await;
}

async fn send_discord(url: &str, payload: &DiscordPayload) {
    let client = reqwest::Client::new();
    match client.post(url).json(payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(target: "rollcron::webhook", url = %url, "Notification sent");
        }
        Ok(resp) => {
            error!(target: "rollcron::webhook", url = %url, status = %resp.status(), "Failed to send notification");
        }
        Err(e) => {
            error!(target: "rollcron::webhook", url = %url, error = %e, "Failed to send notification");
        }
    }
}

// === Internal ===

#[derive(Serialize)]
struct DiscordPayload {
    embeds: Vec<DiscordEmbed>,
}

#[derive(Serialize)]
struct DiscordEmbed {
    title: String,
    color: u32,
    fields: Vec<DiscordField>,
}

#[derive(Serialize)]
struct DiscordField {
    name: &'static str,
    value: String,
    inline: bool,
}

fn build_job_failure_payload(failure: &JobFailure<'_>) -> DiscordPayload {
    let mut fields = vec![
        DiscordField {
            name: "Job",
            value: format!("`{}`", failure.job_id),
            inline: true,
        },
        DiscordField {
            name: "Attempts",
            value: failure.attempts.to_string(),
            inline: true,
        },
        DiscordField {
            name: "Error",
            value: failure.error.clone(),
            inline: false,
        },
    ];

    if !failure.stderr.is_empty() {
        let truncated = truncate(&failure.stderr, 1000);
        fields.push(DiscordField {
            name: "Stderr",
            value: format!("```\n{}\n```", truncated),
            inline: false,
        });
    }

    DiscordPayload {
        embeds: vec![DiscordEmbed {
            title: format!("[rollcron] Job '{}' failed", failure.job_name),
            color: 0xED4245, // Discord red
            fields,
        }],
    }
}

fn build_config_error_payload(err: &str) -> DiscordPayload {
    let truncated = truncate(err, 1000);
    DiscordPayload {
        embeds: vec![DiscordEmbed {
            title: "[rollcron] Config parse error".to_string(),
            color: 0xFFA500, // Orange
            fields: vec![DiscordField {
                name: "Error",
                value: format!("```\n{}\n```", truncated),
                inline: false,
            }],
        }],
    }
}

fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        &s[..max_len]
    }
}
