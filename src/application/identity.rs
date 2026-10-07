use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::domain::errors::ProxyError;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
/// Cota del cuerpo del token endpoint (y de los JWKS): la respuesta real son pocos KB, así que
/// un cuerpo mayor solo puede ser un endpoint que ya no es el de Google (mismo criterio que
/// `infrastructure/http_client.rs::read_body_capped`).
const MAX_OAUTH_RESPONSE_BYTES: u64 = 16 * 1024;
/// Vida de la caché en memoria del JWKS de Google. Los `kid` rotan poco; si llega un `kid`
/// desconocido, la caché se refresca en caliente sin esperar a la expiración.
const JWKS_CACHE_TTL: Duration = Duration::from_secs(3600);
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Identidad verificada por un proveedor OAuth2. `email` ya viene normalizada a minúsculas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    pub email: String,
    pub email_verified: bool,
}

/// Seam de proveedores de identidad. Hoy existe solo `GoogleIdentityProvider`; añadir Zentiel
/// (u otro IdP) es implementar este trait y registrarlo en `IdentityProviders` — la API admin y
/// el `AdminService` no cambian.
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    fn name(&self) -> &'static str;
    /// URL a la que se redirige el navegador del operador para iniciar el login.
    fn authorization_url(&self, state: &str) -> String;
    /// Canjea el `code` del callback por una identidad verificada (incluye la verificación
    /// RS256 del id_token y de los claims `iss`/`aud`/`exp`/`email_verified`).
    async fn exchange_code(&self, code: &str) -> Result<VerifiedIdentity, ProxyError>;
}

/// Registro de proveedores por nombre (`"google"` hoy, `"zentiel"` en el futuro).
#[derive(Default)]
pub struct IdentityProviders {
    providers: HashMap<&'static str, Arc<dyn IdentityProvider>>,
}

impl IdentityProviders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn IdentityProvider>) {
        self.providers.insert(provider.name(), provider);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn IdentityProvider>> {
        self.providers.get(name).cloned()
    }

    /// El login Google está configurado (credenciales presentes en el arranque).
    pub fn has_google(&self) -> bool {
        self.providers.contains_key("google")
    }
}

/// Login con Google (OAuth2 + OpenID Connect `openid email`). El `id_token` se verifica contra
/// los JWKS de Google (`RS256`), nunca se acepta sin firma.
pub struct GoogleIdentityProvider {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    http: reqwest::Client,
    jwks: Arc<RwLock<Option<(jsonwebtoken::jwk::JwkSet, Instant)>>>,
}

impl GoogleIdentityProvider {
    pub fn new(
        client_id: String,
        client_secret: String,
        redirect_uri: String,
    ) -> Result<Self, ProxyError> {
        // `Policy::none()` por el mismo motivo que `http_client::pinned_client`: seguir un
        // redirect revalidaría un destino que nadie autorizó.
        let http = reqwest::Client::builder()
            .timeout(OAUTH_HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ProxyError::Internal {
                reason: format!("No se pudo construir el cliente HTTP de Google OAuth: {}", e),
            })?;
        Ok(Self {
            client_id,
            client_secret,
            redirect_uri,
            http,
            jwks: Arc::new(RwLock::new(None)),
        })
    }

    fn validation(&self) -> jsonwebtoken::Validation {
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_issuer(&["accounts.google.com", "https://accounts.google.com"]);
        validation.set_audience(&[self.client_id.as_str()]);
        validation.required_spec_claims = ["exp", "iss", "aud"]
            .iter()
            .map(|claim| claim.to_string())
            .collect();
        validation
    }

    /// JWKS en caché (~1 h); si el `kid` no está, fuerza un refresco una vez. Errores sin
    /// detalles internos: el motivo vive en el log, el cliente recibe un 401 escueto.
    async fn jwk_for_kid(
        &self,
        kid: &str,
    ) -> Result<jsonwebtoken::jwk::Jwk, ProxyError> {
        {
            let cached = self.jwks.read().await;
            if let Some((set, fetched_at)) = cached.as_ref() {
                if fetched_at.elapsed() < JWKS_CACHE_TTL {
                    if let Some(key) = set.find(kid) {
                        return Ok(key.clone());
                    }
                }
            }
        }

        let set = self.fetch_jwks().await?;
        let key = set.find(kid).cloned();
        *self.jwks.write().await = Some((set, Instant::now()));
        key.ok_or_else(|| ProxyError::Unauthorized {
            reason: "el id_token usa un kid desconocido en los JWKS de Google".to_string(),
        })
    }

