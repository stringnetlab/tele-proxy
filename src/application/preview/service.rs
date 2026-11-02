use std::net::IpAddr;
use std::time::Duration;

use url::Url;

use crate::application::cache_key::preview_cache_key;
use crate::application::preview::extract::{build_preview, ContentKind};
use crate::application::proxy_service::ProxyService;
use crate::domain::errors::ProxyError;
use crate::domain::models::{ClientConfig, ClientKind};
use crate::domain::preview::config::PreviewConfig;
use crate::domain::preview::result::PreviewData;
use crate::domain::validators::{extract_domain, host_allowed, validate_url_strict};
use crate::infrastructure::preview::fetch::{FetchedResponse, PreviewFetcher};
use crate::infrastructure::preview::renderer::{RenderFormat, RenderOutput, RenderRequest};

/// Parámetros de una petición `/mt/` ya validados y normalizados por el handler.
pub struct PreviewOptions {
    /// `content=1`: activa la extracción de texto/markdown (requiere la capacidad `content`).
    pub want_content: bool,
    /// TTL pedido por el cliente (`ttl=`); se acota contra `preview.ttl_max_seconds`.
    pub ttl: Option<u64>,
    /// `force=1`: salta la lectura de caché (aún escribe el resultado fresco).
    pub force: bool,
    /// Forma **canónica** de los parámetros (orden estable) para la clave de caché.
    pub params_canonical: String,
}

/// Resultado de `/mt/`: los datos y dos flags para las cabeceras de respuesta.
pub struct PreviewOutcome {
    pub data: PreviewData,
    pub degraded: bool,
    pub cache_hit: bool,
}

/// Interruptor que decide si un endpoint de preview está habilitado. `/mt/` (metadatos) y `/pv/`
/// (render) llevan flags separados a propósito: apagar el render no debe apagar los metadatos.
#[derive(Clone, Copy)]
enum PreviewGate {
    Meta,
    Render,
}

impl PreviewGate {
    /// `(habilitado, nombre del campo)` — el nombre viaja en el mensaje del 403 para que el
    /// operador sepa qué clave del documento tiene que activar.
    fn of(preview: &PreviewConfig, gate: PreviewGate) -> (bool, &'static str) {
        match gate {
            PreviewGate::Meta => (preview.meta_enabled, "meta_enabled"),
            PreviewGate::Render => (preview.enabled, "enabled"),
        }
    }
}

impl ProxyService {
    /// Preamble compartido de preview: crypt_id→config, rechazo de admins, interruptor del
    /// endpoint y rate limit por capacidad. Devuelve la config para que cada endpoint aplique su
    /// propia comprobación de capacidad y sus límites.
    async fn preview_preamble(
        &self,
        crypt_id: &str,
        capability_key: &str,
        gate: PreviewGate,
    ) -> Result<ClientConfig, ProxyError> {
        let config = self.config_fetcher().get_by_crypt_id(crypt_id).await?;

        // Un admin no puede usar la API de preview (misma separación que `/aq/`).
        if matches!(config.kind, ClientKind::Admin) {
            return Err(ProxyError::Forbidden {
                reason: "los administradores no pueden usar la API de preview".to_string(),
            });
        }

        let (enabled, flag) = PreviewGate::of(&config.preview, gate);
        if !enabled {
            return Err(ProxyError::PreviewDisabled {
                reason: format!("el cliente no tiene `preview.{flag}`"),
            });
        }

        let rate_limit = self
            .cache_store()
            .check_preview_rate_limit(
                crypt_id,
                capability_key,
                config.preview.rate_limit.max_requests,
                config.preview.rate_limit.window_seconds,
            )
            .await;
        if !rate_limit.allowed {
            return Err(ProxyError::RateLimitExceeded {
                current_count: rate_limit.current_count,
                max_requests: config.preview.rate_limit.max_requests,
                retry_after_secs: rate_limit.retry_after_secs,
            });
        }

        Ok(config)
    }

    fn degraded_flag(&self, config: &ClientConfig) -> bool {
        self.is_degraded() || config.is_degraded()
    }

