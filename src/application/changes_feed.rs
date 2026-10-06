use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use crate::domain::services::ConfigFetcher;

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct ChangesResponse {
    results: Vec<ChangeEntry>,
    #[serde(default)]
    last_seq: String,
}

#[derive(Deserialize)]
struct ChangeEntry {
    id: String,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    doc: Option<ChangeDoc>,
}

#[derive(Deserialize)]
struct ChangeDoc {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    internal_id: String,
    #[serde(default)]
    config_version: u64,
}

pub struct ChangesFeedListener {
    couchdb_url: String,
    db_name: String,
    username: String,
    password: String,
    config_fetcher: Arc<dyn ConfigFetcher>,
}

impl ChangesFeedListener {
    pub fn new(
        couchdb_url: String,
        db_name: String,
        username: String,
        password: String,
        config_fetcher: Arc<dyn ConfigFetcher>,
    ) -> Self {
        Self {
            couchdb_url,
            db_name,
            username,
            password,
            config_fetcher,
        }
    }

    pub async fn run(&self) {
        let client = match Client::builder().timeout(Duration::from_secs(30)).build() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "Changes feed: failed to build HTTP client, listener disabled");
                return;
            }
        };

        let mut since = "now".to_string();
        let mut backoff = POLL_INTERVAL;

        loop {
            match self.poll_changes(&client, &since).await {
                Ok(response) => {
                    backoff = POLL_INTERVAL;

                    for change in &response.results {
                        if change.deleted {
                            continue;
                        }

                        if let Some(doc) = &change.doc {
                            if doc.r#type == "client_config" {
                                let internal_id = if doc.internal_id.is_empty() {
                                    change.id.as_str()
                                } else {
                                    doc.internal_id.as_str()
                                };

                                tracing::info!(
                                    doc_id = %change.id,
                                    internal_id = %internal_id,
                                    config_version = doc.config_version,
                                    "Config change detected via _changes feed"
                                );
                                self.config_fetcher.invalidate_cache(internal_id).await;
                            }
                        }
                    }

                    if !response.last_seq.is_empty() {
                        since = response.last_seq;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, backoff_secs = backoff.as_secs(), "Changes feed error, backing off before retry");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn poll_changes(&self, client: &Client, since: &str) -> Result<ChangesResponse, String> {
        let url = format!("{}/{}/_changes", self.couchdb_url, self.db_name);

        let resp = client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[
                ("feed", "normal"),
                ("since", since),
                ("include_docs", "true"),
            ])
            .send()
            .await
            .map_err(|e| format!("CouchDB _changes request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("CouchDB _changes error {}: {}", status, body));
        }

        resp.json::<ChangesResponse>()
            .await
            .map_err(|e| format!("Failed to parse _changes response: {}", e))
    }
}
