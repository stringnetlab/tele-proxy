use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use crate::application::cache_key::sha256_hex;
use crate::application::identity::{IdentityProviders, VerifiedIdentity};
use crate::domain::errors::ProxyError;
use crate::domain::models::{
    AdminUser, AdminUserResponse, ClientConfig, ClientConfigUpdate, ClientKind,
};
use crate::domain::services::AdminRepository;
use crate::domain::validators::validate_admin_email;

/// Prefijos de las claves Valkey. Los valores guardados son emails (no secretos), pero la clave
/// lleva igualmente el hash del identificador: la cookie `teleproxy_admin` solo viaja hasheada
/// en el almacén.
const SESSION_KEY_PREFIX: &str = "admin_session";
const OAUTH_STATE_KEY_PREFIX: &str = "admin_oauth_state";
/// Caché del flag `active` de un admin (60 s): `resolve_session` no martillea CouchDB en cada
/// petición autenticada del panel.
const ADMIN_ACTIVE_CACHE_TTL: Duration = Duration::from_secs(60);
/// Techo de la paginación de `list_clients`.
const MAX_CLIENTS_PAGE_SIZE: u64 = 200;

/// Variables de entorno de administración, parseadas y validadas en el arranque
/// (`parse_in_range` en `main.rs`). El master token se guarda **hasheado** (`sha256:<hex>`):
/// el valor en claro vive solo en el entorno del despliegue.
pub struct AdminSettings {
    /// Hash `sha256:<hex>` del `MASTER_BEARER_TOKEN`; `None` = master deshabilitado.
    pub master_token_hash: Option<String>,
    pub allowed_domains: Vec<String>,
    pub session_ttl_seconds: u64,
    pub oauth_state_ttl_seconds: u64,
    pub login_rate_limit_requests: u32,
    pub login_rate_limit_window_seconds: u64,
    pub admin_rate_limit_requests: u32,
    pub admin_rate_limit_window_seconds: u64,
    pub control_rate_limit_requests: u32,
    pub control_rate_limit_window_seconds: u64,
    /// `Secure` en la cookie de sesión: solo cuando `GOOGLE_REDIRECT_URI` es https.
    pub cookie_secure: bool,
}

/// Sesiones y estados OAuth efímeros sobre Valkey. Trait propio (no `CacheStore`) porque el
/// contrato es de claves planas con TTL, no de respuestas cacheadas; los tests mockean este
/// trait con un HashMap en memoria.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn setex(&self, key: &str, value: &str, ttl_seconds: u64) -> Result<(), ProxyError>;
    async fn get(&self, key: &str) -> Result<Option<String>, ProxyError>;
    /// GET atómico + borrado: consumo de un solo uso para el state OAuth.
    async fn getdel(&self, key: &str) -> Result<Option<String>, ProxyError>;
    async fn del(&self, key: &str) -> Result<(), ProxyError>;
}

/// Sesión de administrador recién creada (login). `session_id` es el valor de la cookie
/// `teleproxy_admin`; se devuelve **en claro una sola vez** al navegador y solo se guarda
/// hasheado en Valkey.
#[derive(Debug)]
pub struct AdminSession {
    pub email: String,
    pub session_id: String,
}

/// Quién autoriza una mutación o un login, para la pista de auditoría.
fn audit(actor: &str, action: &str, target: &str) {
    tracing::info!(
        event = "admin_audit",
        actor = %actor,
        action = %action,
        target = %target,
        "auditoría de administración"
    );
}

fn keyed_hash(value: &str) -> String {
    format!("sha256:{}", sha256_hex(value))
}

fn session_key(session_id: &str) -> String {
    format!("{}:{}", SESSION_KEY_PREFIX, keyed_hash(session_id))
}

fn oauth_state_key(state: &str) -> String {
    format!("{}:{}", OAUTH_STATE_KEY_PREFIX, keyed_hash(state))
}

/// Payload del `PUT` de la **API admin** (`/api/v1/admin/clients/{id}`): el `ClientConfigUpdate`
/// de la API de cliente **más** los campos solo-admin. `wildcard` no existe en
/// `ClientConfigUpdate` a propósito: un cliente no puede activar su propia salida libre — el
/// campo ni siquiera llega a deserializar en su `PUT`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AdminClientUpdate {
    #[serde(flatten)]
    pub config: ClientConfigUpdate,
    pub wildcard: Option<bool>,
}

/// Casos de uso de la administración global: login Google con dominios permitidos, sesiones,
/// verificación del master token y CRUD de admins y clientes con auditoría.
pub struct AdminService {    repo: Arc<dyn AdminRepository>,
    sessions: Arc<dyn SessionStore>,
    providers: IdentityProviders,
    settings: AdminSettings,
    admin_active_cache: Cache<String, bool>,
}