    /// `/mt/{crypt_id}`: metadatos (+ contenido opcional). Fetch validado, cascada de detectores,
    /// caché JSON por `preview_cache_key`. Un fallo del almacén degrada a miss, nunca a error.
    pub async fn meta_preview(
        &self,
        crypt_id: &str,
        url: &str,
        opts: &PreviewOptions,
    ) -> Result<PreviewOutcome, ProxyError> {
        let config = self
            .preview_preamble(crypt_id, "meta", PreviewGate::Meta)
            .await?;

        if !config.preview.capabilities.meta {
            return Err(ProxyError::PreviewDisabled {
                reason: "la capacidad `meta` no está activa para este cliente".to_string(),
            });
        }
        if opts.want_content && !config.preview.capabilities.content {
            return Err(ProxyError::PreviewDisabled {
                reason: "la capacidad `content` no está activa para este cliente".to_string(),
            });
        }

        let key = preview_cache_key(
            "mt",
            &config.internal_id,
            config.config_version,
            url,
            &opts.params_canonical,
        );

        if !opts.force {
            if let Some(bytes) = self.cache_store().get_preview(&key).await? {
                if let Ok(data) = serde_json::from_slice::<PreviewData>(&bytes) {
                    return Ok(PreviewOutcome {
                        data,
                        degraded: self.degraded_flag(&config),
                        cache_hit: true,
                    });
                }
                // Entrada corrupta o de otro formato: se trata como miss y se sobrescribe.
            }
        }

        let settings = self.settings();
        let timeout = Duration::from_secs(settings.upstream_timeout_secs.max(1));
        let fetcher = PreviewFetcher::new(
            self.dns_resolver().clone(),
            timeout,
            config.preview.max_body_bytes,
        );
        let fetched = fetcher
            .fetch(url, &config.whitelist, config.wildcard)
            .await?;

        let mut data = build_preview(&fetched, &config.preview, opts.want_content);

        // Puerta de usabilidad html-get: el fetch directo puede devolver un desafío anti-bot o un
        // shell de SPA sin metadatos. Solo en ese caso se escala al sidecar (más caro) y se
        // conserva el render únicamente si aporta estrictamente más campos que el fetch.
        if config.preview.enabled && should_try_render(&data) {
            if let Some(renderer) = self.renderer().filter(|renderer| renderer.is_configured()) {
                match renderer
                    .rendered_html(
                        &fetched.final_url,
                        config.preview.max_render_timeout_ms,
                        config.preview.screenshot.default_viewport,
                    )
                    .await
                {
                    Ok(html) if !html.trim().is_empty() => {
                        let rendered_response = FetchedResponse {
                            status: 200,
                            final_url: fetched.final_url.clone(),
                            content_type: Some("text/html; charset=utf-8".to_string()),
                            headers: Vec::new(),
                            cookies: Vec::new(),
                            body: html.into_bytes(),
                        };
                        let rendered =
                            build_preview(&rendered_response, &config.preview, opts.want_content);
                        if preview_quality(&rendered) > preview_quality(&data) {
                            tracing::info!(
                                url = %fetched.final_url,
                                "El render aportó más metadatos que el fetch; se usa el HTML renderizado"
                            );
                            data = rendered;
                        }
                    }
                    Ok(_) => {}
                    // Fail-soft: sin sidecar o con un render fallido se devuelve lo extraído del
                    // fetch; `/mt/` nunca depende del render para responder.
                    Err(e) => tracing::info!(
                        error = %e,
                        url = %fetched.final_url,
                        "El render de escalado no está disponible; se devuelve el fetch"
                    ),
                }
            }
        }

        let ttl = opts
            .ttl
            .unwrap_or(config.preview.ttl_default_seconds)
            .min(config.preview.ttl_max_seconds);
        if let Ok(bytes) = serde_json::to_vec(&data) {
            // set_preview ya degrada a no-op si Valkey no responde.
            let _ = self.cache_store().set_preview(&key, &bytes, ttl).await;
        }

        Ok(PreviewOutcome {
            data,
            degraded: self.degraded_flag(&config),
            cache_hit: false,
        })
    }

    /// `/pv/{crypt_id}`: screenshot o PDF vía el sidecar browserless. Valida la URL destino
    /// (anti-SSRF + whitelist) **antes** de entregársela al sidecar, porque Chromium no debe poder
    /// apuntar a redes internas aunque el fetch lo haga otro proceso. Fail-soft: sin sidecar o con
    /// sidecar caído devuelve `render_unavailable`, nunca un 500 interno.
    pub async fn render_preview(
        &self,
        crypt_id: &str,
        url: &str,
        format: RenderFormat,
        full_page: Option<bool>,
        timeout_ms: Option<u64>,
    ) -> Result<RenderOutput, ProxyError> {
        let capability_key = match format {
            RenderFormat::Screenshot => "screenshot",
            RenderFormat::Pdf => "pdf",
        };
        let config = self
            .preview_preamble(crypt_id, capability_key, PreviewGate::Render)
            .await?;

        let capability_on = match format {
            RenderFormat::Screenshot => config.preview.capabilities.screenshot,
            RenderFormat::Pdf => config.preview.capabilities.pdf,
        };
        if !capability_on {
            return Err(ProxyError::PreviewDisabled {
                reason: format!("la capacidad `{capability_key}` no está activa para este cliente"),
            });
        }

        let renderer = self
            .renderer()
            .ok_or_else(|| ProxyError::RenderUnavailable {
                reason: "sidecar de render no configurado".to_string(),
            })?;

        // Validación de destino: la URL se resuelve y se comprueba contra la whitelist y el
        // anti-SSRF de IP, igual que en el fetch de `/mt/`.
        let parsed = validate_url_strict(url)?;
        let domain = extract_domain(&parsed)?;
        if !host_allowed(&domain, &config.whitelist, config.wildcard) {
            return Err(ProxyError::DomainNotWhitelisted { domain });
        }
        self.validate_destination_ip(&parsed).await?;

        let timeout_ms = timeout_ms
            .unwrap_or(config.preview.max_render_timeout_ms)
            .min(config.preview.max_render_timeout_ms);
        let full_page = full_page.unwrap_or(config.preview.screenshot.max_full_page)
            && config.preview.screenshot.max_full_page;

        let request = RenderRequest {
            url: parsed.to_string(),
            format,
            full_page,
            viewport: config.preview.screenshot.default_viewport,
            timeout_ms,
        };
        renderer.render(&request).await
    }

