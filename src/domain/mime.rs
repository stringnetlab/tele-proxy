//! Detección del tipo real de un cuerpo por sus primeros bytes.
//!
//! El `Content-Type` que declara el origen (o el hint `?mime=`) no es una prueba: un challenge
//! anti-bot se sirve con frecuencia como `image/jpeg`, y un PDF puede anunciarse como `text/html`.
//! Aquí solo se reconocen firmas inequívocas; cualquier otro contenido devuelve `None` y la
//! decisión sigue dependiendo del tipo declarado.

/// Bytes que se inspeccionan como máximo. Cubre firmas, BOM y los marcadores textuales que van
/// precedidos de espacios o de una declaración XML.
pub const SNIFF_LEN: usize = 512;

/// Tipo detectado por magic bytes, en minúsculas y sin parámetros. `None` = firma desconocida.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);

    if head.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    if head.starts_with(b"\x89PNG\r\n\x1A\n") {
        return Some("image/png");
    }
    if head.starts_with(b"\xFF\xD8\xFF") {
        return Some("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        match &head[8..12] {
            b"avif" | b"avis" => return Some("image/avif"),
            b"heic" | b"heix" | b"mif1" | b"msf1" => return Some("image/heic"),
            _ => {}
        }
    }
    if head.len() >= 14 && head.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        return Some("image/tiff");
    }
    if head.starts_with(b"\x00\x00\x01\x00") {
        return Some("image/x-icon");
    }
    if head.starts_with(b"wOFF") {
        return Some("font/woff");
    }
    if head.starts_with(b"wOF2") {
        return Some("font/woff2");
    }

    let text = skip_ascii_whitespace(head);
    if starts_with_ignore_ascii_case(text, b"<!doctype html")
        || starts_with_ignore_ascii_case(text, b"<html")
        || starts_with_ignore_ascii_case(text, b"<head")
        || starts_with_ignore_ascii_case(text, b"<body")
    {
        return Some("text/html");
    }
    if starts_with_ignore_ascii_case(text, b"<svg") {
        return Some("image/svg+xml");
    }
    if starts_with_ignore_ascii_case(text, b"<?xml") {
        // Un XML genérico no permite afirmar el tipo; solo se marca cuando el SVG aparece dentro
        // de la ventana que ya se inspecciona.
        let window = &head[..head.len().min(SNIFF_LEN)];
        if window
            .windows(4)
            .any(|chunk| chunk.eq_ignore_ascii_case(b"<svg"))
        {
            return Some("image/svg+xml");
        }
        return Some("application/xml");
    }
    if matches!(text.first(), Some(b'{') | Some(b'[')) {
        return Some("application/json");
    }

    None
}

/// ¿El tipo esperado rompe el layout del cliente si el cuerpo es de otra familia? Una imagen
/// rota un `<img>` y un PDF esperado no se puede abrir; solo estos hints justifican el fallback.
pub fn is_binary(content_type: &str) -> bool {
    let essence = essence(content_type);
    essence.starts_with("image/") || essence.eq_ignore_ascii_case("application/pdf")
}

/// Esencia de un `Content-Type`: sin parámetros y sin espacios (`text/html; charset=utf-8` →
/// `text/html`). No se pasa a minúsculas para no asignar en el camino caliente: el llamador
/// compara sin distinguir mayúsculas.
pub fn essence(value: &str) -> &str {
    value.split(';').next().unwrap_or(value).trim()
}

/// ¿El tipo declarado y lo detectado pueden describir el mismo cuerpo? Dos tipos de la misma
/// familia (imagen, texto, fuente, audio, vídeo) se aceptan; `application/octet-stream` es un
/// comodín legítimo y nunca se marca como mentira.
pub fn agrees(declared: &str, sniffed: &str) -> bool {
    let declared = essence(declared);
    let sniffed = essence(sniffed);
    if declared.eq_ignore_ascii_case(sniffed) {
        return true;
    }
    if declared.eq_ignore_ascii_case("application/octet-stream")
        || declared.eq_ignore_ascii_case("binary/octet-stream")
    {
        return true;
    }
    family(declared) == family(sniffed)
}

fn family(content_type: &str) -> &str {
    match content_type {
        "application/json"
        | "text/json"
        | "application/javascript"
        | "text/javascript"
        | "application/xml"
        | "text/xml"
        | "application/xhtml+xml" => "text",
        other => match other.split('/').next().unwrap_or(other) {
            "application" => other,
            major => major,
        },
    }
}

fn skip_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn starts_with_ignore_ascii_case(haystack: &[u8], prefix: &[u8]) -> bool {
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detecta_las_firmas_binarias_habituales() {
        assert_eq!(sniff(b"%PDF-1.7\n"), Some("application/pdf"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1A\n...."), Some("image/png"));
        assert_eq!(sniff(b"\xFF\xD8\xFF\xE0...."), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"wOF2...."), Some("font/woff2"));
        assert_eq!(
            sniff(b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
            Some("image/bmp")
        );
    }

    #[test]
    fn detecta_html_con_doctype_espacios_y_mayusculas() {
        assert_eq!(sniff(b"<!DOCTYPE html><html>"), Some("text/html"));
        assert_eq!(sniff(b"  \r\n<head><title>x"), Some("text/html"));
        assert_eq!(sniff(b"\xEF\xBB\xBF<html lang=\"es\">"), Some("text/html"));
    }

    #[test]
    fn detecta_json_y_svg() {
        assert_eq!(sniff(b"{\"error\":\"antibot\"}"), Some("application/json"));
        assert_eq!(sniff(b"  [1,2,3]"), Some("application/json"));
        assert_eq!(sniff(b"<svg xmlns=\"...\">"), Some("image/svg+xml"));
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?>\n<svg xmlns=\"...\">"),
            Some("image/svg+xml")
        );
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?>\n<rss>"),
            Some("application/xml")
        );
    }

    #[test]
    fn contenido_desconocido_o_vacio_no_se_detecta() {
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"\x00\x01\x02\x03binario opaco"), None);
        assert_eq!(sniff(b"texto plano sin firma"), None);
    }

    #[test]
    fn octet_stream_es_comodin_pero_imagen_frente_a_html_no() {
        assert!(agrees("application/octet-stream", "image/png"));
        assert!(agrees("image/jpeg; charset=binary", "image/png"));
        assert!(agrees("text/html", "application/xhtml+xml"));
        assert!(agrees("text/html; charset=utf-8", "text/html"));
        assert!(!agrees("image/jpeg", "text/html"));
        assert!(!agrees("application/pdf", "text/html"));
        assert!(!agrees("image/png", "application/json"));
    }

    #[test]
    fn la_esencia_quita_parametros_y_espacios() {
        assert_eq!(essence("text/html; charset=utf-8"), "text/html");
        assert_eq!(essence(" image/png "), "image/png");
        assert_eq!(essence("application/pdf"), "application/pdf");
    }

    #[test]
    fn solo_imagen_y_pdf_justifican_fallback_por_tipo() {
        assert!(is_binary("image/jpeg"));
        assert!(is_binary("image/svg+xml; charset=utf-8"));
        assert!(is_binary("application/pdf"));
        assert!(!is_binary("text/html"));
        assert!(!is_binary("application/json"));
        assert!(!is_binary("application/octet-stream"));
    }
}
