use serde::{Deserialize, Serialize};

/// Técnica de bloqueo atribuida al proveedor (taxonomía `is-antibot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AntibotTechnique {
    Javascript,
    Captcha,
    Waf,
    Cookie,
}

impl AntibotTechnique {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Javascript => "javascript",
            Self::Captcha => "captcha",
            Self::Waf => "waf",
            Self::Cookie => "cookie",
        }
    }
}

/// Señales declarativas de un proveedor. Cada campo es `None` cuando el proveedor no usa esa
/// señal; `html` es una lista de **substrings** (case-insensitive) que delatan la página de
/// desafío. Portado del schema de `is-antibot` sin el runtime JS.
#[derive(Debug, Clone)]
pub struct AntibotProvider {
    pub name: &'static str,
    pub technique: AntibotTechnique,
    /// Nombre de header cuya mera presencia (o valor igual a `header_value`) delata al proveedor.
    pub header: Option<&'static str>,
    pub header_value: Option<&'static str>,
    /// Nombre de cookie que el proveedor implanta.
    pub cookie: Option<&'static str>,
    /// Status HTTP característico (p.ej. 403 de Cloudflare, 503 de desafío JS).
    pub status: Option<u16>,
    /// Substrings en el HTML del desafío.
    pub html: &'static [&'static str],
    /// `unless_open_graph`: si la respuesta trae OG completos, `is-antibot` no la marca como
    /// desafío aunque haya señal (el contenido real está presente).
    pub unless_open_graph: bool,
}

/// Tabla embebida (subconjunto de los 38 proveedores de `is-antibot`, los que se ven en la
/// práctica). Es `const` para que la detección sea O(proveedores) sin asignaciones.
pub const ANTIBOT_PROVIDERS: &[AntibotProvider] = &[
    AntibotProvider {
        name: "cloudflare",
        technique: AntibotTechnique::Javascript,
        header: Some("server"),
        header_value: Some("cloudflare"),
        cookie: Some("__cf_bm"),
        status: Some(503),
        html: &[
            "cf-browser-verification",
            "checking your browser before accessing",
            "attention required! | cloudflare",
            "cf-chl",
        ],
        unless_open_graph: true,
    },
    AntibotProvider {
        name: "datadome",
        technique: AntibotTechnique::Javascript,
        header: Some("x-datadome-cid"),
        header_value: None,
        cookie: Some("datadome"),
        status: Some(403),
        html: &["datadome", "you have been blocked"],
        unless_open_graph: true,
    },
    AntibotProvider {
        name: "perimeterx",
        technique: AntibotTechnique::Waf,
        header: None,
        header_value: None,
        cookie: Some("_px3"),
        status: Some(403),
        html: &[
            "access to this page has been denied",
            "perimeterx",
            "px-captcha",
        ],
        unless_open_graph: false,
    },
    AntibotProvider {
        name: "akamai",
        technique: AntibotTechnique::Waf,
        header: Some("server"),
        header_value: Some("akamaighost"),
        cookie: Some("ak_bmsc"),
        status: Some(403),
        html: &["access denied", "akamai"],
        unless_open_graph: false,
    },
    AntibotProvider {
        name: "imperva",
        technique: AntibotTechnique::Waf,
        header: Some("x-iinfo"),
        header_value: None,
        cookie: Some("visid_incap"),
        status: Some(403),
        html: &["incapsula", "request unsuccessful"],
        unless_open_graph: false,
    },
    AntibotProvider {
        name: "recaptcha",
        technique: AntibotTechnique::Captcha,
        header: None,
        header_value: None,
        cookie: None,
        status: None,
        html: &[
            "g-recaptcha",
            "www.google.com/recaptcha",
            "recaptcha/api.js",
        ],
        unless_open_graph: false,
    },
    AntibotProvider {
        name: "hcaptcha",
        technique: AntibotTechnique::Captcha,
        header: None,
        header_value: None,
        cookie: None,
        status: None,
        html: &["h-captcha", "hcaptcha.com"],
        unless_open_graph: false,
    },
    AntibotProvider {
        name: "aws-waf",
        technique: AntibotTechnique::Javascript,
        header: None,
        header_value: None,
        cookie: Some("aws-waf-token"),
        status: Some(202),
        html: &["awswaf", "challenge.js", "captcha.js"],
        unless_open_graph: true,
    },
    AntibotProvider {
        name: "kasada",
        technique: AntibotTechnique::Javascript,
        header: Some("x-kpsdk-ct"),
        header_value: None,
        cookie: None,
        status: Some(403),
        html: &["kasada"],
        unless_open_graph: false,
    },
];