    /// Fuerza la validación anti-SSRF de la IP de destino sin usarla para conectar (conecta el
    /// sidecar). Para un nombre, `resolve_and_validate` comprueba que no apunte a red interna; para
    /// una IP literal, `validate_url_strict` ya rechazó las privadas en el paso anterior.
    async fn validate_destination_ip(&self, url: &Url) -> Result<(), ProxyError> {
        let hostname = url.host_str().ok_or_else(|| ProxyError::InvalidUrlFormat {
            url: url.to_string(),
            reason: "URL sin host".to_string(),
        })?;
        match hostname.parse::<IpAddr>() {
            Ok(_) => Ok(()),
            Err(_) => {
                self.dns_resolver().resolve_and_validate(hostname).await?;
                Ok(())
            }
        }
    }

    /// `ContentKind` de una URL sin descargarla: lo usa el handler para decidir el MIME de la
    /// respuesta de `/pv/` cuando el sidecar devuelve bytes sin content-type fiable.
    pub fn content_kind_of(content_type: Option<&str>) -> ContentKind {
        ContentKind::from_content_type(content_type)
    }
}

/// Señal de que el fetch directo no entregó contenido útil: desafío anti-bot declarado o un
/// documento sin ninguno de los tres campos que definen un preview (título, descripción, imagen).
fn should_try_render(data: &PreviewData) -> bool {
    data.antibot.detected
        || (data.title.is_none() && data.description.is_none() && data.image.is_none())
}

/// Compara dos extracciones para no reemplazar un fetch correcto con un render que aporta menos.
/// Cada campo presente suma uno; una respuesta sin anti-bot suma uno adicional.
fn preview_quality(data: &PreviewData) -> u8 {
    let fields = [
        data.title.is_some(),
        data.description.is_some(),
        data.image.is_some(),
        data.site_name.is_some(),
        data.author.is_some(),
        data.date.is_some(),
        data.open_graph.is_some(),
        data.twitter.is_some(),
        data.json_ld.is_some(),
    ];
    fields.into_iter().filter(|present| *present).count() as u8
        + if data.antibot.detected { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::preview::extract::ContentKind;

    #[test]
    fn escala_al_render_solo_ante_un_shell_o_antibot() {
        let limpio = PreviewData {
            title: Some("Hola".into()),
            ..PreviewData::default()
        };
        assert!(!should_try_render(&limpio));

        assert!(should_try_render(&PreviewData::default()));

        let bloqueado = PreviewData {
            title: Some("Attention Required".into()),
            antibot: crate::domain::preview::antibot::AntibotInfo {
                detected: true,
                provider: Some("cloudflare".into()),
                technique: Some("javascript".into()),
            },
            ..PreviewData::default()
        };
        assert!(should_try_render(&bloqueado));
    }

    #[test]
    fn el_render_solo_reemplaza_si_aporta_estrictamente_mas() {
        let fetch = PreviewData {
            title: Some("A".into()),
            ..PreviewData::default()
        };
        // Mismo número de campos: el render no sustituye al fetch.
        let render_igual = PreviewData {
            description: Some("B".into()),
            ..PreviewData::default()
        };
        assert_eq!(preview_quality(&fetch), preview_quality(&render_igual));

        let render_mejor = PreviewData {
            title: Some("A".into()),
            image: Some(crate::domain::preview::media::MediaInfo::default()),
            ..PreviewData::default()
        };
        assert!(preview_quality(&render_mejor) > preview_quality(&fetch));
    }

    #[test]
    fn content_kind_delegado() {
        assert_eq!(
            ProxyService::content_kind_of(Some("text/html")),
            ContentKind::Html
        );
    }

    #[test]
    fn los_interruptores_de_mt_y_pv_son_independientes() {
        let por_defecto = PreviewConfig::default();
        assert_eq!(
            PreviewGate::of(&por_defecto, PreviewGate::Meta),
            (true, "meta_enabled")
        );
        assert_eq!(
            PreviewGate::of(&por_defecto, PreviewGate::Render),
            (false, "enabled")
        );

        let con_render = PreviewConfig {
            enabled: true,
            ..PreviewConfig::default()
        };
        assert_eq!(
            PreviewGate::of(&con_render, PreviewGate::Render),
            (true, "enabled")
        );

        let sin_meta = PreviewConfig {
            meta_enabled: false,
            enabled: true,
            ..PreviewConfig::default()
        };
        assert_eq!(
            PreviewGate::of(&sin_meta, PreviewGate::Meta),
            (false, "meta_enabled")
        );
        assert_eq!(
            PreviewGate::of(&sin_meta, PreviewGate::Render),
            (true, "enabled")
        );
    }
}
