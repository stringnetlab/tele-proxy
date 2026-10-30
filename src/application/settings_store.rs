use std::sync::{Arc, RwLock};

use crate::domain::errors::ProxyError;
use crate::domain::runtime_settings::{
    RuntimeSettings, RuntimeSettingsDocument, RuntimeSettingsPatch,
};
use crate::domain::services::SettingsRepository;

/// Intervalo del re-read periódico de `runtime_settings`. Es la red de seguridad del feed
/// `_changes`: si el feed muere o CouchDB se reinicia con `since=now`, sin este re-read los
/// ajustes se quedarían viejos para siempre. Un GET por minuto es despreciable frente al tráfico.
pub const SETTINGS_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Portador en memoria de los ajustes globales, con lectura O(1) y escritura atómica.
///
/// Se lee una vez por petición (no por byte ni por campo): el llamador toma un `Arc` de snapshot y
/// lo usa durante toda la petición, de modo que un cambio a mitad de camino no puede mezclar dos
/// configuraciones dentro del mismo request — p. ej. que el techo de lectura del cuerpo cambie
/// entre el `read_body_capped` del camino con caché y el del camino sin caché.
///
/// `RwLock<Arc<_>>` en vez de `arc-swap`: la lectura es un incremento atómico y la escritura es
/// rara (un PATCH de admin o el refresh por minuto), así que no merece una dependencia nueva.
pub struct SettingsStore {
    current: RwLock<Arc<RuntimeSettings>>,
    /// Valores del entorno con los que arrancó el proceso. Sirven de suelo cuando el documento
    /// desaparece de CouchDB: sin ellos, un borrado dejaría activos para siempre los últimos
    /// valores del documento borrado.
    baseline: Arc<RuntimeSettings>,
}

impl SettingsStore {
    pub fn new(initial: RuntimeSettings) -> Self {
        Self {
            current: RwLock::new(Arc::new(initial.clone())),
            baseline: Arc::new(initial),
        }
    }

    /// Snapshot para una petición o una ejecución de script. Barato: clona un `Arc`.
    pub fn snapshot(&self) -> Arc<RuntimeSettings> {
        // Un `RwLock` envenenado solo lo envenena un escritor que hizo panic, y aquí el escritor
        // únicamente asigna un `Arc` ya validado: el dato sigue siendo consistente.
        let guard = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(&guard)
    }

    /// Instala `next` si pasa la validación. Devuelve `true` solo si el valor cambió de verdad,
    /// para que el refresh periódico no llene el log con eventos idénticos.
    pub fn apply(&self, next: RuntimeSettings) -> Result<bool, ProxyError> {
        next.validate()?;

        let mut guard = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if **guard == next {
            return Ok(false);
        }

        tracing::info!(
            event = "runtime_settings_applied",
            upstream_timeout_secs = next.upstream_timeout_secs,
            max_response_bytes = next.max_response_bytes,
            cacheable_size_bytes = next.cacheable_size_bytes,
            stale_cache_ttl_secs = next.stale_cache_ttl_secs,
            lua_timeout_ms = next.lua_timeout_ms,
            lua_memory_limit_mb = next.lua_memory_limit_mb,
            webhook_timeout_ms = next.webhook_timeout_ms,
            verbose_errors = next.verbose_errors,
            "Ajustes globales del proxy actualizados"
        );

        *guard = Arc::new(next);
        Ok(true)
    }

    /// Vuelve a los valores del entorno. Es el comportamiento definido ante un `runtime_settings`
    /// borrado de CouchDB: sin esto, el último valor del documento seguiría activo para siempre.
    pub fn reset(&self) -> bool {
        match self.apply((*self.baseline).clone()) {
            Ok(changed) => changed,
            // Inalcanzable: el baseline se validó al construir el store.
            Err(e) => {
                tracing::error!(error = %e, "El baseline de runtime_settings es inválido");
                false
            }
        }
    }
}

/// Sincroniza el store con CouchDB. `Ok(None)` (documento borrado o primer arranque sin seed)
/// devuelve al baseline del entorno; un documento inválido conserva el último valor bueno.
pub async fn sync_from_repository(store: &SettingsStore, repo: &dyn SettingsRepository) {
    match repo.load_settings().await {
        Ok(Some(doc)) => {
            install_document(store, &doc);
        }
        Ok(None) => {
            store.reset();
        }
        Err(e) => {
            tracing::warn!(error = %e, "No se pudo releer runtime_settings desde CouchDB");
        }
    }
}

