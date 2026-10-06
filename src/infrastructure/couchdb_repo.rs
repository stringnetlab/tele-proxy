use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use moka::future::Cache;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::domain::errors::ProxyError;
use crate::domain::models::{
    ClientConfig, ClientConfigUpdate, ErrorHandlingConfig, ErrorMode, RateLimitConfig,
    ScriptingConfig,
};
use crate::domain::services::ConfigFetcher;

pub struct CouchDbRepository {
    base_url: String,
    db_name: String,
    username: String,
    password: String,
    client: Client,
    local_cache: Cache<String, ClientConfig>,
    id_index: Arc<RwLock<HashMap<String, (String, String)>>>,
}

#[derive(Serialize)]
struct DesignDoc {
    views: Views,
}

#[derive(Serialize)]
struct Views {
    by_crypt_id: ViewDef,
    by_token_hash: ViewDef,
}

#[derive(Serialize)]
struct ViewDef {
    map: String,
}

#[derive(Deserialize)]
struct ViewRow {
    id: String,
}

#[derive(Deserialize)]
struct ViewResponse {
    rows: Vec<ViewRow>,
}

impl CouchDbRepository {
    pub fn new(
        base_url: String,
        db_name: String,
        username: String,
        password: String,
        cache_ttl_seconds: u64,
        cache_max_capacity: u64,
    ) -> Self {
        let local_cache = Cache::builder()
            .max_capacity(cache_max_capacity)
            .time_to_live(std::time::Duration::from_secs(cache_ttl_seconds))
            .build();

        let client = Client::new();

        Self {
            base_url,
            db_name,
            username,
            password,
            client,
            local_cache,
            id_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn ensure_design_doc(&self) -> Result<(), ProxyError> {
        let design_doc_id = "_design/proxy_lookup";
        let url = format!("{}/{}/{}", self.base_url, self.db_name, design_doc_id);

        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("CouchDB connection failed: {}", e),
            })?;

        if resp.status().is_success() {
            return Ok(());
        }

        let design_doc = DesignDoc {
            views: Views {
                by_crypt_id: ViewDef {
                    map: r#"function(doc) { if (doc.type === 'client_config' && doc.crypt_id) { emit(doc.crypt_id, null); } }"#.to_string(),
                },
                by_token_hash: ViewDef {
                    map: r#"function(doc) { if (doc.type === 'client_config' && doc.bearer_token_hash) { emit(doc.bearer_token_hash, null); } }"#.to_string(),
                },
            },
        };

        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&design_doc)
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to create design doc: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Failed to create design doc: {}", body),
            });
        }

        tracing::info!("CouchDB design doc created");
        Ok(())
    }

    /// Si la DB no tiene documentos de configuración (type === "client_config"), crea un cliente
    /// demo con permisos mínimos para pruebas y demos. El bearer token es `demo-token` (hash SHA-256
    /// calculado en runtime). La whitelist solo permite `example.com`, rate limit de 5 req/60s, sin
    /// scripting, error handling transparent.
    pub async fn seed_demo_client_if_empty(&self) -> Result<(), ProxyError> {
        let url = self.view_url("by_crypt_id");
        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("limit", "1")])
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("CouchDB view check failed: {}", e),
            })?;

        if !resp.status().is_success() {
            return Ok(());
        }

        let view_resp: ViewResponse = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("Failed to parse view response: {}", e),
        })?;

        if !view_resp.rows.is_empty() {
            return Ok(());
        }

        let demo_token = "demo-token";
        let mut hasher = Sha256::new();
        hasher.update(demo_token.as_bytes());
        let result = hasher.finalize();
        let hex_str = result.iter().map(|b| format!("{:02x}", b)).collect::<String>();
        let token_hash = format!("sha256:{}", hex_str);

        let demo_config = ClientConfig {
            id: "demo_client".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "demo_client".to_string(),
            crypt_id: nanoid::nanoid!(12),
            bearer_token_hash: token_hash,
            config_version: 1,
            whitelist: vec!["example.com".to_string()],
            rate_limit: RateLimitConfig {
                max_requests: 5,
                window_seconds: 60,
            },
            max_scripting_body_bytes: 0,
            scripting: ScriptingConfig {
                enabled: false,
                code: String::new(),
                code_hash: String::new(),
            },
            error_handling: ErrorHandlingConfig {
                mode: ErrorMode::Transparent,
                fallback_urls: HashMap::new(),
            },
        };

        let url = self.doc_url(&demo_config.id);
        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&demo_config)
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to create demo client: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Failed to create demo client: {}", body),
            });
        }

        tracing::info!(
            crypt_id = %demo_config.crypt_id,
            "Demo client seeded (bearer token: 'demo-token', whitelist: example.com, 5 req/60s)"
        );
        Ok(())
    }

    fn doc_url(&self, doc_id: &str) -> String {
        format!("{}/{}/{}", self.base_url, self.db_name, doc_id)
    }

    fn view_url(&self, view_name: &str) -> String {
        format!(
            "{}/{}/_design/proxy_lookup/_view/{}",
            self.base_url, self.db_name, view_name
        )
    }

    async fn fetch_doc_by_id(&self, doc_id: &str) -> Result<ClientConfig, ProxyError> {
        let url = self.doc_url(doc_id);

        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("CouchDB request failed: {}", e),
            })?;

        match resp.status().as_u16() {
            200 => {}
            404 => {
                return Err(ProxyError::ConfigNotFound {
                    internal_id: doc_id.to_string(),
                })
            }
            status => {
                let body = resp.text().await.unwrap_or_default();
                return Err(ProxyError::Internal {
                    reason: format!("CouchDB error {}: {}", status, body),
                });
            }
        }

        let config: ClientConfig = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("Failed to parse CouchDB response: {}", e),
        })?;

        Ok(config)
    }

    async fn fetch_doc_by_view(
        &self,
        view_name: &str,
        key: &str,
    ) -> Result<ClientConfig, ProxyError> {
        let url = self.view_url(view_name);

        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("key", format!("\"{}\"", key))])
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("CouchDB view query failed: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("CouchDB view error: {}", body),
            });
        }

        let view_resp: ViewResponse = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("Failed to parse view response: {}", e),
        })?;

        let row = view_resp
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| ProxyError::ConfigNotFound {
                internal_id: format!("{}={}", view_name, key),
            })?;

        self.fetch_doc_by_id(&row.id).await
    }

    async fn cache_config(&self, config: &ClientConfig) {
        self.local_cache
            .insert(config.crypt_id.clone(), config.clone())
            .await;
        self.local_cache
            .insert(config.bearer_token_hash.clone(), config.clone())
            .await;
        let mut index = self.id_index.write().await;
        index.insert(
            config.internal_id.clone(),
            (config.crypt_id.clone(), config.bearer_token_hash.clone()),
        );
    }
}

