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
    AdminUser, ClientConfig, ClientConfigUpdate, ErrorHandlingConfig, ErrorMode, RateLimitConfig,
    ScriptingConfig,
};
use crate::domain::services::{AdminRepository, ConfigFetcher};

/// Map functions de las vistas del design doc. Fuente única de verdad: `ensure_design_doc` crea
/// el documento con estas vistas y **lo actualiza** cuando difieren (p. ej. tras añadir una
/// vista nueva en una release). Las dos primeras existen desde la primera versión: no cambiar
/// su clave ni su condición, son el lookup caliente del proxy.
const DESIGN_VIEWS: &[(&str, &str)] = &[
    (
        "by_crypt_id",
        r#"function(doc) { if (doc.type === 'client_config' && doc.crypt_id) { emit(doc.crypt_id, null); } }"#,
    ),
    (
        "by_token_hash",
        r#"function(doc) { if (doc.type === 'client_config' && doc.bearer_token_hash) { emit(doc.bearer_token_hash, null); } }"#,
    ),
    (
        "list_admins",
        r#"function(doc) { if (doc.type === 'admin_user' && doc.email) { emit(doc.email, null); } }"#,
    ),
    (
        "list_clients",
        r#"function(doc) { if (doc.type === 'client_config') { emit(null, null); } }"#,
    ),
];

fn view_def(name: &str) -> ViewDef {
    ViewDef {
        map: DESIGN_VIEWS
            .iter()
            .find(|(view_name, _)| *view_name == name)
            .map(|(_, map)| (*map).to_string())
            .unwrap_or_default(),
    }
}

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
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
    views: Views,
}

#[derive(Serialize, Deserialize)]
struct Views {
    by_crypt_id: ViewDef,
    by_token_hash: ViewDef,
    list_admins: ViewDef,
    list_clients: ViewDef,
}

#[derive(Serialize, Deserialize)]
struct ViewDef {
    map: String,
}

/// Documento existente tal como está en CouchDB: solo interesan el `_rev` (para el update) y
/// los `map` actuales (para decidir si hace falta reescribir).
#[derive(Deserialize)]
struct ExistingDesignDoc {
    #[serde(rename = "_rev")]
    rev: Option<String>,
    views: HashMap<String, ViewDef>,
}

impl DesignDoc {
    fn with_rev(rev: Option<String>) -> Self {
        Self {
            rev,
            views: Views {
                by_crypt_id: view_def("by_crypt_id"),
                by_token_hash: view_def("by_token_hash"),
                list_admins: view_def("list_admins"),
                list_clients: view_def("list_clients"),
            },
        }
    }

    fn is_satisfied_by(existing: &ExistingDesignDoc) -> bool {
        DESIGN_VIEWS.iter().all(|(name, map)| {
            existing
                .views
                .get(*name)
                .is_some_and(|view_def| view_def.map == *map)
        })
    }
}

#[derive(Deserialize)]
struct ViewRow {
    id: String,
}

#[derive(Deserialize)]
struct ViewResponse {
    #[serde(default)]
    total_rows: Option<u64>,
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