/// Instala un documento leído de CouchDB. Si llega inválido (edición manual, campo fuera de rango)
/// se conserva el último valor bueno y se registra: un documento roto no debe tumbar el tráfico.
/// Infallible a propósito — sus llamadores son el feed `_changes` y el re-read periódico, y ninguno
/// de los dos tiene a quién propagarle el error.
pub fn install_document(store: &SettingsStore, doc: &RuntimeSettingsDocument) -> bool {
    match doc.resolve().and_then(|settings| store.apply(settings)) {
        Ok(changed) => changed,
        Err(e) => {
            tracing::warn!(
                error = %e,
                doc_id = %doc.id,
                "runtime_settings inválido en CouchDB, se conserva el último valor bueno"
            );
            false
        }
    }
}

/// Caso de uso de la escritura admin: persiste el patch en CouchDB y, con el documento ya escrito,
/// lo instala en el store local para que el cambio sea visible sin esperar al feed `_changes` ni al
/// re-read periódico. El repositorio valida contra el estado leído de CouchDB (no contra el
/// snapshot del llamador), así que dos PATCH concurrentes no pueden colar un valor que ninguno
/// validó; el error de validación o el 409 definitivo se propagan tal cual.
pub async fn apply_settings_patch(
    store: &SettingsStore,
    repo: &dyn SettingsRepository,
    patch: &RuntimeSettingsPatch,
) -> Result<RuntimeSettings, ProxyError> {
    let doc = repo.patch_settings(patch).await?;
    install_document(store, &doc);
    Ok(store.snapshot().as_ref().clone())
}

