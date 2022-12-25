use serde_derive::{Deserialize, Serialize};
use std::sync::Arc;
use axum::http::StatusCode;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Message, Transport};
use lettre::transport::smtp::extension::ClientId;
use lettre::transport::smtp::client::{SmtpConnection, TlsParametersBuilder};
use std::time::Duration;

use crate::DbCtx;

pub struct RemoteNotifier {
    pub remote_path: String,
    pub notifier: NotifierConfig,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum NotifierConfig {
    GitHub {
        token: String,
    },
    Email {
        username: String,
        password: String,
        mailserver: String,
        from: String,
        to: String,
    }
}

impl NotifierConfig {
    pub fn github_from_file(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("can't read notifier config at {}: {:?}", path, e))?;
        let config = serde_json::from_slice(&bytes)
            .map_err(|e| format!("can't deserialize notifier config at {}: {:?}", path, e))?;

        if matches!(config, NotifierConfig::GitHub { .. }) {
            Ok(config)
        } else {
            Err(format!("config at {} doesn't look like a github config (but was otherwise valid?)", path))
        }
    }

    pub fn email_from_file(path: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("can't read notifier config at {}: {:?}", path, e))?;
        let config = serde_json::from_slice(&bytes)
            .map_err(|e| format!("can't deserialize notifier config at {}: {:?}", path, e))?;

        if matches!(config, NotifierConfig::Email { .. }) {
            Ok(config)
        } else {
            Err(format!("config at {} doesn't look like an email config (but was otherwise valid?)", path))
        }
    }
}

impl RemoteNotifier {
    pub async fn tell_pending_job(&self, ctx: &Arc<DbCtx>, repo_id: u64, sha: &str, job_id: u64) -> Result<(), String> {
        self.tell_job_status(
            ctx,
            repo_id, sha, job_id,
            "pending", "build is queued", &format!("https://{}/{}/{}", "ci.butactuallyin.space", &self.remote_path, sha)
        ).await
    }

    pub async fn tell_complete_job(&self, ctx: &Arc<DbCtx>, repo_id: u64, sha: &str, job_id: u64, desc: Result<String, String>) -> Result<(), String> {
        match desc {
            Ok(status) => {
                self.tell_job_status(
                    ctx,
                    repo_id, sha, job_id,
                    "success", &status, &format!("https://{}/{}/{}", "ci.butactuallyin.space", &self.remote_path, sha)
                ).await
            },
            Err(status) => {
                self.tell_job_status(
                    ctx,
                    repo_id, sha, job_id,
                    "failure", &status, &format!("https://{}/{}/{}", "ci.butactuallyin.space", &self.remote_path, sha)
                ).await
            }
        }
    }

    pub async fn tell_job_status(&self, ctx: &Arc<DbCtx>, repo_id: u64, sha: &str, job_id: u64, state: &str, desc: &str, target_url: &str) -> Result<(), String> {
        match &self.notifier {
            NotifierConfig::GitHub { token } => {
                let status_info = serde_json::json!({
                    "state": state,
                    "description": desc,
                    "target_url": target_url,
                    "context": "actuallyinspace runner",
                });

                // TODO: should pool (probably in ctx?) to have an upper bound in concurrent
                // connections.
                let client = reqwest::Client::new();
                let req = client.post(&format!("https://api.github.com/repos/{}/statuses/{}", &self.remote_path, sha))
                    .body(serde_json::to_string(&status_info).expect("can stringify json"))
                    .header("content-type", "application/json")
                    .header("user-agent", "iximeow")
                    .header("authorization", format!("Bearer {}", token))
                    .header("accept", "application/vnd.github+json");
                eprintln!("sending {:?}", req);
                eprintln!("  body: {}", serde_json::to_string(&status_info).expect("can stringify json"));
                let res = req
                    .send()
                    .await;

                match res {
                    Ok(res) => {
                        if res.status() == StatusCode::OK || res.status() == StatusCode::CREATED{
                            Ok(())
                        } else {
                            Err(format!("bad response: {}, response data: {:?}", res.status().as_u16(), res))
                        }
                    }
                    Err(e) => {
                        Err(format!("failure sending request: {:?}", e))
                    }
                }
            }
            NotifierConfig::Email { username, password, mailserver, from, to } => {
                panic!("should send an email saying that a job is now pending for `sha`")
            }
        }
    }
}