/// Señales de la respuesta del fetch sobre las que corre la detección. Se construyen en la capa
/// de infraestructura a partir del `reqwest::Response` (headers, cookies, status, body ya
/// decodificado y acotado).
#[derive(Debug, Clone)]
pub struct ResponseSignals<'a> {
    pub status: u16,
    /// Headers en minúscula (nombre → primer valor).
    pub headers: &'a [(String, String)],
    /// Nombres de cookie presentes en `Set-Cookie` / `Cookie`.
    pub cookies: &'a [String],
    /// HTML (o texto) ya decodificado y acotado a 2 MB; en minúscula para el matching.
    pub body_lowercase: &'a str,
    /// `true` si la respuesta trae OpenGraph completo (og:title + og:image o og:description).
    pub has_open_graph: bool,
}

/// Resultado de la detección. `detected=false` con `provider=None` es el caso sano.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AntibotInfo {
    pub detected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub technique: Option<String>,
}

impl AntibotInfo {
    pub fn none() -> Self {
        Self::default()
    }
}

/// Busca el primer proveedor cuya señal coincida. Devuelve `None` cuando la respuesta es limpia
/// o cuando la única señal viene de un proveedor marcado `unless_open_graph` y la respuesta trae
/// OG completos (el contenido real está ahí, no es un desafío).
pub fn detect(signals: &ResponseSignals<'_>) -> AntibotInfo {
    for provider in ANTIBOT_PROVIDERS {
        if matches(provider, signals) {
            if provider.unless_open_graph && signals.has_open_graph {
                continue;
            }
            return AntibotInfo {
                detected: true,
                provider: Some(provider.name.to_string()),
                technique: Some(provider.technique.as_str().to_string()),
            };
        }
    }
    AntibotInfo::none()
}

fn matches(provider: &AntibotProvider, signals: &ResponseSignals<'_>) -> bool {
    if let Some(name) = provider.header {
        let name_lc = name.to_ascii_lowercase();
        for (h, v) in signals.headers {
            if h == &name_lc {
                match provider.header_value {
                    Some(expected) => {
                        if v.to_ascii_lowercase()
                            .contains(&expected.to_ascii_lowercase())
                        {
                            return true;
                        }
                    }
                    None => return true,
                }
            }
        }
    }

    if let Some(cookie) = provider.cookie {
        let cookie_lc = cookie.to_ascii_lowercase();
        if signals
            .cookies
            .iter()
            .any(|c| c.to_ascii_lowercase() == cookie_lc)
        {
            return true;
        }
    }

    if let Some(status) = provider.status {
        // El status por sí solo es señal débil: solo cuenta si además coincide una señal de html.
        // `!provider.html.is_empty()` no hace falta: `.any()` sobre un html vacío ya es `false`.
        if status == signals.status
            && provider
                .html
                .iter()
                .any(|needle| signals.body_lowercase.contains(needle))
        {
            return true;
        }
    }

    provider
        .html
        .iter()
        .any(|needle| signals.body_lowercase.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals<'a>(
        status: u16,
        headers: &'a [(String, String)],
        cookies: &'a [String],
        body: &'a str,
        og: bool,
    ) -> ResponseSignals<'a> {
        ResponseSignals {
            status,
            headers,
            cookies,
            body_lowercase: body,
            has_open_graph: og,
        }
    }

    #[test]
    fn respuesta_limpia_no_detecta() {
        let s = signals(200, &[], &[], "<html><body>hola</body></html>", false);
        assert!(!detect(&s).detected);
    }

    #[test]
    fn cloudflare_por_html_y_status() {
        let headers = [("server".to_string(), "cloudflare".to_string())];
        let s = signals(
            503,
            &headers,
            &[],
            "<title>attention required! | cloudflare</title>",
            false,
        );
        let info = detect(&s);
        assert!(info.detected);
        assert_eq!(info.provider.as_deref(), Some("cloudflare"));
        assert_eq!(info.technique.as_deref(), Some("javascript"));
    }

    #[test]
    fn unless_open_graph_silencia_cuando_hay_og() {
        // Misma señal de Cloudflare, pero con OG completos: contenido real presente.
        let headers = [("server".to_string(), "cloudflare".to_string())];
        let s = signals(
            503,
            &headers,
            &[],
            "checking your browser before accessing",
            true,
        );
        assert!(!detect(&s).detected);
    }

    #[test]
    fn recaptcha_por_html_sin_og() {
        let s = signals(
            200,
            &[],
            &[],
            "<div class=\"g-recaptcha\" data-sitekey=\"x\"></div>",
            false,
        );
        let info = detect(&s);
        assert!(info.detected);
        assert_eq!(info.provider.as_deref(), Some("recaptcha"));
        assert_eq!(info.technique.as_deref(), Some("captcha"));
    }

    #[test]
    fn cookie_datadome_detecta() {
        let cookies = ["datadome".to_string()];
        let s = signals(200, &[], &cookies, "<html>cuerpo normal</html>", false);
        assert_eq!(detect(&s).provider.as_deref(), Some("datadome"));
    }

    #[test]
    fn status_solo_no_es_suficiente_sin_html() {
        // 403 de Datadome pero sin ninguna otra señal: no se marca.
        let s = signals(403, &[], &[], "<html>ok</html>", false);
        assert!(!detect(&s).detected);
    }
}