/// Bucle de re-read periódico. Se lanza junto al listener de `_changes`; ambos convergen al mismo
/// estado, así que el orden no importa y un fallo de uno no deja los ajustes congelados.
pub async fn run_settings_refresh(
    store: Arc<SettingsStore>,
    repo: Arc<dyn SettingsRepository>,
    interval: std::time::Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        sync_from_repository(&store, repo.as_ref()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime_settings::{
        RuntimeSettingsPatch, RUNTIME_SETTINGS_DOC_ID, RUNTIME_SETTINGS_DOC_TYPE,
        RUNTIME_SETTINGS_SCHEMA_VERSION,
    };

    fn store_con_defaults() -> SettingsStore {
        SettingsStore::new(RuntimeSettings::defaults())
    }

    #[test]
    fn aplicar_un_cambio_lo_hace_visible_en_el_snapshot_siguiente() {
        let store = store_con_defaults();
        let cambiado = RuntimeSettings::defaults()
            .apply_patch(&RuntimeSettingsPatch {
                upstream_timeout_secs: Some(90),
                ..Default::default()
            })
            .unwrap();

        assert!(store.apply(cambiado).unwrap(), "el valor cambió");
        assert_eq!(store.snapshot().upstream_timeout_secs, 90);
    }

    #[test]
    fn reaplicar_el_mismo_valor_no_cuenta_como_cambio() {
        let store = store_con_defaults();
        let mismos = RuntimeSettings::defaults();
        assert!(!store.apply(mismos).unwrap());
    }

    #[test]
    fn un_valor_invalido_se_rechaza_y_conserva_el_anterior() {
        let store = store_con_defaults();
        let err = store
            .apply(RuntimeSettings {
                upstream_timeout_secs: 3_600,
                ..RuntimeSettings::defaults()
            })
            .expect_err("fuera de rango");

        assert!(matches!(err, ProxyError::InvalidConfig { .. }));
        assert_eq!(store.snapshot().upstream_timeout_secs, 30);
    }

    #[test]
    fn un_snapshot_tomado_no_se_muta_aunque_cambien_los_ajustes() {
        // Es la garantía que protege una petición en vuelo: toma el snapshot al entrar y nada de lo
        // que ocurra después puede alterar los valores que ya está usando.
        let store = Arc::new(store_con_defaults());
        let en_vuelo = store.snapshot();

        store
            .apply(RuntimeSettings {
                max_response_bytes: 8 << 20,
                cacheable_size_bytes: 4 << 20,
                ..RuntimeSettings::defaults()
            })
            .unwrap();

        assert_eq!(en_vuelo.max_response_bytes, 100 * 1024 * 1024);
        assert_eq!(store.snapshot().max_response_bytes, 8 << 20);
    }

    #[test]
    fn un_documento_invalido_deja_el_valor_bueno_intacto() {
        let store = store_con_defaults();
        let doc = doc_con(RuntimeSettingsPatch {
            lua_memory_limit_mb: Some(64_000),
            ..Default::default()
        });

        assert!(!install_document(&store, &doc));
        assert_eq!(store.snapshot().lua_memory_limit_mb, 50);
    }

    struct RepoStub {
        doc: Option<RuntimeSettingsDocument>,
    }

    #[async_trait::async_trait]
    impl SettingsRepository for RepoStub {
        async fn load_settings(&self) -> Result<Option<RuntimeSettingsDocument>, ProxyError> {
            Ok(self.doc.clone())
        }

        async fn patch_settings(
            &self,
            _patch: &RuntimeSettingsPatch,
        ) -> Result<RuntimeSettingsDocument, ProxyError> {
            unimplemented!("el stub solo cubre la lectura")
        }
    }

    fn doc_con(patch: RuntimeSettingsPatch) -> RuntimeSettingsDocument {
        RuntimeSettingsDocument {
            id: RUNTIME_SETTINGS_DOC_ID.to_string(),
            rev: Some("1-abc".to_string()),
            doc_type: RUNTIME_SETTINGS_DOC_TYPE.to_string(),
            schema_version: RUNTIME_SETTINGS_SCHEMA_VERSION,
            updated_at: "2026-10-30T00:00:00Z".to_string(),
            settings: patch,
        }
    }

    #[tokio::test]
    async fn sync_aplica_lo_que_hay_en_couchdb() {
        let store = store_con_defaults();
        let repo = RepoStub {
            doc: Some(doc_con(RuntimeSettingsPatch {
                lua_timeout_ms: Some(900),
                ..Default::default()
            })),
        };

        sync_from_repository(&store, &repo).await;
        assert_eq!(store.snapshot().lua_timeout_ms, 900);
    }

    #[tokio::test]
    async fn un_documento_borrado_devuelve_al_baseline_del_entorno() {
        let store = store_con_defaults();
        store
            .apply(RuntimeSettings {
                lua_timeout_ms: 900,
                ..RuntimeSettings::defaults()
            })
            .unwrap();

        sync_from_repository(&store, &RepoStub { doc: None }).await;

        // Sin esto, borrar el documento dejaría activos para siempre los últimos valores.
        assert_eq!(store.snapshot().lua_timeout_ms, 200);
    }

    /// Stub escribible: aplica el patch sobre `defaults()` igual que haría el repositorio real sobre
    /// un documento sembrado, y devuelve el documento resultante ya con `rev`.
    struct WritableRepoStub;

    #[async_trait::async_trait]
    impl SettingsRepository for WritableRepoStub {
        async fn load_settings(&self) -> Result<Option<RuntimeSettingsDocument>, ProxyError> {
            Ok(None)
        }

        async fn patch_settings(
            &self,
            patch: &RuntimeSettingsPatch,
        ) -> Result<RuntimeSettingsDocument, ProxyError> {
            let resolved = RuntimeSettings::defaults().apply_patch(patch)?;
            Ok(RuntimeSettingsDocument::new(
                &resolved,
                Some("1-abc".to_string()),
            ))
        }
    }

    #[tokio::test]
    async fn apply_patch_persiste_y_hace_visible_el_cambio_sin_esperar_al_feed() {
        let store = store_con_defaults();
        let resueltos = apply_settings_patch(
            &store,
            &WritableRepoStub,
            &RuntimeSettingsPatch {
                webhook_timeout_ms: Some(4_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(resueltos.webhook_timeout_ms, 4_000);
        assert_eq!(store.snapshot().webhook_timeout_ms, 4_000);
    }

    #[tokio::test]
    async fn apply_patch_invalido_propaga_el_error_y_no_toca_el_store() {
        let store = store_con_defaults();
        let err = apply_settings_patch(
            &store,
            &WritableRepoStub,
            &RuntimeSettingsPatch {
                lua_memory_limit_mb: Some(64_000),
                ..Default::default()
            },
        )
        .await
        .expect_err("fuera de rango");

        assert!(matches!(err, ProxyError::InvalidConfig { .. }));
        assert_eq!(store.snapshot().lua_memory_limit_mb, 50);
    }
}
