use std::collections::HashMap;

use scraper::{Html, Selector};
use url::Url;

use crate::domain::preview::media::MediaInfo;
use crate::domain::preview::result::{MetaMap, PreviewData};

/// Cascada de detectores por campo (D4, sin oEmbed en esta entrega). Orden de precedencia por
/// campo: **OpenGraph/Twitter → JSON-LD → `<meta>` genéricos → DOM → content-type**. Cada campo se
/// resuelve de forma independiente: si OG no trae título se cae al `<title>` del DOM, etc.
///
/// Todas las URLs se resuelven contra `base_url` (la URL final tras redirects) para que un
/// `og:image` relativo salga absoluto.
pub fn run(doc: &Html, base_url: &Url) -> PreviewData {
    let og = collect_prefixed_meta(doc, "property", "og:");
    let twitter = collect_prefixed_meta(doc, "name", "twitter:");
    let json_ld = collect_json_ld(doc);

    let mut data = PreviewData {
        url: Some(base_url.to_string()),
        open_graph: some_if_not_empty(og.clone()),
        twitter: some_if_not_empty(twitter.clone()),
        json_ld: if json_ld.is_empty() {
            None
        } else {
            Some(json_ld.clone())
        },
        ..Default::default()
    };

    data.lang = html_lang(doc);
    data.title = first_non_empty(vec![
        og.get("title").cloned(),
        twitter.get("title").cloned(),
        json_ld_string(&json_ld, &["headline", "name"]),
        meta_content(doc, &["title", "og:title"]),
        dom_title(doc),
    ]);
    data.description = first_non_empty(vec![
        og.get("description").cloned(),
        twitter.get("description").cloned(),
        json_ld_string(&json_ld, &["description"]),
        meta_content(
            doc,
            &["description", "og:description", "twitter:description"],
        ),
    ]);
    data.site_name = first_non_empty(vec![
        og.get("site_name").cloned(),
        meta_content(doc, &["application-name", "og:site_name"]),
    ]);
    data.author = first_non_empty(vec![
        meta_content(doc, &["author", "article:author", "og:article:author"]),
        json_ld_string(&json_ld, &["author"]),
    ]);
    data.date = first_non_empty(vec![
        meta_content(
            doc,
            &[
                "article:published_time",
                "date",
                "pubdate",
                "publishdate",
                "og:updated_time",
            ],
        ),
        json_ld_string(&json_ld, &["datePublished", "dateCreated"]),
    ]);
    data.publisher = first_non_empty(vec![
        meta_content(doc, &["publisher", "article:publisher"]),
        json_ld_string(&json_ld, &["publisher"]),
    ]);

    data.image = detect_image(doc, base_url, &og, &twitter, &json_ld);
    data.logo = detect_logo(doc, base_url, &json_ld);
    data.video = detect_video(base_url, &og, &twitter);
    data.audio = detect_audio(base_url, &og);
    data.favicons = detect_favicons(doc, base_url);

    data
}

fn some_if_not_empty(map: HashMap<String, String>) -> Option<MetaMap> {
    if map.is_empty() {
        None
    } else {
        Some(MetaMap { values: map })
    }
}

fn first_non_empty(candidates: Vec<Option<String>>) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
}

/// Recoge `<meta {attr}="{prefix}*" content="...">` en un mapa con el prefijo retirado. `attr` es
/// `property` para OpenGraph y `name` para Twitter Cards.
fn collect_prefixed_meta(doc: &Html, attr: &str, prefix: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let selector = Selector::parse("meta").expect("selector `meta` válido");
    for element in doc.select(&selector) {
        let value = element.value();
        let Some(key) = value.attr(attr) else {
            continue;
        };
        let Some(stripped) = key.strip_prefix(prefix) else {
            continue;
        };
        if let Some(content) = value.attr("content") {
            // La primera declaración gana (orden de precedencia del documento).
            out.entry(stripped.to_string())
                .or_insert_with(|| content.to_string());
        }
    }
    out
}

