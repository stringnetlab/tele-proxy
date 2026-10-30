use scraper::Html;
use url::Url;

use crate::domain::preview::antibot::{detect, ResponseSignals};
use crate::domain::preview::config::PreviewConfig;
use crate::domain::preview::media::MediaInfo;
use crate::domain::preview::result::PreviewData;
use crate::infrastructure::preview::fetch::FetchedResponse;
use crate::infrastructure::preview::parse_html::decode_html;

use super::{cascade, content};

/// Naturaleza del recurso por `Content-Type` (D3, `detect.rs` del plan). Decide si se parsea HTML o
/// se devuelve un `MediaInfo` directo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Html,
    Image,
    Video,
    Audio,
    Other,
}

impl ContentKind {
    pub fn from_content_type(content_type: Option<&str>) -> Self {
        let Some(ct) = content_type else {
            return Self::Other;
        };
        let essence = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match essence.as_str() {
            "text/html" | "application/xhtml+xml" => Self::Html,
            _ if essence.starts_with("image/") => Self::Image,
            _ if essence.starts_with("video/") => Self::Video,
            _ if essence.starts_with("audio/") => Self::Audio,
            _ => Self::Other,
        }
    }
}

/// Construye el `PreviewData` a partir de la respuesta ya descargada. `want_content` activa la
/// extracción de texto/markdown (capacidad `content`), que es la más cara y solo corre sobre HTML.
///
/// No hace I/O ni falla: todo el trabajo de red y validación ocurrió en el fetch. Un `Content-Type`
/// binario devuelve un `MediaInfo` directo; el HTML pasa por la cascada de detectores.
pub fn build_preview(
    fetched: &FetchedResponse,
    _config: &PreviewConfig,
    want_content: bool,
) -> PreviewData {
    let kind = ContentKind::from_content_type(fetched.content_type.as_deref());
    let base_url = Url::parse(&fetched.final_url);

    let mut data = match (kind, base_url) {
        (ContentKind::Html, Ok(base)) => build_from_html(fetched, &base, want_content),
        (ContentKind::Image, _) => PreviewData {
            url: Some(fetched.final_url.clone()),
            image: Some(direct_media(fetched)),
            ..Default::default()
        },
        (ContentKind::Video, _) => PreviewData {
            url: Some(fetched.final_url.clone()),
            video: Some(direct_media(fetched)),
            ..Default::default()
        },
        (ContentKind::Audio, _) => PreviewData {
            url: Some(fetched.final_url.clone()),
            audio: Some(direct_media(fetched)),
            ..Default::default()
        },
        _ => PreviewData {
            url: Some(fetched.final_url.clone()),
            ..Default::default()
        },
    };

    data.antibot = detect_antibot(fetched, &data);
    data
}

fn build_from_html(fetched: &FetchedResponse, base_url: &Url, want_content: bool) -> PreviewData {
    let decoded = decode_html(&fetched.body, fetched.content_type.as_deref());
    let doc = Html::parse_document(&decoded);

    let mut data = cascade::run(&doc, base_url);

    if want_content {
        let extracted = content::extract(&decoded, base_url);
        if !extracted.is_empty() {
            data.content = Some(extracted);
        }
    }

    data
}

/// Un recurso binario (imagen/video/audio servido directo) no tiene metadatos embebidos que se
/// puedan leer sin decodificar el contenedor: se reporta la URL y el MIME, y nada más.
fn direct_media(fetched: &FetchedResponse) -> MediaInfo {
    let mut media = MediaInfo::with_url(fetched.final_url.clone());
    media.mime = fetched
        .content_type
        .as_ref()
        .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_string());
    media.size = Some(fetched.body.len() as u64);
    media.with_derived_size_pretty()
}

