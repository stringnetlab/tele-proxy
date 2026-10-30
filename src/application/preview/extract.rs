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

/// Un recurso binario (imagen/video/audio servido directo) no expone metadatos embebidos sin
/// decodificar el contenedor: se reporta la URL, el MIME y el tamaño. Para imágenes se añaden las
/// dimensiones leyendo la cabecera del fichero (sin decodificar nada).
fn direct_media(fetched: &FetchedResponse) -> MediaInfo {
    let mut media = MediaInfo::with_url(fetched.final_url.clone());
    media.mime = fetched
        .content_type
        .as_ref()
        .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_string());
    media.size = Some(fetched.body.len() as u64);
    if matches!(media.mime.as_deref(), Some(mime) if mime.starts_with("image/")) {
        if let Some((width, height)) = sniff_dimensions(&fetched.body) {
            media.width = Some(width);
            media.height = Some(height);
        }
    }
    media.with_derived_size_pretty()
}

/// Dimensiones `(ancho, alto)` de PNG, GIF, JPEG y WebP leídas de los primeros bytes del fichero.
/// No decodifica la imagen: es O(1) y un fichero truncado o de otro formato devuelve `None` en
/// lugar de fallar. SVG (dimensiones en atributos XML) queda fuera: se reporta sin medidas.
fn sniff_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_dimensions(bytes);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return gif_dimensions(bytes);
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return jpeg_dimensions(bytes);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return webp_dimensions(bytes);
    }
    None
}

/// Firma (8) + longitud (4) + `IHDR` (4) + ancho (4 BE) + alto (4 BE).
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    nonzero(be_u32(bytes, 16)?, be_u32(bytes, 20)?)
}

/// Cabecera (6) + ancho (2 LE) + alto (2 LE).
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    nonzero(le_u16(bytes, 6)? as u32, le_u16(bytes, 8)? as u32)
}

/// Recorre los segmentos JPEG hasta el `SOFn`, que es donde viven las dimensiones. Se detiene en
/// `SOS`: a partir de ahí el flujo entrópico puede contener bytes `0xFF` y ya no hay cabeceras.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // tras el SOI
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let mut marker = bytes[i + 1];
        i += 2;
        // Bytes de relleno 0xFF entre segmentos.
        while marker == 0xFF {
            marker = *bytes.get(i)?;
            i += 1;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        let segment_len = be_u16(bytes, i)? as usize;
        if segment_len < 2 {
            return None;
        }
        if is_sof(marker) {
            // Precisión (1) + alto (2 BE) + ancho (2 BE).
            let height = be_u16(bytes, i + 3)?;
            let width = be_u16(bytes, i + 5)?;
            return nonzero(width as u32, height as u32);
        }
        i += segment_len;
    }
    None
}

fn is_sof(marker: u8) -> bool {
    matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
}

/// Primer chunk WebP: `VP8 ` (con pérdida), `VP8L` (sin pérdida) o `VP8X` (extendido).
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes.get(12..16)? {
        b"VP8 " => {
            if bytes.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            nonzero(
                le_u16(bytes, 26)? as u32 & 0x3FFF,
                le_u16(bytes, 28)? as u32 & 0x3FFF,
            )
        }
        b"VP8L" => {
            if *bytes.get(20)? != 0x2F {
                return None;
            }
            let b = bytes.get(21..25)?;
            let width = (b[0] as u32 | ((b[1] as u32 & 0x3F) << 8)) + 1;
            let height = ((b[1] as u32 >> 6) | (b[2] as u32) << 2 | (b[3] as u32 & 0x0F) << 10) + 1;
            nonzero(width, height)
        }
        b"VP8X" => {
            // Banderas (4) + ancho-1 (3 LE) + alto-1 (3 LE).
            nonzero(le_u24(bytes, 24)? + 1, le_u24(bytes, 27)? + 1)
        }
        _ => None,
    }
}