    /// Crea el design doc si falta y **lo actualiza** cuando los `map` difieren de
    /// `DESIGN_VIEWS` (p. ej. esta release añadió `list_admins`/`list_clients`). El update
    /// preserva el `_rev` del documento existente; un 409 (rev obsoleta) se resuelve
    /// releyendo el `_rev` una vez, no con un bucle.
    pub async fn ensure_design_doc(&self) -> Result<(), ProxyError> {
        let design_doc_id = "_design/proxy_lookup";
        let url = format!("{}/{}/{}", self.base_url, self.db_name, design_doc_id);

        let fetch_existing = |client: &Client| {
            client
                .get(&url)
                .basic_auth(&self.username, Some(&self.password))
                .send()
        };

        let existing: Option<ExistingDesignDoc> = {
            let resp = fetch_existing(&self.client).await.map_err(|e| {
                ProxyError::Internal {
                    reason: format!("Falló la conexión a CouchDB: {}", e),
                }
            })?;
            if resp.status().is_success() {
                let body = resp.bytes().await.map_err(|e| ProxyError::Internal {
                    reason: format!("No se pudo leer el design doc existente: {}", e),
                })?;
                Some(serde_json::from_slice(&body).map_err(|e| ProxyError::Internal {
                    reason: format!("El design doc existente no tiene la forma esperada: {}", e),
                })?)
            } else {
                None
            }
        };

        if let Some(ref doc) = existing {
            if DesignDoc::is_satisfied_by(doc) {
                return Ok(());
            }
            tracing::info!("Design doc de CouchDB desactualizado, actualizando vistas");
        }

        // Intento inicial con el `_rev` conocido; si el documento no existía, sin `_rev`.
        let mut rev = existing.and_then(|doc| doc.rev);
        for attempt in 0..2 {
            let payload = DesignDoc::with_rev(rev.clone());
            let resp = self
                .client
                .put(&url)
                .basic_auth(&self.username, Some(&self.password))
                .json(&payload)
                .send()
                .await
                .map_err(|e| ProxyError::Internal {
                    reason: format!("No se pudo escribir el design doc: {}", e),
                })?;

            if resp.status().is_success() {
                tracing::info!("Design doc de CouchDB creado/actualizado");
                return Ok(());
            }

            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();

            // 409 = el `_rev` usado es obsoleto (otro proceso lo actualizó antes): se relee el
            // documento una única vez y se reintenta. Cualquier otro error es terminal.
            if status == 409 && attempt == 0 {
                tracing::warn!(error = %body, "Conflicto 409 escribiendo el design doc, releyendo _rev");
                let retry = fetch_existing(&self.client).await.map_err(|e| {
                    ProxyError::Internal {
                        reason: format!("Falló la reconexión a CouchDB tras el 409: {}", e),
                    }
                })?;
                if retry.status().is_success() {
                    let body = retry.bytes().await.map_err(|e| ProxyError::Internal {
                        reason: format!("No se pudo releer el design doc tras el 409: {}", e),
                    })?;
                    let doc: ExistingDesignDoc =
                        serde_json::from_slice(&body).map_err(|e| ProxyError::Internal {
                            reason: format!(
                                "El design doc releído tras el 409 no parsea: {}",
                                e
                            ),
                        })?;
                    if DesignDoc::is_satisfied_by(&doc) {
                        return Ok(());
                    }
                    rev = doc.rev;
                    continue;
                }
            }

            return Err(ProxyError::Internal {
                reason: format!("No se pudo escribir el design doc: {}", body),
            });
        }

        Err(ProxyError::Internal {
            reason: "No se pudo escribir el design doc tras reintentar el _rev".to_string(),
        })
    }