/// `<meta name="{cualquiera de names}" content="...">`, case-insensitive sobre el name.
fn meta_content(doc: &Html, names: &[&str]) -> Option<String> {
    let selector = Selector::parse("meta").expect("selector `meta` válido");
    for element in doc.select(&selector) {
        let value = element.value();
        let Some(name) = value.attr("name").or_else(|| value.attr("property")) else {
            continue;
        };
        let name_lc = name.to_ascii_lowercase();
        if names.iter().any(|n| n.to_ascii_lowercase() == name_lc) {
            if let Some(content) = value.attr("content") {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

fn dom_title(doc: &Html) -> Option<String> {
    let selector = Selector::parse("title").expect("selector `title` válido");
    doc.select(&selector)
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn html_lang(doc: &Html) -> Option<String> {
    let selector = Selector::parse("html").expect("selector `html` válido");
    doc.select(&selector)
        .next()
        .and_then(|h| h.value().attr("lang"))
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

/// Parsea los `<script type="application/ld+json">`. Devuelve la lista plana de objetos (los
/// `@graph` se aplanan) para que `json_ld_string` busque en todos.
fn collect_json_ld(doc: &Html) -> Vec<serde_json::Value> {
    let selector = Selector::parse("script[type=\"application/ld+json\"]")
        .expect("selector de json-ld válido");
    let mut out = Vec::new();
    for element in doc.select(&selector) {
        let raw: String = element.text().collect();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
            continue;
        };
        flatten_json_ld(value, &mut out);
    }
    out
}

fn flatten_json_ld(value: serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                flatten_json_ld(item, out);
            }
        }
        serde_json::Value::Object(mut map) => {
            if let Some(graph) = map.remove("@graph") {
                // El contenedor puede traer más claves junto a `@graph`; solo se emite si aporta
                // algo (un `{}` vacío tras retirar el graph es ruido).
                if !map.is_empty() {
                    out.push(serde_json::Value::Object(map));
                }
                flatten_json_ld(graph, out);
            } else {
                out.push(serde_json::Value::Object(map));
            }
        }
        other => out.push(other),
    }
}

/// Busca la primera clave presente en cualquier objeto JSON-LD. La iteración es **por clave**
/// (prioridad) y dentro de cada clave por nodo: así `headline` gana a `name` aunque el nodo con
/// `name` (p.ej. un `Person` de un `@graph`) aparezca antes. Los valores anidados (p.ej. `author`
/// como objeto `{name}`) se reducen a su `name`/`url` cuando existen.
fn json_ld_string(nodes: &[serde_json::Value], keys: &[&str]) -> Option<String> {
    for key in keys {
        for node in nodes {
            if let Some(found) = node.get(*key) {
                if let Some(s) = json_value_to_string(found) {
                    return Some(s);
                }
            }
        }
    }
    None
}

fn json_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Object(map) => map
            .get("name")
            .or_else(|| map.get("url"))
            .and_then(json_value_to_string),
        serde_json::Value::Array(items) => items.iter().find_map(json_value_to_string),
        _ => None,
    }
}

fn detect_image(
    doc: &Html,
    base_url: &Url,
    og: &HashMap<String, String>,
    twitter: &HashMap<String, String>,
    json_ld: &[serde_json::Value],
) -> Option<MediaInfo> {
    let url = first_non_empty(vec![
        og.get("image").or_else(|| og.get("image:url")).cloned(),
        og.get("image:secure_url").cloned(),
        twitter
            .get("image")
            .or_else(|| twitter.get("image:src"))
            .cloned(),
        json_ld_string(json_ld, &["image", "thumbnailUrl"]),
        meta_content(doc, &["image", "thumbnail"]),
        first_img_src(doc),
    ])?;

    let absolute = resolve(base_url, &url)?;
    let mut media = MediaInfo::with_url(absolute.clone());

    if let Some(w) = og.get("image:width").and_then(|v| v.parse::<u32>().ok()) {
        media.width = Some(w);
    }
    if let Some(h) = og.get("image:height").and_then(|v| v.parse::<u32>().ok()) {
        media.height = Some(h);
    }
    if media.mime.is_none() {
        media.mime = mime_from_url(&absolute);
    }
    Some(media.with_derived_size_pretty())
}