impl AdminService {
    pub fn new(
        repo: Arc<dyn AdminRepository>,
        sessions: Arc<dyn SessionStore>,
        providers: IdentityProviders,
        settings: AdminSettings,
    ) -> Self {
        Self {
            repo,
            sessions,
            providers,
            settings,
            admin_active_cache: Cache::builder()
                .time_to_live(ADMIN_ACTIVE_CACHE_TTL)
                .build(),
        }
    }

    pub fn settings(&self) -> &AdminSettings {
        &self.settings
    }

    /// El master token **solo** autoriza rutas `/api/v1/admin/*`: aquí se verifica, y las rutas
    /// de cliente (`/api/v1/clients/*`) ni miran esta función. Master ausente → siempre falso.
    pub fn verify_master_token(&self, presented: &str) -> bool {
        self.settings
            .master_token_hash
            .as_ref()
            .is_some_and(|expected| keyed_hash(presented) == *expected)
    }

    /// URL de inicio de login Google con un state fresco (de un solo uso) ya persistido.
    pub async fn google_login_url(&self) -> Result<String, ProxyError> {
        let provider = self.providers.get("google").ok_or_else(|| {
            ProxyError::ServiceUnavailable {
                reason: "login con Google no configurado".to_string(),
            }
        })?;
        let state = nanoid::nanoid!(24);
        self.sessions
            .setex(&oauth_state_key(&state), "1", self.settings.oauth_state_ttl_seconds)
            .await?;
        Ok(provider.authorization_url(&state))
    }

    /// Callback OAuth: consume el state (expirado/inexistente → 400), canjea el code, y exige
    /// email verificado + dominio permitido + `admin_user` activo. Crea la sesión y audita.
    pub async fn complete_google_login(
        &self,
        code: &str,
        state: &str,
    ) -> Result<AdminSession, ProxyError> {
        let stored = self.sessions.getdel(&oauth_state_key(state)).await?;
        if stored.is_none() {
            return Err(ProxyError::InvalidConfig {
                field: "state".to_string(),
                reason: "expired or unknown OAuth state".to_string(),
            });
        }

        let provider = self.providers.get("google").ok_or_else(|| {
            ProxyError::ServiceUnavailable {
                reason: "login con Google no configurado".to_string(),
            }
        })?;
        let identity = provider.exchange_code(code).await?;
        self.login_verified_identity("google", identity).await
    }