    /// Si la DB no tiene documentos de configuración (type === "client_config") y el proceso
    /// arranca en `MODO=desarrollo`, crea un cliente demo con permisos mínimos para pruebas y
    /// demos. El bearer token es `demo-token` (hash SHA-256 calculado en runtime). En cualquier
    /// otro modo **no siembra nada**: un cliente con token conocido en producción sería una
    /// puerta trasera. El log solo menciona el `crypt_id` (público por diseño): el token en
    /// claro jamás se registra.
    pub async fn seed_demo_client_if_empty(&self, modo_desarrollo: bool) -> Result<(), ProxyError> {
        if !modo_desarrollo {
            tracing::debug!("Modo producción: el cliente demo no se siembra");
            return Ok(());
        }

        let url = self.view_url("by_crypt_id");
        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("limit", "1")])
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló la comprobación de la vista de CouchDB: {}", e),
            })?;

        if !resp.status().is_success() {
            return Ok(());
        }

        let view_resp: ViewResponse = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de la vista: {}", e),
        })?;

        if !view_resp.rows.is_empty() {
            return Ok(());
        }

        let demo_token = "demo-token";
        let mut hasher = Sha256::new();
        hasher.update(demo_token.as_bytes());
        let result = hasher.finalize();
        let hex_str = result
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        let token_hash = format!("sha256:{}", hex_str);

        let demo_config = ClientConfig {
            id: "demo_client".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "demo_client".to_string(),
            crypt_id: nanoid::nanoid!(12),
            bearer_token_hash: token_hash,
            config_version: 1,
            kind: crate::domain::models::ClientKind::Client,
            wildcard: false,
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
                expression: String::new(),
            },
            error_handling: ErrorHandlingConfig {
                mode: ErrorMode::Transparent,
                fallback_urls: HashMap::new(),
            },
            header_rules: Vec::new(),
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
                reason: format!("No se pudo crear el cliente demo: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("No se pudo crear el cliente demo: {}", body),
            });
        }

        tracing::info!(
            crypt_id = %demo_config.crypt_id,
            "Cliente demo sembrado (whitelist: example.com, 5 req/60s); el bearer token no se registra"
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
        self.fetch_raw_doc(doc_id).await
    }

    /// GET de un documento CouchDB deserializado a `T`. El 404 se normaliza a
    /// `ConfigNotFound` con el `doc_id` pedido (el `internal_id` de la pista de auditoría).
    async fn fetch_raw_doc<T: serde::de::DeserializeOwned>(
        &self,
        doc_id: &str,
    ) -> Result<T, ProxyError> {
        let url = self.doc_url(doc_id);

        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló la petición a CouchDB: {}", e),
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
                    reason: format!("Error de CouchDB {}: {}", status, body),
                });
            }
        }

        let doc: T = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de CouchDB: {}", e),
        })?;

        Ok(doc)
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
                reason: format!("Falló la consulta de la vista de CouchDB: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de la vista de CouchDB: {}", body),
            });
        }

        let view_resp: ViewResponse = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de la vista: {}", e),
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

    /// PUT de un documento CouchDB y devolución del `_rev` asignado. Un 409 se reporta como
    /// error de conflicto de revisión (el llamador tiene un documento obsoleto y debe releer).
    async fn write_doc<T: Serialize>(&self, doc_id: &str, doc: &T) -> Result<String, ProxyError> {
        let url = self.doc_url(doc_id);
        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(doc)
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló la escritura en CouchDB ({}): {}", doc_id, e),
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let reason = if status == 409 {
                format!("Conflicto de _rev en '{}': el documento cambió en CouchDB", doc_id)
            } else {
                format!("Error de CouchDB {} escribiendo '{}': {}", status, doc_id, body)
            };
            return Err(ProxyError::Internal { reason });
        }

        #[derive(Deserialize)]
        struct WriteResp {
            rev: String,
        }

        let write_resp: WriteResp = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de escritura: {}", e),
        })?;
        Ok(write_resp.rev)
    }

    /// Invalida la caché local bajo los identificadores anteriores de un cliente (p. ej. tras
    /// rotar token o crypt_id, o al borrarlo): el id_index recuerda cuáles eran.
    async fn invalidate_by_internal_id(&self, internal_id: &str) {
        let mut index = self.id_index.write().await;
        if let Some((crypt_id, token_hash)) = index.remove(internal_id) {
            self.local_cache.invalidate(&crypt_id).await;
            self.local_cache.invalidate(&token_hash).await;
        }
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
                        "CouchDB no disponible, devolviendo config por defecto degradada"
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
                        "CouchDB no disponible, devolviendo config por defecto degradada"
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
            if let Some(ref expression) = scripting.expression {
                config.scripting.expression = expression.clone();
            }
        }
        if let Some(ref error_handling) = update.error_handling {
            config.error_handling = error_handling.clone();
        }
        if let Some(ref header_rules) = update.header_rules {
            config.header_rules = header_rules.clone();
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
                reason: format!("Falló la actualización en CouchDB: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de actualización de CouchDB: {}", body),
            });
        }

        #[derive(Deserialize)]
        struct UpdateResp {
            rev: String,
        }

        let update_resp: UpdateResp = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de actualización: {}", e),
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
                reason: format!("Falló la rotación en CouchDB: {}", e),
            })?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de rotación de CouchDB: {}", body),
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
            "crypt_id rotado"
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
                "Caché invalidada vía feed _changes"
            );
        }
    }
}

#[async_trait]
impl AdminRepository for CouchDbRepository {
    async fn get_admin(&self, email: &str) -> Result<AdminUser, ProxyError> {
        let admin: AdminUser = self.fetch_raw_doc(&AdminUser::doc_id(email)).await?;
        Ok(admin)
    }

