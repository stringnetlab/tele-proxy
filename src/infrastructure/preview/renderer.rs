use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::errors::ProxyError;

/// Formato del render. Cada uno apunta a un endpoint distinto del sidecar browserless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderFormat {
    Screenshot,
    Pdf,
}

impl RenderFormat {
    fn endpoint(self) -> &'static str {
        match self {
            Self::Screenshot => "/screenshot",
            Self::Pdf => "/pdf",
        }
    }

    /// MIME del binario devuelto. El sidecar responde los bytes crudos; el content-type lo fija
    /// aquí el formato pedido (browserless no lo declara de forma fiable).
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Screenshot => "image/png",
            Self::Pdf => "application/pdf",
        }
    }
}

/// Petición de render. La `url` ya viene **validada** por el llamador (anti-SSRF + whitelist):
/// el sidecar es infraestructura interna de confianza y el binario que produce no se revalida.
#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub url: String,
    pub format: RenderFormat,
    pub full_page: bool,
    pub viewport: [u32; 2],
    /// Deadline del render; el handler lo acota contra `preview.max_render_timeout_ms`.
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RenderOutput {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

/// Cuerpo JSON de `/screenshot` (schema de browserless). Los campos que no se envían quedan al
/// default del sidecar.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScreenshotBody<'a> {
    url: &'a str,
    timeout: u64,
    goto_options: GotoOptions,
    viewport: Viewport,
    options: ScreenshotOptions,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PdfBody<'a> {
    url: &'a str,
    timeout: u64,
    goto_options: GotoOptions,
    options: PdfOptions,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GotoOptions {
    wait_until: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContentBody<'a> {
    url: &'a str,
    timeout: u64,
    goto_options: GotoOptions,
    viewport: Viewport,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Viewport {
    width: u32,
    height: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScreenshotOptions {
    r#type: &'static str,
    full_page: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PdfOptions {
    format: &'static str,
    print_background: bool,
    landscape: bool,
}

/// Cliente REST del sidecar browserless (D7). **Fail-soft por contrato**: si el sidecar no está
/// configurado, no responde o devuelve un error, se mapea a `RenderUnavailable`/`RenderTimeout`
/// (503/504), nunca a un 500 interno — un render caído no debe tumbar `/mt/`, que no lo usa.
///
/// El `base_url` sale del entorno (operador), no de la petición, así que no es superficie de SSRF;
/// por eso este cliente usa un `reqwest::Client` normal en vez del pipeline pinned.
pub struct BrowserlessRenderer {
    http: reqwest::Client,
    base_url: Option<String>,
    token: Option<String>,
    /// Tope del binario aceptado del sidecar (defensa contra un render desbocado).
    max_output_bytes: u64,
}

impl BrowserlessRenderer {
    /// `base_url` vacío/`None` → renderer desactivado: cualquier render devuelve
    /// `RenderUnavailable` sin intentar red.
    pub fn new(
        base_url: Option<String>,
        token: Option<String>,
        timeout: Duration,
        max_output_bytes: u64,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("cliente HTTP del renderer construible");
        Self {
            http,
            base_url: base_url.filter(|u| !u.trim().is_empty()),
            token: token.filter(|t| !t.trim().is_empty()),
            max_output_bytes,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.base_url.is_some()
    }

    /// Salud del sidecar (`GET /pressure`). `false` en cualquier fallo: el handler la usa para
    /// degradar sin cortar el servicio.
    pub async fn healthy(&self) -> bool {
        let Some(base) = self.base_url.as_ref() else {
            return false;
        };
        let url = self.with_token(&format!("{}/pressure", base.trim_end_matches('/')));
        self.http
            .get(url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    pub async fn render(&self, req: &RenderRequest) -> Result<RenderOutput, ProxyError> {
        let Some(base) = self.base_url.as_ref() else {
            return Err(ProxyError::RenderUnavailable {
                reason: "sidecar de render no configurado".to_string(),
            });
        };
        let base = base.trim_end_matches('/');
        let url = self.with_token(&format!("{base}{}", req.format.endpoint()));

        let body = match req.format {
            RenderFormat::Screenshot => serde_json::to_value(ScreenshotBody {
                url: &req.url,
                timeout: req.timeout_ms,
                goto_options: GotoOptions {
                    wait_until: "networkidle2",
                },
                viewport: Viewport {
                    width: req.viewport[0],
                    height: req.viewport[1],
                },
                options: ScreenshotOptions {
                    r#type: "png",
                    full_page: req.full_page,
                },
            })
            .expect("cuerpo de screenshot serializable"),
            RenderFormat::Pdf => serde_json::to_value(PdfBody {
                url: &req.url,
                timeout: req.timeout_ms,
                goto_options: GotoOptions {
                    wait_until: "networkidle2",
                },
                options: PdfOptions {
                    format: "A4",
                    print_background: true,
                    landscape: false,
                },
            })
            .expect("cuerpo de pdf serializable"),
        };

        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.map_error(&req.url, req.timeout_ms, e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(ProxyError::RenderUnavailable {
                reason: format!("el sidecar respondió {}", status.as_u16()),
            });
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProxyError::RenderUnavailable {
                reason: format!("no se pudo leer el binario del render: {e}"),
            })?;

        if bytes.len() as u64 > self.max_output_bytes {
            return Err(ProxyError::PreviewPayloadTooLarge {
                content_length: bytes.len() as u64,
                max_allowed: self.max_output_bytes,
            });
        }

        Ok(RenderOutput {
            bytes: bytes.to_vec(),
            content_type: req.format.content_type(),
        })
    }

    /// `/content`: HTML tras ejecutar JavaScript. Lo usa `/mt/` como escalado cuando el fetch
    /// directo devuelve un shell (anti-bot o sin metadatos). Mismo fail-soft que `render()`.
    pub async fn rendered_html(
        &self,
        url: &str,
        timeout_ms: u64,
        viewport: [u32; 2],
    ) -> Result<String, ProxyError> {
        let Some(base) = self.base_url.as_ref() else {
            return Err(ProxyError::RenderUnavailable {
                reason: "sidecar de render no configurado".to_string(),
            });
        };
        let base = base.trim_end_matches('/');
        let endpoint = self.with_token(&format!("{base}/content"));
        let body = ContentBody {
            url,
            timeout: timeout_ms,
            goto_options: GotoOptions {
                wait_until: "networkidle2",
            },
            viewport: Viewport {
                width: viewport[0],
                height: viewport[1],
            },
        };

        let response = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.map_error(url, timeout_ms, e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(ProxyError::RenderUnavailable {
                reason: format!("el sidecar respondió {}", status.as_u16()),
            });
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProxyError::RenderUnavailable {
                reason: format!("no se pudo leer el HTML renderizado: {e}"),
            })?;
        if bytes.len() as u64 > self.max_output_bytes {
            return Err(ProxyError::PreviewPayloadTooLarge {
                content_length: bytes.len() as u64,
                max_allowed: self.max_output_bytes,
            });
        }

        String::from_utf8(bytes.to_vec()).map_err(|e| ProxyError::RenderUnavailable {
            reason: format!("el HTML renderizado no es UTF-8: {e}"),
        })
    }

    fn map_error(&self, url: &str, timeout_ms: u64, e: reqwest::Error) -> ProxyError {
        if e.is_timeout() {
            ProxyError::RenderTimeout {
                url: url.to_string(),
                timeout_ms,
            }
        } else if e.is_connect() {
            ProxyError::RenderUnavailable {
                reason: "no se pudo conectar al sidecar de render".to_string(),
            }
        } else {
            ProxyError::RenderUnavailable {
                reason: format!("fallo del sidecar de render: {e}"),
            }
        }
    }

    fn with_token(&self, url: &str) -> String {
        match &self.token {
            Some(token) => format!("{url}?token={token}"),
            None => url.to_string(),
        }
    }
}

/// Solo para los tests/deserialización de un healthcheck estructurado si el sidecar lo ofrece.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Pressure {
    pub is_healthy: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_base_url_el_render_es_no_disponible() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let renderer = BrowserlessRenderer::new(None, None, Duration::from_millis(100), 1024);
        assert!(!renderer.is_configured());
        let req = RenderRequest {
            url: "https://a/x".into(),
            format: RenderFormat::Screenshot,
            full_page: false,
            viewport: [1280, 720],
            timeout_ms: 100,
        };
        let err = rt.block_on(renderer.render(&req)).unwrap_err();
        assert!(
            matches!(err, ProxyError::RenderUnavailable { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn base_url_vacio_cuenta_como_desactivado() {
        let renderer =
            BrowserlessRenderer::new(Some("   ".into()), None, Duration::from_millis(10), 1024);
        assert!(!renderer.is_configured());
    }

    #[test]
    fn endpoint_y_content_type_por_formato() {
        assert_eq!(RenderFormat::Screenshot.endpoint(), "/screenshot");
        assert_eq!(RenderFormat::Pdf.endpoint(), "/pdf");
        assert_eq!(RenderFormat::Screenshot.content_type(), "image/png");
        assert_eq!(RenderFormat::Pdf.content_type(), "application/pdf");
    }

    #[test]
    fn token_se_anade_como_query() {
        let renderer = BrowserlessRenderer::new(
            Some("http://sidecar:3000".into()),
            Some("secreto".into()),
            Duration::from_millis(10),
            1024,
        );
        let url = renderer.with_token("http://sidecar:3000/screenshot");
        assert_eq!(url, "http://sidecar:3000/screenshot?token=secreto");
    }

    #[test]
    fn cuerpo_de_screenshot_es_json_valido() {
        let body = ScreenshotBody {
            url: "https://a/x",
            timeout: 1000,
            goto_options: GotoOptions {
                wait_until: "networkidle2",
            },
            viewport: Viewport {
                width: 1920,
                height: 1080,
            },
            options: ScreenshotOptions {
                r#type: "png",
                full_page: true,
            },
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["gotoOptions"]["waitUntil"], "networkidle2");
        assert_eq!(json["options"]["fullPage"], true);
        assert_eq!(json["options"]["type"], "png");
    }
}