fn first_img_src(doc: &Html) -> Option<String> {
    let selector = Selector::parse("img[src]").expect("selector `img[src]` válido");
    doc.select(&selector)
        .next()
        .and_then(|i| i.value().attr("src"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn detect_logo(doc: &Html, base_url: &Url, json_ld: &[serde_json::Value]) -> Option<MediaInfo> {
    let url = first_non_empty(vec![
        link_rel_href(doc, &["logo"]),
        json_ld_string(json_ld, &["logo"]),
    ])?;
    let absolute = resolve(base_url, &url)?;
    let mut media = MediaInfo::with_url(absolute.clone());
    media.mime = mime_from_url(&absolute);
    Some(media.with_derived_size_pretty())
}

fn detect_video(
    base_url: &Url,
    og: &HashMap<String, String>,
    twitter: &HashMap<String, String>,
) -> Option<MediaInfo> {
    let url = first_non_empty(vec![
        og.get("video").or_else(|| og.get("video:url")).cloned(),
        og.get("video:secure_url").cloned(),
        twitter.get("player").cloned(),
    ])?;
    let absolute = resolve(base_url, &url)?;
    let mut media = MediaInfo::with_url(absolute.clone());
    media.mime = og
        .get("video:type")
        .cloned()
        .or_else(|| mime_from_url(&absolute));
    if let Some(w) = og.get("video:width").and_then(|v| v.parse::<u32>().ok()) {
        media.width = Some(w);
    }
    if let Some(h) = og.get("video:height").and_then(|v| v.parse::<u32>().ok()) {
        media.height = Some(h);
    }
    if let Some(d) = og.get("video:duration").and_then(|v| v.parse::<f64>().ok()) {
        media.duration = Some(d);
    }
    Some(media.with_derived_size_pretty())
}

fn detect_audio(base_url: &Url, og: &HashMap<String, String>) -> Option<MediaInfo> {
    let url = first_non_empty(vec![
        og.get("audio").or_else(|| og.get("audio:url")).cloned(),
        og.get("audio:secure_url").cloned(),
    ])?;
    let absolute = resolve(base_url, &url)?;
    let mut media = MediaInfo::with_url(absolute.clone());
    media.mime = og
        .get("audio:type")
        .cloned()
        .or_else(|| mime_from_url(&absolute));
    Some(media.with_derived_size_pretty())
}

/// `<link rel="icon|shortcut icon|apple-touch-icon|mask-icon">`. Con fallback al `/favicon.ico`
/// del origen de `base_url` cuando el HTML no declara ninguno (comportamiento `link-preview-js`).
fn detect_favicons(doc: &Html, base_url: &Url) -> Option<Vec<MediaInfo>> {
    let selector = Selector::parse("link[rel]").expect("selector `link[rel]` válido");
    let mut out: Vec<MediaInfo> = Vec::new();
    for element in doc.select(&selector) {
        let value = element.value();
        let rel = value.attr("rel")?.to_ascii_lowercase();
        let is_icon = ["icon", "shortcut icon", "apple-touch-icon", "mask-icon"]
            .iter()
            .any(|want| rel.split_whitespace().any(|token| token == *want));
        if !is_icon {
            continue;
        }
        let Some(href) = value.attr("href") else {
            continue;
        };
        let Some(absolute) = resolve(base_url, href) else {
            continue;
        };
        let mut media = MediaInfo::with_url(absolute.clone());
        media.mime = value
            .attr("type")
            .map(|t| t.to_string())
            .or_else(|| mime_from_url(&absolute));
        if let Some(sizes) = value.attr("sizes") {
            if let Some((w, h)) = parse_sizes(sizes) {
                media.width = Some(w);
                media.height = Some(h);
            }
        }
        out.push(media.with_derived_size_pretty());
    }

    if out.is_empty() {
        let fallback = base_url
            .join("/favicon.ico")
            .ok()
            .map(|u| MediaInfo::with_url(u.to_string()))?;
        return Some(vec![fallback]);
    }
    Some(out)
}

fn link_rel_href(doc: &Html, rels: &[&str]) -> Option<String> {
    let selector = Selector::parse("link[rel]").expect("selector `link[rel]` válido");
    for element in doc.select(&selector) {
        let value = element.value();
        let rel = value.attr("rel")?.to_ascii_lowercase();
        if rels
            .iter()
            .any(|want| rel.split_whitespace().any(|t| t == *want))
        {
            if let Some(href) = value.attr("href") {
                let trimmed = href.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

/// `sizes="192x192"` → `(192, 192)`.
fn parse_sizes(sizes: &str) -> Option<(u32, u32)> {
    let first = sizes.split_whitespace().next()?;
    let (w, h) = first.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Resuelve una URL (posiblemente relativa) contra la base. Devuelve `None` si no es una URL
/// absoluta http/https utilizable (p.ej. `data:` o un relativo roto).
fn resolve(base_url: &Url, candidate: &str) -> Option<String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return None;
    }
    let absolute = base_url.join(trimmed).ok()?;
    if !matches!(absolute.scheme(), "http" | "https") {
        return None;
    }
    Some(absolute.to_string())
}

/// MIME por sufijo del path (best-effort; el detector no hace fetch de la imagen).
fn mime_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/articulo/1").unwrap()
    }

    #[test]
    fn og_gana_sobre_el_dom() {
        let html = Html::parse_document(
            r#"<html lang="es"><head>
            <title>Título DOM</title>
            <meta property="og:title" content="Título OG">
            <meta property="og:description" content="Desc OG">
            <meta property="og:image" content="/img/portada.png">
            <meta property="og:image:width" content="1200">
            <meta property="og:image:height" content="630">
            </head><body></body></html>"#,
        );
        let data = run(&html, &base());
        assert_eq!(data.title.as_deref(), Some("Título OG"));
        assert_eq!(data.description.as_deref(), Some("Desc OG"));
        assert_eq!(data.lang.as_deref(), Some("es"));
        let image = data.image.expect("og:image presente");
        assert_eq!(
            image.url.as_deref(),
            Some("https://example.com/img/portada.png")
        );
        assert_eq!(image.width, Some(1200));
        assert_eq!(image.height, Some(630));
        assert_eq!(image.mime.as_deref(), Some("image/png"));
    }

    #[test]
    fn titulo_cae_al_dom_sin_og() {
        let html = Html::parse_document("<html><head><title>Solo DOM</title></head></html>");
        let data = run(&html, &base());
        assert_eq!(data.title.as_deref(), Some("Solo DOM"));
    }

    #[test]
    fn json_ld_aporta_autor_y_fecha() {
        let html = Html::parse_document(
            r#"<html><head>
            <script type="application/ld+json">
            {"@type":"NewsArticle","headline":"H","author":{"name":"Ana"},"datePublished":"2026-01-02","publisher":{"name":"Editorial"}}
            </script>
            </head></html>"#,
        );
        let data = run(&html, &base());
        assert_eq!(data.title.as_deref(), Some("H"));
        assert_eq!(data.author.as_deref(), Some("Ana"));
        assert_eq!(data.date.as_deref(), Some("2026-01-02"));
        assert_eq!(data.publisher.as_deref(), Some("Editorial"));
        assert!(data.json_ld.is_some());
    }

    #[test]
    fn graph_de_json_ld_se_aplana() {
        // El `@graph` se aplana: el `headline` del nodo Article (segundo) alimenta el título y el
        // `author` anidado del mismo nodo se reduce a su `name`.
        let html = Html::parse_document(
            r#"<html><head><script type="application/ld+json">
            {"@graph":[{"@type":"Person","name":"Bea"},{"@type":"Article","headline":"Titulo","author":{"name":"Ana"}}]}
            </script></head></html>"#,
        );
        let data = run(&html, &base());
        assert_eq!(data.title.as_deref(), Some("Titulo"));
        assert_eq!(data.author.as_deref(), Some("Ana"));
        // Los dos nodos del graph quedan accesibles en `json_ld`.
        assert_eq!(data.json_ld.expect("json_ld presente").len(), 2);
    }

    #[test]
    fn twitter_card_y_favicon_con_sizes() {
        let html = Html::parse_document(
            r#"<html><head>
            <meta name="twitter:card" content="summary_large_image">
            <meta name="twitter:image" content="https://cdn.example/tw.png">
            <link rel="icon" href="/favicon-32.png" sizes="32x32" type="image/png">
            </head></html>"#,
        );
        let data = run(&html, &base());
        let tw = data.twitter.expect("twitter presente");
        assert_eq!(
            tw.values.get("card").map(String::as_str),
            Some("summary_large_image")
        );
        assert_eq!(
            data.image.expect("twitter:image").url.as_deref(),
            Some("https://cdn.example/tw.png")
        );
        let favicons = data.favicons.expect("favicon");
        assert_eq!(favicons.len(), 1);
        assert_eq!(favicons[0].width, Some(32));
        assert_eq!(
            favicons[0].url.as_deref(),
            Some("https://example.com/favicon-32.png")
        );
    }

    #[test]
    fn favicon_de_respaldo_cuando_no_hay_link() {
        let html = Html::parse_document("<html><head><title>x</title></head></html>");
        let data = run(&html, &base());
        let favicons = data.favicons.expect("fallback");
        assert_eq!(favicons.len(), 1);
        assert_eq!(
            favicons[0].url.as_deref(),
            Some("https://example.com/favicon.ico")
        );
    }

    #[test]
    fn imagen_relativa_se_resuelve_contra_la_base() {
        let html = Html::parse_document(
            r#"<html><head><meta property="og:image" content="../assets/x.jpg"></head></html>"#,
        );
        let data = run(&html, &base());
        assert_eq!(
            data.image.expect("og").url.as_deref(),
            Some("https://example.com/assets/x.jpg")
        );
    }

    #[test]
    fn data_url_se_descarta_como_imagen() {
        let html = Html::parse_document(
            r#"<html><head><meta property="og:image" content="data:image/png;base64,AAA"></head></html>"#,
        );
        let data = run(&html, &base());
        assert!(data.image.is_none(), "data: no es una URL absoluta http");
    }

    #[test]
    fn video_con_duracion() {
        let html = Html::parse_document(
            r#"<html><head>
            <meta property="og:video" content="https://cdn.example/v.mp4">
            <meta property="og:video:type" content="video/mp4">
            <meta property="og:video:duration" content="95.5">
            </head></html>"#,
        );
        let data = run(&html, &base());
        let video = data.video.expect("og:video");
        assert_eq!(video.mime.as_deref(), Some("video/mp4"));
        assert_eq!(video.duration, Some(95.5));
    }
}