fn nonzero(width: u32, height: u32) -> Option<(u32, u32)> {
    (width > 0 && height > 0).then_some((width, height))
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn le_u24(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(3)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], 0]))
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

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[0x08, 0x06, 0x00, 0x00, 0x00]);
        b
    }

    fn gif_bytes(width: u16, height: u16) -> Vec<u8> {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        b.extend_from_slice(&[0x00, 0x00, 0x00]);
        b
    }

    fn jpeg_bytes(width: u16, height: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        b.extend_from_slice(&[0u8; 14]); // payload APP0
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&[0x03, 0x01, 0x11, 0x00]);
        b
    }

    fn webp_vp8x_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut b = b"RIFF\x00\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00\x00\x00\x00\x00".to_vec();
        for v in [width - 1, height - 1] {
            b.extend_from_slice(&v.to_le_bytes()[..3]);
        }
        b
    }

    fn webp_vp8l_bytes(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width - 1, height - 1);
        let mut b = b"RIFF\x00\x00\x00\x00WEBPVP8L\x00\x00\x00\x00\x2f".to_vec();
        b.push((w & 0xFF) as u8);
        b.push((((w >> 8) & 0x3F) | ((h & 0x03) << 6)) as u8);
        b.push(((h >> 2) & 0xFF) as u8);
        b.push(((h >> 10) & 0x0F) as u8);
        b
    }

    fn webp_vp8_bytes(width: u16, height: u16) -> Vec<u8> {
        let mut b = b"RIFF\x00\x00\x00\x00WEBPVP8 \x00\x00\x00\x00".to_vec();
        b.extend_from_slice(&[0x00, 0x00, 0x00]); // frame tag
        b.extend_from_slice(&[0x9D, 0x01, 0x2A]); // start code
        b.extend_from_slice(&(width & 0x3FFF).to_le_bytes());
        b.extend_from_slice(&(height & 0x3FFF).to_le_bytes());
        b
    }

    #[test]
    fn sniff_dimensiones_png() {
        assert_eq!(sniff_dimensions(&png_bytes(1200, 630)), Some((1200, 630)));
    }

    #[test]
    fn sniff_dimensiones_gif() {
        assert_eq!(sniff_dimensions(&gif_bytes(320, 240)), Some((320, 240)));
    }

    #[test]
    fn sniff_dimensiones_jpeg_tras_segmento_app() {
        assert_eq!(sniff_dimensions(&jpeg_bytes(800, 600)), Some((800, 600)));
    }

    #[test]
    fn sniff_dimensiones_webp_en_sus_tres_variantes() {
        assert_eq!(
            sniff_dimensions(&webp_vp8_bytes(640, 480)),
            Some((640, 480))
        );
        assert_eq!(
            sniff_dimensions(&webp_vp8l_bytes(300, 200)),
            Some((300, 200))
        );
        assert_eq!(
            sniff_dimensions(&webp_vp8x_bytes(1920, 1080)),
            Some((1920, 1080))
        );
    }

    #[test]
    fn sniff_dimensiones_de_bytes_cortos_o_desconocidos_es_none() {
        for body in [
            &b""[..],
            b"\x89PNG\r\n\x1a\n",
            b"GIF89a\x40",
            b"\xFF\xD8\xFF\xE0",
            b"RIFF\x00\x00\x00\x00WEBP",
            b"no es una imagen",
        ] {
            assert_eq!(sniff_dimensions(body), None, "{body:?}");
        }
    }

    #[test]
    fn imagen_directa_rellena_width_y_height() {
        let f = fetched(200, Some("image/png"), &png_bytes(1200, 630));
        let data = build_preview(&f, &PreviewConfig::default(), false);
        let image = data.image.expect("media directa");
        assert_eq!(image.width, Some(1200));
        assert_eq!(image.height, Some(630));
    }

    #[test]
    fn imagen_directa_sin_cabecera_reconocible_no_inventa_dimensiones() {
        let f = fetched(200, Some("image/png"), &[0u8; 2048]);
        let data = build_preview(&f, &PreviewConfig::default(), false);
        let image = data.image.expect("media directa");
        assert_eq!(image.width, None);
        assert_eq!(image.height, None);
    }

    #[test]
    fn video_directo_no_intenta_sniffing() {
        let f = fetched(200, Some("video/mp4"), &png_bytes(1200, 630));
        let data = build_preview(&f, &PreviewConfig::default(), false);
        let video = data.video.expect("media directa");
        assert_eq!(video.width, None);
        assert_eq!(video.height, None);
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