    /// Camino común tras verificar una identidad (hoy solo Google; con Zentiel sería el mismo):
    /// dominio permitido, admin existente y activo, sesión nueva.
    async fn login_verified_identity(
        &self,
        provider_name: &str,
        identity: VerifiedIdentity,
    ) -> Result<AdminSession, ProxyError> {
        if !identity.email_verified {
            return Err(ProxyError::Forbidden {
                reason: "el proveedor de identidad no verificó el email".to_string(),
            });
        }
        let domain = identity
            .email
            .rsplit_once('@')
            .map(|(_, domain)| domain.to_string())
            .unwrap_or_default();
        if !self
            .settings
            .allowed_domains
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&domain))
        {
            return Err(ProxyError::Forbidden {
                reason: format!("dominio '{domain}' no permitido para administradores"),
            });
        }

        let admin = match self.repo.get_admin(&identity.email).await {
            Ok(admin) => admin,
            Err(ProxyError::ConfigNotFound { .. }) => {
                return Err(ProxyError::Forbidden {
                    reason: "el email no es un administrador registrado".to_string(),
                })
            }
            Err(e) => return Err(e),
        };
        if !admin.active {
            return Err(ProxyError::Forbidden {
                reason: "administrador desactivado".to_string(),
            });
        }

        let session = self.create_session(&admin.email).await?;
        audit(&admin.email, &format!("login_{provider_name}"), &admin.email);
        Ok(session)
    }

    async fn create_session(&self, email: &str) -> Result<AdminSession, ProxyError> {
        let session_id = nanoid::nanoid!(32);
        self.sessions
            .setex(
                &session_key(&session_id),
                email,
                self.settings.session_ttl_seconds,
            )
            .await?;
        Ok(AdminSession {
            email: email.to_string(),
            session_id,
        })
    }

    /// Resuelve la cookie de sesión a un email, validando que el admin siga activo (con caché
    /// Moka de 60 s). `None` = sin sesión válida.
    pub async fn resolve_session(&self, session_id: &str) -> Option<String> {
        let email = self.sessions.get(&session_key(session_id)).await.ok()??;

        if let Some(active) = self.admin_active_cache.get(&email).await {
            return active.then_some(email);
        }
        let active = matches!(
            self.repo.get_admin(&email).await,
            Ok(admin) if admin.active
        );
        self.admin_active_cache.insert(email.clone(), active).await;
        active.then_some(email)
    }

    /// Logout: borra la sesión y devuelve el email que la portaba (para auditar).
    pub async fn destroy_session(&self, session_id: &str) -> Result<Option<String>, ProxyError> {
        let email = self.sessions.getdel(&session_key(session_id)).await?;
        if let Some(ref actor) = email {
            audit(actor, "logout", actor);
        }
        Ok(email)
    }

    /// Un admin recién desactivado/borrado no debe seguir teniendo sesiones válidas en caliente.
    pub async fn invalidate_admin_active_cache(&self, email: &str) {
        self.admin_active_cache.invalidate(email).await;
    }

    // --- Administradores ---

    pub async fn list_admins(&self) -> Result<Vec<AdminUserResponse>, ProxyError> {
        let admins = self.repo.list_admins().await?;
        Ok(admins.into_iter().map(AdminUserResponse::from).collect())
    }

    pub async fn create_admin(
        &self,
        email: &str,
        actor: &str,
    ) -> Result<AdminUserResponse, ProxyError> {
        validate_admin_email(email, &self.settings.allowed_domains)?;
        let email = email.trim().to_lowercase();
        if self.repo.get_admin(&email).await.is_ok() {
            return Err(ProxyError::InvalidConfig {
                field: "email".to_string(),
                reason: format!("'{email}' is already an admin"),
            });
        }
        let now = chrono::Utc::now().to_rfc3339();
        let admin = AdminUser {
            id: AdminUser::doc_id(&email),
            rev: None,
            r#type: "admin_user".to_string(),
            email: email.clone(),
            role: "admin".to_string(),
            active: true,
            created_at: now.clone(),
            updated_at: now,
            created_by: actor.to_string(),
        };
        let saved = self.repo.put_admin(&admin).await?;
        audit(actor, "admin_create", &email);
        Ok(AdminUserResponse::from(saved))
    }

    pub async fn set_admin_active(
        &self,
        email: &str,
        active: bool,
        actor: &str,
    ) -> Result<AdminUserResponse, ProxyError> {
        let email = email.trim().to_lowercase();
        if email == actor {
            return Err(ProxyError::Forbidden {
                reason: "no puede desactivarse a sí mismo".to_string(),
            });
        }
        let mut admin = self.repo.get_admin(&email).await?;
        admin.active = active;
        admin.updated_at = chrono::Utc::now().to_rfc3339();
        let saved = self.repo.put_admin(&admin).await?;
        self.invalidate_admin_active_cache(&email).await;
        audit(
            actor,
            if active { "admin_activate" } else { "admin_deactivate" },
            &email,
        );
        Ok(AdminUserResponse::from(saved))
    }

    pub async fn delete_admin(&self, email: &str, actor: &str) -> Result<(), ProxyError> {
        let email = email.trim().to_lowercase();
        if email == actor {
            return Err(ProxyError::Forbidden {
                reason: "no puede eliminarse a sí mismo".to_string(),
            });
        }
        let admin = self.repo.get_admin(&email).await?;
        if admin.active {
            // Evitar lockout: el último admin activo solo puede caer por decisión futura del
            // master (que siempre conserva acceso de emergencia mientras esté configurado).
            let active_count = self
                .repo
                .list_admins()
                .await?
                .iter()
                .filter(|admin| admin.active)
                .count();
            if active_count <= 1 {
                return Err(ProxyError::Forbidden {
                    reason: "no se puede eliminar el último administrador activo".to_string(),
                });
            }
        }
        self.repo.delete_admin(&email).await?;
        self.invalidate_admin_active_cache(&email).await;
        audit(actor, "admin_delete", &email);
        Ok(())
    }

    // --- Clientes ---

    pub async fn list_clients(
        &self,
        limit: u64,
        skip: u64,
    ) -> Result<(Vec<ClientConfig>, u64), ProxyError> {
        let limit = limit.clamp(1, MAX_CLIENTS_PAGE_SIZE);
        self.repo.list_clients(limit, skip).await
    }

    /// Crea un cliente nuevo. `wildcard: Some(true)` lo marca como salida libre (solo-admin).
    /// Devuelve el config guardado y el bearer token **en claro**, que solo existe en la
    /// respuesta de esta llamada (en CouchDB viaja hasheado).
    pub async fn create_client(
        &self,
        actor: &str,
        wildcard: Option<bool>,
    ) -> Result<(ClientConfig, String), ProxyError> {
        let token = nanoid::nanoid!(32);
        let internal_id = nanoid::nanoid!();
        let now = chrono::Utc::now().to_rfc3339();
        let config = ClientConfig {
            id: internal_id.clone(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id,
            crypt_id: nanoid::nanoid!(12),
            bearer_token_hash: keyed_hash(&token),
            config_version: 1,
            kind: ClientKind::Client,
            wildcard: wildcard.unwrap_or(false),
            whitelist: Vec::new(),
            rate_limit: crate::domain::models::RateLimitConfig {
                max_requests: 50,
                window_seconds: 60,
            },
            max_scripting_body_bytes: 5_242_880,
            scripting: crate::domain::models::ScriptingConfig {
                enabled: false,
                code: String::new(),
                code_hash: String::new(),
                expression: String::new(),
            },
            error_handling: crate::domain::models::ErrorHandlingConfig {
                mode: crate::domain::models::ErrorMode::Transparent,
                fallback_urls: std::collections::HashMap::new(),
            },
            header_rules: Vec::new(),
        };
        let saved = self.repo.put_client(&config).await?;
        tracing::info!(
            event = "admin_audit",
            actor = %actor,
            action = "client_create",
            target = %saved.internal_id,
            wildcard = saved.wildcard,
            created_at = %now,
            "auditoría de administración"
        );
        Ok((saved, token))
    }

    pub async fn get_client(&self, internal_id: &str) -> Result<ClientConfig, ProxyError> {
        let config = self.repo.get_client_by_internal_id(internal_id).await?;
        Self::ensure_client_kind(&config)?;
        Ok(config)
    }

    /// Aplica el `PUT` de la API admin: el `ClientConfigUpdate` validado (mismas cotas que el
    /// `PUT` de la API de cliente) **más** los campos solo-admin (`wildcard`), e incrementa
    /// `config_version`, igual que `ConfigFetcher::update_config`.
    pub async fn update_client(
        &self,
        internal_id: &str,
        update: &AdminClientUpdate,
        actor: &str,
    ) -> Result<ClientConfig, ProxyError> {
        let mut config = self.get_client(internal_id).await?;
        let wildcard_antes = config.wildcard;

        let ClientConfigUpdate {
            whitelist,
            rate_limit,
            max_scripting_body_bytes,
            scripting,
            error_handling,
            header_rules,
        } = &update.config;

        if let Some(whitelist) = whitelist {
            config.whitelist = whitelist.clone();
        }
        if let Some(rate_limit) = rate_limit {
            config.rate_limit = rate_limit.clone();
        }
        if let Some(max_bytes) = max_scripting_body_bytes {
            config.max_scripting_body_bytes = *max_bytes;
        }
        if let Some(scripting) = scripting {
            if let Some(enabled) = scripting.enabled {
                config.scripting.enabled = enabled;
            }
            if let Some(code) = &scripting.code {
                config.scripting.code = code.clone();
            }
            if let Some(code_hash) = &scripting.code_hash {
                config.scripting.code_hash = code_hash.clone();
            }
            if let Some(expression) = &scripting.expression {
                config.scripting.expression = expression.clone();
            }
        }
        if let Some(error_handling) = error_handling {
            config.error_handling = error_handling.clone();
        }
        if let Some(header_rules) = header_rules {
            config.header_rules = header_rules.clone();
        }
        if let Some(wildcard) = update.wildcard {
            config.wildcard = wildcard;
        }
        config.config_version += 1;

        let saved = self.repo.put_client(&config).await?;
        audit(actor, "client_update", internal_id);
        // El toggle de wildcard es sensible (habilita salida libre): deja rastro explícito,
        // no solo el "client_update" genérico.
        if saved.wildcard != wildcard_antes {
            tracing::info!(
                event = "admin_audit",
                actor = %actor,
                action = "client_wildcard_toggle",
                target = %internal_id,
                wildcard = saved.wildcard,
                "auditoría de administración"
            );
        }
        Ok(saved)
    }

    /// Rota el bearer token: el nuevo se devuelve **en claro una sola vez**; el hash anterior
    /// queda invalidado en la caché local por `put_client`.
    pub async fn rotate_client_token(
        &self,
        internal_id: &str,
        actor: &str,
    ) -> Result<(ClientConfig, String), ProxyError> {
        let mut config = self.get_client(internal_id).await?;
        let token = nanoid::nanoid!(32);
        config.bearer_token_hash = keyed_hash(&token);
        config.config_version += 1;
        let saved = self.repo.put_client(&config).await?;
        audit(actor, "client_rotate_token", internal_id);
        Ok((saved, token))
    }

    pub async fn rotate_client_id(
        &self,
        internal_id: &str,
        actor: &str,
    ) -> Result<String, ProxyError> {
        let mut config = self.get_client(internal_id).await?;
        let old_crypt_id = config.crypt_id.clone();
        let new_crypt_id = nanoid::nanoid!(12);
        config.crypt_id = new_crypt_id.clone();
        config.config_version += 1;
        self.repo.put_client(&config).await?;
        audit(actor, "client_rotate_id", internal_id);
        tracing::info!(
            event = "crypt_id_rotated",
            internal_id = %internal_id,
            old_crypt_id = %old_crypt_id,
            new_crypt_id = %new_crypt_id,
            "crypt_id rotado"
        );
        Ok(new_crypt_id)
    }

    pub async fn delete_client(&self, internal_id: &str, actor: &str) -> Result<(), ProxyError> {
        self.get_client(internal_id).await?;
        self.repo.delete_client(internal_id).await?;
        audit(actor, "client_delete", internal_id);
        Ok(())
    }

    /// Los documentos `kind == Admin` no son clientes: la API admin no los toca y las rutas de
    /// cliente y proxy los rechazan (defensa en profundidad, separación admin/cliente).
    pub fn ensure_client_kind(config: &ClientConfig) -> Result<(), ProxyError> {
        if config.kind == ClientKind::Admin {
            return Err(ProxyError::Forbidden {
                reason: "un administrador no puede usar el proxy ni la API de cliente"
                    .to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::application::identity::{IdentityProvider, VerifiedIdentity};
    use crate::domain::models::{ClientKind, RateLimitConfig, ScriptingConfig};

    // --- Fakes manuales (el proyecto no usa mockall) ---

    #[derive(Default)]
    struct FakeRepo {
        admins: Mutex<Vec<AdminUser>>,
        clients: Mutex<Vec<ClientConfig>>,
    }

    #[async_trait]
    impl AdminRepository for FakeRepo {
        async fn get_admin(&self, email: &str) -> Result<AdminUser, ProxyError> {
            self.admins
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .find(|a| a.email == email.to_lowercase())
                .cloned()
                .ok_or_else(|| ProxyError::ConfigNotFound {
                    internal_id: email.to_string(),
                })
        }

        async fn list_admins(&self) -> Result<Vec<AdminUser>, ProxyError> {
            Ok(self.admins.lock().unwrap_or_else(|p| p.into_inner()).clone())
        }

        async fn put_admin(&self, admin: &AdminUser) -> Result<AdminUser, ProxyError> {
            let mut admins = self.admins.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(existing) = admins
                .iter_mut()
                .find(|a| a.email == admin.email.to_lowercase())
            {
                *existing = admin.clone();
            } else {
                admins.push(admin.clone());
            }
            Ok(admin.clone())
        }

        async fn delete_admin(&self, email: &str) -> Result<(), ProxyError> {
            self.admins
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|a| a.email != email.to_lowercase());
            Ok(())
        }

        async fn list_clients(
            &self,
            _limit: u64,
            _skip: u64,
        ) -> Result<(Vec<ClientConfig>, u64), ProxyError> {
            let clients = self.clients.lock().unwrap_or_else(|p| p.into_inner()).clone();
            let total = clients.len() as u64;
            Ok((clients, total))
        }

        async fn get_client_by_internal_id(
            &self,
            internal_id: &str,
        ) -> Result<ClientConfig, ProxyError> {
            self.clients
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .find(|c| c.internal_id == internal_id)
                .cloned()
                .ok_or_else(|| ProxyError::ConfigNotFound {
                    internal_id: internal_id.to_string(),
                })
        }

        async fn put_client(&self, config: &ClientConfig) -> Result<ClientConfig, ProxyError> {
            let mut clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(existing) = clients
                .iter_mut()
                .find(|c| c.internal_id == config.internal_id)
            {
                *existing = config.clone();
            } else {
                clients.push(config.clone());
            }
            Ok(config.clone())
        }

        async fn delete_client(&self, internal_id: &str) -> Result<(), ProxyError> {
            self.clients
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|c| c.internal_id != internal_id);
            Ok(())
        }

        async fn client_count(&self) -> Result<u64, ProxyError> {
            Ok(self.clients.lock().unwrap_or_else(|p| p.into_inner()).len() as u64)
        }
    }

    #[derive(Default)]
    struct FakeSessions {
        map: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SessionStore for FakeSessions {
        async fn setex(
            &self,
            key: &str,
            value: &str,
            _ttl_seconds: u64,
        ) -> Result<(), ProxyError> {
            self.map
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(key.to_string(), value.to_string());
            Ok(())
        }

        async fn get(&self, key: &str) -> Result<Option<String>, ProxyError> {
            Ok(self
                .map
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(key)
                .cloned())
        }

        async fn getdel(&self, key: &str) -> Result<Option<String>, ProxyError> {
            Ok(self
                .map
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(key))
        }

        async fn del(&self, key: &str) -> Result<(), ProxyError> {
            self.map
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(key);
            Ok(())
        }
    }

    struct FakeGoogle {
        identity: VerifiedIdentity,
    }

    #[async_trait]
    impl IdentityProvider for FakeGoogle {
        fn name(&self) -> &'static str {
            "google"
        }

        fn authorization_url(&self, state: &str) -> String {
            format!("https://accounts.google.com/o/oauth2/v2/auth?state={state}")
        }

        async fn exchange_code(&self, _code: &str) -> Result<VerifiedIdentity, ProxyError> {
            Ok(self.identity.clone())
        }
    }

    fn settings(master: Option<&str>) -> AdminSettings {
        AdminSettings {
            master_token_hash: master.map(|token| keyed_hash(token)),
            allowed_domains: vec!["gmail.com".to_string(), "stringnet.pe".to_string()],
            session_ttl_seconds: 3600,
            oauth_state_ttl_seconds: 300,
            login_rate_limit_requests: 5,
            login_rate_limit_window_seconds: 60,
            admin_rate_limit_requests: 120,
            admin_rate_limit_window_seconds: 60,
            control_rate_limit_requests: 60,
            control_rate_limit_window_seconds: 60,
            cookie_secure: false,
        }
    }

    fn service(
        repo: Arc<dyn AdminRepository>,
        sessions: Arc<dyn SessionStore>,
        providers: IdentityProviders,
        master: Option<&str>,
    ) -> AdminService {
        AdminService::new(repo, sessions, providers, settings(master))
    }

    fn admin(email: &str, active: bool) -> AdminUser {
        AdminUser {
            id: AdminUser::doc_id(email),
            rev: None,
            r#type: "admin_user".to_string(),
            email: email.to_string(),
            role: "admin".to_string(),
            active,
            created_at: "2024-01-01T00:00:00+00:00".to_string(),
            updated_at: "2024-01-01T00:00:00+00:00".to_string(),
            created_by: "master_token".to_string(),
        }
    }

    fn google(identity: VerifiedIdentity) -> IdentityProviders {
        let mut providers = IdentityProviders::new();
        providers.register(Arc::new(FakeGoogle { identity }));
        providers
    }

    #[test]
    fn master_token_acertado_fallado_y_vacio() {
        let repo = Arc::new(FakeRepo::default());
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            IdentityProviders::new(),
            Some("supersecret"),
        );
        assert!(svc.verify_master_token("supersecret"));
        assert!(!svc.verify_master_token("otro"));
        assert!(!svc.verify_master_token(""));

        // Master no configurado: siempre falso, aunque presenten algo.
        let svc_vacio = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            IdentityProviders::new(),
            None,
        );
        assert!(!svc_vacio.verify_master_token("supersecret"));
    }

    #[tokio::test]
    async fn login_google_completo_crea_sesion_de_un_solo_uso() {
        let repo = Arc::new(FakeRepo::default());
        repo.admins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(admin("juan@gmail.com", true));
        let sessions = Arc::new(FakeSessions::default());
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::clone(&sessions) as Arc<dyn SessionStore>,
            google(VerifiedIdentity {
                email: "Juan@Gmail.com".to_string(),
                email_verified: true,
            }),
            None,
        );

        // El state se genera en `google_login_url` y se persiste de un solo uso.
        let url = svc.google_login_url().await.expect("url de login");
        let state = url.split("state=").nth(1).expect("state en la url").to_string();

        let session = svc
            .complete_google_login("code", &state)
            .await
            .expect("login completo");
        assert_eq!(session.email, "juan@gmail.com");

        // La sesión resuelve.
        let resolved = svc
            .resolve_session(&session.session_id)
            .await
            .expect("sesión resuelta");
        assert_eq!(resolved, "juan@gmail.com");

        // Reusar el mismo state es un 400: consumo de un solo uso.
        let reuse = svc.complete_google_login("code", &state).await;
        assert!(matches!(
            reuse,
            Err(ProxyError::InvalidConfig { ref field, .. }) if field == "state"
        ));

        // Logout: la sesión deja de resolver.
        let email = svc
            .destroy_session(&session.session_id)
            .await
            .expect("logout");
        assert_eq!(email.as_deref(), Some("juan@gmail.com"));
        assert!(svc.resolve_session(&session.session_id).await.is_none());
    }

    #[tokio::test]
    async fn login_rechaza_dominio_no_permitido_y_admin_desconocido() {
        let cases = [
            (
                VerifiedIdentity {
                    email: "juan@evil.com".to_string(),
                    email_verified: true,
                },
                "dominio no permitido",
            ),
            (
                VerifiedIdentity {
                    email: "juan@gmail.com".to_string(),
                    email_verified: false,
                },
                "email no verificado",
            ),
        ];
        for (identity, label) in cases {
            let svc = service(
                Arc::new(FakeRepo::default()) as Arc<dyn AdminRepository>,
                Arc::new(FakeSessions::default()),
                google(identity),
                None,
            );
            let state = svc.google_login_url().await.expect("url");
            let state = state.split("state=").nth(1).expect("state").to_string();
            let result = svc.complete_google_login("code", &state).await;
            assert!(
                matches!(result, Err(ProxyError::Forbidden { .. })),
                "{label}: {result:?}"
            );
        }

        // Email verificado y dominio permitido pero sin registro de admin: 403.
        let svc = service(
            Arc::new(FakeRepo::default()) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            google(VerifiedIdentity {
                email: "nadie@gmail.com".to_string(),
                email_verified: true,
            }),
            None,
        );
        let state = svc.google_login_url().await.expect("url");
        let state = state.split("state=").nth(1).expect("state").to_string();
        let result = svc.complete_google_login("code", &state).await;
        assert!(matches!(result, Err(ProxyError::Forbidden { ..})));
    }

    #[tokio::test]
    async fn login_rechaza_admin_desactivado() {
        let repo = Arc::new(FakeRepo::default());
        repo.admins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(admin("juan@gmail.com", false));
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            google(VerifiedIdentity {
                email: "juan@gmail.com".to_string(),
                email_verified: true,
            }),
            None,
        );
        let state = svc.google_login_url().await.expect("url");
        let state = state.split("state=").nth(1).expect("state").to_string();
        let result = svc.complete_google_login("code", &state).await;
        assert!(matches!(result, Err(ProxyError::Forbidden { .. })));
    }

    #[tokio::test]
    async fn admin_no_puede_autodesactivarse_ni_autoborrarse() {
        let repo = Arc::new(FakeRepo::default());
        repo.admins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(admin("juan@gmail.com", true));
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            IdentityProviders::new(),
            Some("supersecret"),
        );

        let desactivar = svc
            .set_admin_active("juan@gmail.com", false, "juan@gmail.com")
            .await;
        assert!(matches!(desactivar, Err(ProxyError::Forbidden { .. })));

        let borrar = svc.delete_admin("juan@gmail.com", "juan@gmail.com").await;
        assert!(matches!(borrar, Err(ProxyError::Forbidden { .. })));
    }

    #[tokio::test]
    async fn no_se_borra_el_ultimo_admin_activo() {
        let repo = Arc::new(FakeRepo::default());
        repo.admins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(admin("juan@gmail.com", true));
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            IdentityProviders::new(),
            Some("supersecret"),
        );

        // Otro admin (inactivo) sí puede eliminarse; el único activo, no.
        svc.create_admin("otro@gmail.com", "master_token")
            .await
            .expect("crear segundo admin");
        svc.set_admin_active("otro@gmail.com", false, "master_token")
            .await
            .expect("desactivar segundo admin");
        svc.delete_admin("otro@gmail.com", "master_token")
            .await
            .expect("borrar inactivo");

        let ultimo = svc.delete_admin("juan@gmail.com", "master_token").await;
        assert!(matches!(ultimo, Err(ProxyError::Forbidden { .. })));
    }

    #[tokio::test]
    async fn create_client_devuelve_token_y_kind_client() {
        let repo = Arc::new(FakeRepo::default());
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            IdentityProviders::new(),
            Some("supersecret"),
        );
        let (config, token) = svc.create_client("master_token", None).await.expect("crear cliente");
        assert_eq!(token.len(), 32);
        assert_eq!(config.kind, ClientKind::Client);
        assert_eq!(config.bearer_token_hash, keyed_hash(&token));
        // El token en claro no se guarda: solo su hash.
        let stored = svc.get_client(&config.internal_id).await.expect("leer cliente");
        assert!(!serde_json::to_string(&stored)
            .expect("serializar")
            .contains(&token));
        assert_eq!(stored.config_version, 1);
    }

    #[tokio::test]
    async fn rotar_token_cambia_hash_y_solo_se_devuelve_una_vez() {
        let repo = Arc::new(FakeRepo::default());
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            IdentityProviders::new(),
            Some("supersecret"),
        );
        let (config, token_viejo) = svc.create_client("master_token", None).await.expect("crear");
        let (rotado, token_nuevo) = svc
            .rotate_client_token(&config.internal_id, "master_token")
            .await
            .expect("rotar");
        assert_ne!(token_viejo, token_nuevo);
        assert_eq!(rotado.bearer_token_hash, keyed_hash(&token_nuevo));
        assert_eq!(rotado.config_version, config.config_version + 1);
    }

    #[tokio::test]
    async fn admin_puede_activar_y_quitar_wildcard_y_el_cliente_nunca() {
        let repo = Arc::new(FakeRepo::default());
        let svc = service(
            Arc::clone(&repo) as Arc<dyn AdminRepository>,
            Arc::new(FakeSessions::default()),
            IdentityProviders::new(),
            Some("supersecret"),
        );

        // Creación con wildcard: la whitelist puede quedar vacía, es coherente.
        let (config, _) = svc
            .create_client("master_token", Some(true))
            .await
            .expect("crear cliente wildcard");
        assert!(config.wildcard);
        assert!(config.whitelist.is_empty());

        // El PUT de la API admin lo aplica y lo quita (y sube config_version).
        let mut base = crate::domain::models::ClientConfigUpdate {
            whitelist: None,
            rate_limit: None,
            max_scripting_body_bytes: None,
            scripting: None,
            error_handling: None,
            header_rules: None,
        };
        let quitado = svc
            .update_client(
                &config.internal_id,
                &AdminClientUpdate {
                    config: base.clone(),
                    wildcard: Some(false),
                },
                "master_token",
            )
            .await
            .expect("quitar wildcard");
        assert!(!quitado.wildcard);
        assert_eq!(quitado.config_version, config.config_version + 1);

        base.whitelist = Some(vec!["example.com".to_string()]);
        let reactivado = svc
            .update_client(
                &config.internal_id,
                &AdminClientUpdate {
                    config: base,
                    wildcard: Some(true),
                },
                "master_token",
            )
            .await
            .expect("reactivar wildcard");
        assert!(reactivado.wildcard);
    }

    #[test]
    fn put_de_cliente_no_puede_llevar_wildcard_por_construccion() {
        // `ClientConfigUpdate` (el payload del `PUT /api/v1/clients/config`) NO tiene campo
        // `wildcard`: no existe en el struct, así que serde lo ignora al deserializar y
        // `validate_config_update` no tiene nada que aceptar. Este test fija ese contrato: si
        // alguien añade el campo al struct en el futuro, aquí se rompe.
        let json = r#"{
            "wildcard": true,
            "whitelist": ["example.com"],
            "rate_limit": {"max_requests": 50, "window_seconds": 60}
        }"#;
        let update: crate::domain::models::ClientConfigUpdate =
            serde_json::from_str(json).expect("el payload del cliente deserializa (campo ignorado)");
        crate::domain::validators::validate_config_update(&update)
            .expect("el payload del cliente valida; el wildcard viajó desapercibido porque no existe");
        let serialized = serde_json::to_string(&update).expect("serializar");
        assert!(
            !serialized.contains("wildcard"),
            "ClientConfigUpdate jamás debe serializar wildcard: {serialized}"
        );
    }

    #[test]
    fn ensure_client_kind_rechaza_admins() {
        let mut config = ClientConfig {
            id: "a".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "a".to_string(),
            crypt_id: "abcdefghijkl".to_string(),
            bearer_token_hash: "sha256:x".to_string(),
            config_version: 1,
            kind: ClientKind::Admin,
            wildcard: false,
            whitelist: Vec::new(),
            rate_limit: RateLimitConfig {
                max_requests: 50,
                window_seconds: 60,
            },
            max_scripting_body_bytes: 0,
            scripting: ScriptingConfig {
                enabled: false,
                code: String::new(),
                code_hash: String::new(),
                expression: String::new(),
            },
            error_handling: crate::domain::models::ErrorHandlingConfig {
                mode: crate::domain::models::ErrorMode::Transparent,
                fallback_urls: std::collections::HashMap::new(),
            },
            header_rules: Vec::new(),
        };
        assert!(AdminService::ensure_client_kind(&config).is_err());
        config.kind = ClientKind::Client;
        assert!(AdminService::ensure_client_kind(&config).is_ok());
    }
}