    async fn fetch_jwks(&self) -> Result<jsonwebtoken::jwk::JwkSet, ProxyError> {
        let resp = self
            .http
            .get(GOOGLE_JWKS_URL)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "Falló la descarga de los JWKS de Google");
                ProxyError::Unauthorized {
                    reason: "no se pudieron obtener las claves públicas de Google".to_string(),
                }
            })?;
        let body = crate::infrastructure::http_client::read_body_capped(
            resp,
            MAX_OAUTH_RESPONSE_BYTES,
        )
        .await
        .map_err(|_| ProxyError::Unauthorized {
            reason: "los JWKS de Google no tienen la forma esperada".to_string(),
        })?;
        serde_json::from_slice(&body).map_err(|e| {
            tracing::warn!(error = %e, "Los JWKS de Google no parsean");
            ProxyError::Unauthorized {
                reason: "los JWKS de Google no tienen la forma esperada".to_string(),
            }
        })
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct GoogleIdClaims {
    iss: String,
    aud: serde_json::Value,
    exp: u64,
    email: String,
    #[serde(default)]
    email_verified: bool,
}

#[derive(Deserialize)]
struct GoogleTokenResponse {
    id_token: String,
}

#[async_trait]
impl IdentityProvider for GoogleIdentityProvider {
    fn name(&self) -> &'static str {
        "google"
    }

    fn authorization_url(&self, state: &str) -> String {
        url::Url::parse_with_params(
            GOOGLE_AUTH_URL,
            [
                ("client_id", self.client_id.as_str()),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("response_type", "code"),
                ("scope", "openid email"),
                ("state", state),
            ],
        )
        .map(|url| url.to_string())
        .unwrap_or_else(|_| GOOGLE_AUTH_URL.to_string())
    }

    async fn exchange_code(&self, code: &str) -> Result<VerifiedIdentity, ProxyError> {
        // Cuerpo urlencoded a mano: la feature `form` de reqwest no está activada en este
        // árbol (y no se toca por una sola llamada); `url` ya es dependencia del proyecto.
        let form = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("code", code),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("grant_type", "authorization_code"),
            ])
            .finish();
        let resp = self
            .http
            .post(GOOGLE_TOKEN_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "Falló el canje del code contra Google");
                ProxyError::Unauthorized {
                    reason: "Google no aceptó el código de autorización".to_string(),
                }
            })?;

        let body = crate::infrastructure::http_client::read_body_capped(
            resp,
            MAX_OAUTH_RESPONSE_BYTES,
        )
        .await
        .map_err(|_| ProxyError::Unauthorized {
            reason: "la respuesta del token endpoint no tiene la forma esperada".to_string(),
        })?;
        let token: GoogleTokenResponse = serde_json::from_slice(&body).map_err(|e| {
            tracing::warn!(error = %e, "El token endpoint de Google no devolvió un id_token");
            ProxyError::Unauthorized {
                reason: "Google no devolvió un id_token".to_string(),
            }
        })?;

        let header = jsonwebtoken::decode_header(&token.id_token).map_err(|e| {
            tracing::warn!(error = %e, "El id_token de Google no tiene cabecera JWT válida");
            ProxyError::Unauthorized {
                reason: "id_token de Google inválido".to_string(),
            }
        })?;
        let kid = header.kid.ok_or_else(|| ProxyError::Unauthorized {
            reason: "id_token de Google sin kid".to_string(),
        })?;
        let jwk = self.jwk_for_kid(&kid).await?;
        let key = jsonwebtoken::DecodingKey::from_jwk(&jwk).map_err(|e| {
            tracing::warn!(error = %e, "El JWK de Google no construye una clave de verificación");
            ProxyError::Unauthorized {
                reason: "clave pública de Google inválida".to_string(),
            }
        })?;

        let data: jsonwebtoken::TokenData<GoogleIdClaims> =
            jsonwebtoken::decode(&token.id_token, &key, &self.validation()).map_err(|e| {
                tracing::warn!(error = %e, "La verificación del id_token de Google falló");
                ProxyError::Unauthorized {
                    reason: "la firma o los claims del id_token no verifican".to_string(),
                }
            })?;

        // `Validation` ya garantizó iss/aud/exp; el claim que decide el login es
        // `email_verified` (sin email verificado el dominio permitido no prueba nada).
        Ok(VerifiedIdentity {
            email: data.claims.email.to_lowercase(),
            email_verified: data.claims.email_verified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GoogleIdClaims` tiene que deserializar lo que Google envía de verdad: `aud` como
    /// string, `email_verified` como bool, sin claims extra que rompan serde.
    #[test]
    fn claims_de_google_deserializan() {
        let json = r#"{
            "iss": "https://accounts.google.com",
            "aud": "client-id.apps.googleusercontent.com",
            "exp": 1735689600,
            "iat": 1735686000,
            "sub": "1234567890",
            "email": "Operador@gmail.com",
            "email_verified": true
        }"#;
        let claims: GoogleIdClaims = serde_json::from_str(json).expect("claims de Google");
        assert_eq!(claims.email, "Operador@gmail.com");
        assert!(claims.email_verified);
        assert_eq!(claims.iss, "https://accounts.google.com");
    }

    #[test]
    fn email_verified_ausente_es_falso() {
        let json = r#"{
            "iss": "https://accounts.google.com",
            "aud": "x",
            "exp": 1735689600,
            "email": "a@gmail.com"
        }"#;
        let claims: GoogleIdClaims = serde_json::from_str(json).expect("claims");
        assert!(!claims.email_verified);
    }
}