#[async_trait]
impl ConfigFetcher for CouchDbRepository {
    async fn get_by_crypt_id(&self, crypt_id: &str) -> Result<ClientConfig, ProxyError> {
        if let Some(cached) = self.local_cache.get(crypt_id).await {
            return Ok(cached);
        }

        match self.fetch_doc_by_view("by_crypt_id", crypt_id).await {
            Ok(config) => {
                self.cache_config(&config).await;
                Ok(config)
            }
            Err(e) => {
                if is_connection_error(&e) {
                    tracing::warn!(
                        crypt_id = %crypt_id,
                        error = %e,
                        "CouchDB unavailable, returning degraded default config"
                    );
                    Ok(ClientConfig::default_degraded())
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn get_by_token_hash(&self, token_hash: &str) -> Result<ClientConfig, ProxyError> {
        if let Some(cached) = self.local_cache.get(token_hash).await {
            return Ok(cached);
        }

        match self.fetch_doc_by_view("by_token_hash", token_hash).await {
            Ok(config) => {
                self.cache_config(&config).await;
                Ok(config)
            }
            Err(e) => {
                if is_connection_error(&e) {
                    tracing::warn!(
                        error = %e,
                        "CouchDB unavailable, returning degraded default config"
                    );
                    Ok(ClientConfig::default_degraded())
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn update_config(
        &self,
        internal_id: &str,
        update: &ClientConfigUpdate,
    ) -> Result<ClientConfig, ProxyError> {
        let mut config = self.fetch_doc_by_id(internal_id).await?;

        if let Some(ref whitelist) = update.whitelist {
            config.whitelist = whitelist.clone();
        }
        if let Some(ref rate_limit) = update.rate_limit {
            config.rate_limit = rate_limit.clone();
        }
        if let Some(max_bytes) = update.max_scripting_body_bytes {
            config.max_scripting_body_bytes = max_bytes;
        }
        if let Some(ref scripting) = update.scripting {
            if let Some(enabled) = scripting.enabled {
                config.scripting.enabled = enabled;
            }
            if let Some(ref code) = scripting.code {
                config.scripting.code = code.clone();
            }
            if let Some(ref code_hash) = scripting.code_hash {
                config.scripting.code_hash = code_hash.clone();
            }
        }
        if let Some(ref error_handling) = update.error_handling {
            config.error_handling = error_handling.clone();
        }

        config.config_version += 1;

        let url = self.doc_url(internal_id);
        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&config)
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("CouchDB update failed: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("CouchDB update error: {}", body),
            });
        }

        #[derive(Deserialize)]
        struct UpdateResp {
            rev: String,
        }

        let update_resp: UpdateResp = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("Failed to parse update response: {}", e),
        })?;

        config.rev = Some(update_resp.rev);

        self.local_cache.invalidate(&config.crypt_id).await;
        self.local_cache.invalidate(&config.bearer_token_hash).await;
        self.cache_config(&config).await;

        Ok(config)
    }

    async fn rotate_crypt_id(&self, internal_id: &str) -> Result<String, ProxyError> {
        let mut config = self.fetch_doc_by_id(internal_id).await?;

        let old_crypt_id = config.crypt_id.clone();
        let new_crypt_id = nanoid::nanoid!(12);

        config.crypt_id = new_crypt_id.clone();
        config.config_version += 1;

        let url = self.doc_url(internal_id);
        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&config)
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("CouchDB rotate failed: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("CouchDB rotate error: {}", body),
            });
        }

        self.local_cache.invalidate(&old_crypt_id).await;
        self.local_cache.invalidate(&config.bearer_token_hash).await;
        self.cache_config(&config).await;

        // Pista de auditoría del diccionario (`docs/ERROR_DICTIONARY.md`, § Eventos que no son
        // errores): `internal_id` es el único identificador que sobrevive al cambio de `crypt_id`.
        tracing::info!(
            event = "crypt_id_rotated",
            internal_id = %internal_id,
            old_crypt_id = %old_crypt_id,
            new_crypt_id = %new_crypt_id,
            "crypt_id rotated"
        );

        Ok(new_crypt_id)
    }

    async fn invalidate_cache(&self, internal_id: &str) {
        let mut index = self.id_index.write().await;
        if let Some((crypt_id, token_hash)) = index.remove(internal_id) {
            self.local_cache.invalidate(&crypt_id).await;
            self.local_cache.invalidate(&token_hash).await;
            tracing::info!(
                internal_id = %internal_id,
                crypt_id = %crypt_id,
                "Cache invalidated via _changes feed"
            );
        }
    }
}

pub async fn verify_couchdb_connection(
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<(), String> {
    let client = Client::new();
    let resp = client
        .get(base_url)
        .basic_auth(username, Some(password))
        .send()
        .await
        .map_err(|e| format!("CouchDB connection failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("CouchDB returned status {}", resp.status()));
    }

    Ok(())
}

fn is_connection_error(error: &ProxyError) -> bool {
    match error {
        ProxyError::Internal { reason } => {
            reason.contains("connection failed")
                || reason.contains("Connection refused")
                || reason.contains("connect")
                || reason.contains("dns error")
                || reason.contains("timed out")
                || reason.contains("unreachable")
        }
        _ => false,
    }
}
