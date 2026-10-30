use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use crate::domain::runtime_settings::RUNTIME_SETTINGS_DOC_ID;
use crate::domain::services::{ConfigFetcher, SettingsRepository};

use super::settings_store::{sync_from_repository, SettingsStore};

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

/// Qué hay que hacer con un evento del feed.
#[derive(Debug, PartialEq)]
enum ChangeAction {
    /// Invalidar la caché de configs bajo este `internal_id`.
    InvalidateConfig(String),
    /// Releer `runtime_settings` desde CouchDB (o volver al baseline si se borró).
    ReloadSettings,
    Ignore,
}

/// Decisión pura sobre un evento del feed, separada del bucle para poder probarse sin red.
///
/// Los documentos borrados no traen cuerpo ni con `include_docs`, así que un cliente revocado se
/// invalida por `_id`: los docs de cliente se crean con `_id` igual a `internal_id` y, sin esa
/// rama, seguiría sirviendo `/aq/` hasta expirar el TTL de Moka. `runtime_settings` se reconoce
/// por su `_id` fijo, que es constante, así que el borrado también cae en la misma rama.
fn action_for(change: &ChangeEntry) -> ChangeAction {
    if change.id == RUNTIME_SETTINGS_DOC_ID {
        return ChangeAction::ReloadSettings;
    }

    if change.deleted {
        return ChangeAction::InvalidateConfig(change.id.clone());
    }

    let Some(doc) = change.doc.as_ref() else {
        return ChangeAction::Ignore;
    };
    if doc.r#type != "client_config" {
        return ChangeAction::Ignore;
    }

    ChangeAction::InvalidateConfig(if doc.internal_id.is_empty() {
        change.id.clone()
    } else {
        doc.internal_id.clone()
    })
}

pub struct ChangesFeedListener {
    couchdb_url: String,
    db_name: String,
    username: String,
    password: String,
    config_fetcher: Arc<dyn ConfigFetcher>,
    settings_store: Arc<SettingsStore>,
    settings_repo: Arc<dyn SettingsRepository>,
}

impl ChangesFeedListener {
    pub fn new(
        couchdb_url: String,
        db_name: String,
        username: String,
        password: String,
        config_fetcher: Arc<dyn ConfigFetcher>,
        settings_store: Arc<SettingsStore>,
        settings_repo: Arc<dyn SettingsRepository>,
    ) -> Self {
        Self {
            couchdb_url,
            db_name,
            username,
            password,
            config_fetcher,
            settings_store,
            settings_repo,
        }
    }

    pub async fn run(&self) {
        let client = match Client::builder().timeout(Duration::from_secs(30)).build() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "Feed de cambios: no se pudo construir el cliente HTTP, listener deshabilitado");
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
                        match action_for(change) {
                            ChangeAction::Ignore => {}

                            ChangeAction::ReloadSettings => {
                                if change.deleted {
                                    tracing::info!(
                                        doc_id = %change.id,
                                        "runtime_settings borrado vía feed _changes, volviendo a los valores del entorno"
                                    );
                                } else {
                                    tracing::info!(
                                        doc_id = %change.id,
                                        "Cambio de runtime_settings detectado vía feed _changes"
                                    );
                                }
                                sync_from_repository(
                                    &self.settings_store,
                                    self.settings_repo.as_ref(),
                                )
                                .await;
                            }

                            ChangeAction::InvalidateConfig(internal_id) => {
                                if change.deleted {
                                    tracing::info!(
                                        doc_id = %change.id,
                                        "Documento borrado vía feed _changes, invalidando caché"
                                    );
                                } else {
                                    tracing::info!(
                                        doc_id = %change.id,
                                        internal_id = %internal_id,
                                        config_version = change
                                            .doc
                                            .as_ref()
                                            .map(|doc| doc.config_version)
                                            .unwrap_or_default(),
                                        "Cambio de config detectado vía feed _changes"
                                    );
                                }

                                self.config_fetcher.invalidate_cache(&internal_id).await;
                            }
                        }
                    }

                    if !response.last_seq.is_empty() {
                        since = response.last_seq;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, backoff_secs = backoff.as_secs(), "Error del feed de cambios, esperando antes de reintentar");
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
            .map_err(|e| format!("Falló la petición _changes a CouchDB: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Error _changes de CouchDB {}: {}", status, body));
        }

        resp.json::<ChangesResponse>()
            .await
            .map_err(|e| format!("No se pudo parsear la respuesta _changes: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(id: &str, deleted: bool, doc: Option<(&str, &str, u64)>) -> ChangeEntry {
        ChangeEntry {
            id: id.to_string(),
            deleted,
            doc: doc.map(|(r#type, internal_id, config_version)| ChangeDoc {
                r#type: r#type.to_string(),
                internal_id: internal_id.to_string(),
                config_version,
            }),
        }
    }

    #[test]
    fn un_borrado_invalida_por_id_aunque_no_traiga_cuerpo() {
        // Es la rama que faltaba: sin ella, revocar un cliente desde otra instancia no se aplicaba
        // hasta que expiraba el TTL de la caché local.
        assert_eq!(
            action_for(&change("cliente-1", true, None)),
            ChangeAction::InvalidateConfig("cliente-1".to_string())
        );
    }

    #[test]
    fn una_modificacion_invalida_por_internal_id() {
        assert_eq!(
            action_for(&change(
                "cliente-1",
                false,
                Some(("client_config", "int-1", 4))
            )),
            ChangeAction::InvalidateConfig("int-1".to_string())
        );
    }

    #[test]
    fn sin_internal_id_en_el_doc_se_usa_el_id_del_camino() {
        assert_eq!(
            action_for(&change("cliente-1", false, Some(("client_config", "", 2)))),
            ChangeAction::InvalidateConfig("cliente-1".to_string())
        );
    }

    #[test]
    fn los_docs_que_no_son_de_cliente_no_invalidan_nada() {
        assert_eq!(
            action_for(&change(
                "admin_user:a@b.c",
                false,
                Some(("admin_user", "x", 1))
            )),
            ChangeAction::Ignore
        );
        assert_eq!(
            action_for(&change("cliente-1", false, None)),
            ChangeAction::Ignore
        );
    }

    #[test]
    fn el_documento_de_ajustes_recarga_settings_tambien_si_se_borra() {
        // Se reconoce por el `_id` fijo, no por el `type`: un borrado no trae cuerpo.
        assert_eq!(
            action_for(&change(RUNTIME_SETTINGS_DOC_ID, false, None)),
            ChangeAction::ReloadSettings
        );
        assert_eq!(
            action_for(&change(RUNTIME_SETTINGS_DOC_ID, true, None)),
            ChangeAction::ReloadSettings
        );
    }
}