/// Señales para la detección antibot. `has_open_graph` sale del `PreviewData` ya construido: si hay
/// OG completos (título + imagen o descripción), el contenido real está presente y los proveedores
/// marcados `unless_open_graph` no deben marcar la respuesta como desafío.
fn detect_antibot(
    fetched: &FetchedResponse,
    data: &PreviewData,
) -> crate::domain::preview::antibot::AntibotInfo {
    let body_lowercase = String::from_utf8_lossy(&fetched.body).to_ascii_lowercase();

    let has_open_graph = data
        .open_graph
        .as_ref()
        .map(|og| {
            og.values.contains_key("title")
                && (og.values.contains_key("image") || og.values.contains_key("description"))
        })
        .unwrap_or(false);

    let signals = ResponseSignals {
        status: fetched.status,
        headers: &fetched.headers,
        cookies: &fetched.cookies,
        body_lowercase: &body_lowercase,
        has_open_graph,
    };
    detect(&signals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::preview::config::PreviewConfig;

    fn fetched(status: u16, content_type: Option<&str>, body: &[u8]) -> FetchedResponse {
        FetchedResponse {
            status,
            final_url: "https://example.com/x".to_string(),
            content_type: content_type.map(|s| s.to_string()),
            headers: Vec::new(),
            cookies: Vec::new(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn content_kind_por_essence() {
        assert_eq!(
            ContentKind::from_content_type(Some("text/html; charset=utf-8")),
            ContentKind::Html
        );
        assert_eq!(
            ContentKind::from_content_type(Some("application/xhtml+xml")),
            ContentKind::Html
        );
        assert_eq!(
            ContentKind::from_content_type(Some("image/png")),
            ContentKind::Image
        );
        assert_eq!(
            ContentKind::from_content_type(Some("video/mp4")),
            ContentKind::Video
        );
        assert_eq!(
            ContentKind::from_content_type(Some("application/pdf")),
            ContentKind::Other
        );
        assert_eq!(ContentKind::from_content_type(None), ContentKind::Other);
    }

    #[test]
    fn imagen_directa_devuelve_media_con_size() {
        let f = fetched(200, Some("image/png"), &[0u8; 2048]);
        let data = build_preview(&f, &PreviewConfig::default(), true);
        let image = data.image.expect("media directa");
        assert_eq!(image.mime.as_deref(), Some("image/png"));
        assert_eq!(image.size, Some(2048));
        assert_eq!(image.size_pretty.as_deref(), Some("2 KB"));
        assert!(
            data.content.is_none(),
            "una imagen no tiene contenido de artículo"
        );
    }

    #[test]
    fn html_con_content_extrae_texto() {
        let html = r#"<html lang="es"><head><title>Art</title>
        <meta property="og:title" content="OG"></head><body>
        <article><p>Un párrafo suficientemente largo para que readability lo tome como contenido
        principal del artículo y no lo descarte por ser demasiado breve o de navegación.</p>
        <p>Otro párrafo más para dar densidad al bloque principal y asegurar la extracción.</p></article>
        </body></html>"#;
        let f = fetched(200, Some("text/html; charset=utf-8"), html.as_bytes());
        let data = build_preview(&f, &PreviewConfig::default(), true);
        assert_eq!(data.title.as_deref(), Some("OG"));
        assert!(data.content.is_some(), "content pedido → se extrae");
        assert!(!data.antibot.detected);
    }

    #[test]
    fn sin_content_no_se_extrae_texto() {
        let html = "<html><head><title>t</title></head><body><p>texto</p></body></html>";
        let f = fetched(200, Some("text/html"), html.as_bytes());
        let data = build_preview(&f, &PreviewConfig::default(), false);
        assert!(data.content.is_none());
    }

    #[test]
    fn desafio_cloudflare_se_detecta() {
        let mut f = fetched(
            503,
            Some("text/html"),
            b"checking your browser before accessing",
        );
        f.headers = vec![("server".to_string(), "cloudflare".to_string())];
        let data = build_preview(&f, &PreviewConfig::default(), true);
        assert!(data.antibot.detected);
        assert_eq!(data.antibot.provider.as_deref(), Some("cloudflare"));
    }

    /// Fixture "golden": un artículo realista que ejercita la cascada completa de una vez. Cada
    /// campo está declarado en varias fuentes a propósito y con valores distintos, para fijar la
    /// precedencia (OG gana a JSON-LD y al DOM) y la resolución de URLs relativas a absolutas.
    /// `author`/`date`/`publisher` NO llevan `<meta>`: solo existen en el JSON-LD, así se prueba
    /// también el salto de la cascada hacia el detector estructurado.
    #[test]
    fn articulo_golden_recorre_toda_la_cascada() {
        let html = r#"<html lang="es"><head>
        <title>Título DOM (debe perder)</title>
        <meta property="og:title" content="Título del artículo">
        <meta property="og:description" content="Descripción corta del artículo.">
        <meta property="og:image" content="/img/og.png">
        <meta property="og:site_name" content="Example">
        <meta property="og:type" content="article">
        <meta name="twitter:card" content="summary_large_image">
        <meta name="twitter:title" content="Título Twitter (debe perder)">
        <link rel="icon" href="/favicon.png" type="image/png" sizes="32x32">
        <script type="application/ld+json">
        {"@context":"https://schema.org","@type":"Article",
         "headline":"Título JSON-LD (debe perder)",
         "author":{"@type":"Person","name":"Ana Pérez"},
         "datePublished":"2026-01-15T09:00:00Z",
         "publisher":{"@type":"Organization","name":"Example Media"}}
        </script>
        </head><body>
        <article><h1>Título del artículo</h1>
        <p>Primer párrafo suficientemente largo para que el extractor de contenido lo reconozca
        como el cuerpo principal del artículo y no lo descarte por ser breve o de navegación.</p>
        <p>Segundo párrafo que aporta densidad al bloque principal, con varias oraciones que
        describen el tema tratado y refuerzan la señal de que aquí está el contenido real.</p>
        <p>Tercer párrafo de cierre que resume lo expuesto y garantiza que el artículo supere el
        umbral mínimo de texto que exige la extracción para no devolver un resultado vacío.</p>
        </article>
        </body></html>"#;
        let f = fetched(200, Some("text/html; charset=utf-8"), html.as_bytes());
        let data = build_preview(&f, &PreviewConfig::default(), true);

        assert_eq!(data.lang.as_deref(), Some("es"));
        assert_eq!(
            data.title.as_deref(),
            Some("Título del artículo"),
            "og:title gana al <title> del DOM y al headline del JSON-LD"
        );
        assert_eq!(
            data.description.as_deref(),
            Some("Descripción corta del artículo.")
        );
        assert_eq!(data.site_name.as_deref(), Some("Example"));
        assert_eq!(
            data.author.as_deref(),
            Some("Ana Pérez"),
            "sin <meta author>, la cascada cae al author del JSON-LD"
        );
        assert_eq!(data.date.as_deref(), Some("2026-01-15T09:00:00Z"));
        assert_eq!(data.publisher.as_deref(), Some("Example Media"));

        let image = data.image.expect("og:image relativo → MediaInfo absoluto");
        assert_eq!(
            image.url.as_deref(),
            Some("https://example.com/img/og.png"),
            "el og:image relativo se resuelve contra la URL final"
        );
        assert_eq!(image.mime.as_deref(), Some("image/png"));

        let og = data.open_graph.expect("bloque OpenGraph presente");
        assert_eq!(
            og.values.get("title").map(String::as_str),
            Some("Título del artículo")
        );
        let tw = data.twitter.expect("bloque Twitter presente");
        assert_eq!(
            tw.values.get("card").map(String::as_str),
            Some("summary_large_image")
        );
        let json_ld = data.json_ld.expect("JSON-LD presente");
        assert!(!json_ld.is_empty());

        let favicons = data.favicons.expect("<link rel=icon> → favicons");
        assert_eq!(
            favicons.first().and_then(|f| f.url.as_deref()),
            Some("https://example.com/favicon.png")
        );

        let content = data.content.expect("content pedido → se extrae");
        assert!(
            content
                .text
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty()),
            "el texto extraído no está vacío"
        );
        assert!(!data.antibot.detected, "un artículo 200 no es un desafío");
    }
}