    async fn list_admins(&self) -> Result<Vec<AdminUser>, ProxyError> {
        let url = self.view_url("list_admins");
        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló la consulta de la vista list_admins: {}", e),
            })?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de la vista list_admins: {}", body),
            });
        }
        let view_resp: ViewResponse = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de list_admins: {}", e),
        })?;

        let mut admins = Vec::with_capacity(view_resp.rows.len());
        for row in view_resp.rows {
            admins.push(self.fetch_raw_doc(&row.id).await?);
        }
        Ok(admins)
    }

    async fn put_admin(&self, admin: &AdminUser) -> Result<AdminUser, ProxyError> {
        let mut saved = admin.clone();
        saved.email = saved.email.to_lowercase();
        saved.id = AdminUser::doc_id(&saved.email);
        saved.r#type = "admin_user".to_string();
        let rev = self.write_doc(&saved.id, &saved).await?;
        saved.rev = Some(rev);
        Ok(saved)
    }

    async fn delete_admin(&self, email: &str) -> Result<(), ProxyError> {
        let admin = self.get_admin(email).await?;
        let url = self.doc_url(&admin.id);
        let resp = self
            .client
            .delete(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("rev", admin.rev.unwrap_or_default())])
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló el borrado del admin: {}", e),
            })?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de CouchDB borrando el admin: {}", body),
            });
        }
        Ok(())
    }

    async fn list_clients(
        &self,
        limit: u64,
        skip: u64,
    ) -> Result<(Vec<ClientConfig>, u64), ProxyError> {
        let url = self.view_url("list_clients");
        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[
                ("limit", limit.to_string()),
                ("skip", skip.to_string()),
            ])
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló la consulta de la vista list_clients: {}", e),
            })?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de la vista list_clients: {}", body),
            });
        }
        let view_resp: ViewResponse = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de list_clients: {}", e),
        })?;
        let total = view_resp.total_rows.unwrap_or(view_resp.rows.len() as u64);

        let mut clients = Vec::with_capacity(view_resp.rows.len());
        for row in view_resp.rows {
            clients.push(self.fetch_doc_by_id(&row.id).await?);
        }
        Ok((clients, total))
    }

    async fn get_client_by_internal_id(
        &self,
        internal_id: &str,
    ) -> Result<ClientConfig, ProxyError> {
        self.fetch_doc_by_id(internal_id).await
    }

    async fn put_client(&self, config: &ClientConfig) -> Result<ClientConfig, ProxyError> {
        // Invalida primero bajo los identificadores anteriores: un token o crypt_id rotado debe
        // dejar de resolver en caliente, no solo tras expirar el TTL de Moka.
        self.invalidate_by_internal_id(&config.internal_id).await;
        let mut saved = config.clone();
        let rev = self.write_doc(&config.id, &saved).await?;
        saved.rev = Some(rev);
        self.cache_config(&saved).await;
        Ok(saved)
    }

    async fn delete_client(&self, internal_id: &str) -> Result<(), ProxyError> {
        let config = self.fetch_doc_by_id(internal_id).await?;
        let url = self.doc_url(&config.id);
        let resp = self
            .client
            .delete(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("rev", config.rev.clone().unwrap_or_default())])
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló el borrado del cliente: {}", e),
            })?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de CouchDB borrando el cliente: {}", body),
            });
        }
        self.invalidate_by_internal_id(internal_id).await;
        Ok(())
    }

    async fn client_count(&self) -> Result<u64, ProxyError> {
        let url = self.view_url("list_clients");
        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .query(&[("limit", "0")])
            .send()
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("Falló la consulta de client_count: {}", e),
            })?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProxyError::Internal {
                reason: format!("Error de la vista list_clients: {}", body),
            });
        }
        let view_resp: ViewResponse = resp.json().await.map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo parsear la respuesta de client_count: {}", e),
        })?;
        Ok(view_resp.total_rows.unwrap_or(0))
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
        .map_err(|e| format!("Falló la conexión a CouchDB: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("CouchDB devolvió status {}", resp.status()));
    }

    Ok(())
}

fn is_connection_error(error: &ProxyError) -> bool {
    match error {
        ProxyError::Internal { reason } => {
            reason.contains("Falló la conexión")
                || reason.contains("Connection refused")
                || reason.contains("connect")
                || reason.contains("dns error")
                || reason.contains("timed out")
                || reason.contains("unreachable")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn seed_demo_no_se_ejecuta_fuera_de_desarrollo() {
        // URL de un CouchDB inexistente: si la función tocara la red, fallaría. En modo
        // producción tiene que devolver Ok sin salir de casa (hallazgo C1).
        let repo = CouchDbRepository::new(
            "http://127.0.0.1:1".to_string(),
            "test_db".to_string(),
            "user".to_string(),
            "pass".to_string(),
            60,
            100,
        );
        assert!(repo.seed_demo_client_if_empty(false).await.is_ok());
    }
}
