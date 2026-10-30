use encoding_rs::Encoding;

/// Longitud del prefijo que se inspecciona buscando un `<meta charset>` cuando el `Content-Type`
/// no lo declara. Basta con la cabecera del documento; el cuerpo grande no se escanea.
const META_SNIFF_BYTES: usize = 2048;

/// Decodifica el cuerpo a `String` usando el charset real (D3/Fase 3: `charset` es la causa nº1 de
/// metadatos mojibake). Orden de resolución, igual que el spec HTML:
/// 1. BOM (UTF-8 / UTF-16LE / UTF-16BE) — gana siempre.
/// 2. `charset=` del `Content-Type`.
/// 3. `<meta charset>` o `<meta http-equiv="Content-Type" content="...charset=...">` del prefijo.
/// 4. UTF-8 por defecto.
///
/// Nunca falla: un charset desconocido o un cuerpo inválido decae a UTF-8 con sustitución, porque
/// unos metadatos parcialmente legibles valen más que un 500.
pub fn decode_html(body: &[u8], content_type: Option<&str>) -> String {
    if let Some((encoding, rest)) = sniff_bom(body) {
        return decode_with(encoding, rest);
    }

    let label = charset_from_content_type(content_type)
        .or_else(|| charset_from_meta(body))
        .unwrap_or_else(|| "utf-8".to_string());

    let encoding = Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::UTF_8);
    decode_with(encoding, body)
}

fn decode_with(encoding: &'static Encoding, bytes: &[u8]) -> String {
    // `decode` devuelve `Cow`: evita copiar cuando el cuerpo ya es UTF-8 válido.
    encoding.decode(bytes).0.into_owned()
}

fn sniff_bom(body: &[u8]) -> Option<(&'static Encoding, &[u8])> {
    if let Some(rest) = body.strip_prefix(&[0xEF, 0xBB, 0xBF][..]) {
        return Some((encoding_rs::UTF_8, rest));
    }
    if let Some(rest) = body.strip_prefix(&[0xFF, 0xFE][..]) {
        return Some((encoding_rs::UTF_16LE, rest));
    }
    if let Some(rest) = body.strip_prefix(&[0xFE, 0xFF][..]) {
        return Some((encoding_rs::UTF_16BE, rest));
    }
    None
}

/// `Content-Type: text/html; charset=windows-1252` → `windows-1252`.
fn charset_from_content_type(content_type: Option<&str>) -> Option<String> {
    let value = content_type?;
    for part in value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("charset=") {
            let cleaned = rest.trim().trim_matches('"').trim_matches('\'');
            if !cleaned.is_empty() {
                return Some(cleaned.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Escanea el prefijo ASCII en busca de una declaración `<meta charset>` o del equivalente
/// `http-equiv`. Búsqueda tolerante a mayúsculas y espacios, sin parser HTML (todavía no sabemos el
/// charset, así que no podemos parsear de forma fiable).
fn charset_from_meta(body: &[u8]) -> Option<String> {
    let prefix_len = body.len().min(META_SNIFF_BYTES);
    let prefix = String::from_utf8_lossy(&body[..prefix_len]).to_ascii_lowercase();

    // `<meta charset="x">`
    if let Some(pos) = prefix.find("charset") {
        let after = &prefix[pos + "charset".len()..];
        let after = after
            .trim_start_matches(|c: char| c.is_whitespace() || c == '=' || c == '"' || c == '\'');
        let label: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
            .collect();
        if !label.is_empty() {
            return Some(label);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_por_defecto() {
        let body = "<title>caña</title>".as_bytes();
        assert_eq!(decode_html(body, None), "<title>caña</title>");
    }

    #[test]
    fn charset_del_content_type_gana() {
        // "españa" en windows-1252: la ñ es 0xF1.
        let body = b"<title>espa\xf1a</title>";
        let out = decode_html(body, Some("text/html; charset=windows-1252"));
        assert_eq!(out, "<title>españa</title>");
    }

    #[test]
    fn charset_del_meta_cuando_no_hay_content_type() {
        let body =
            b"<html><head><meta charset=\"windows-1252\"></head><body>espa\xf1a</body></html>";
        let out = decode_html(body, None);
        assert!(out.contains("españa"), "got: {out}");
    }

    #[test]
    fn utf16le_por_bom() {
        // BOM UTF-16LE + "hi"
        let mut body = vec![0xFF, 0xFE];
        body.extend_from_slice(&[b'h', 0x00, b'i', 0x00]);
        assert_eq!(decode_html(&body, None), "hi");
    }

    #[test]
    fn utf16be_por_bom() {
        let mut body = vec![0xFE, 0xFF];
        body.extend_from_slice(&[0x00, b'h', 0x00, b'i']);
        assert_eq!(decode_html(&body, None), "hi");
    }

    #[test]
    fn bom_utf8_se_retira() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice("hola".as_bytes());
        assert_eq!(decode_html(&body, None), "hola");
    }

    #[test]
    fn charset_desconocido_decae_a_utf8() {
        let body = "valid".as_bytes();
        assert_eq!(
            decode_html(body, Some("text/html; charset=iso-9999-1")),
            "valid"
        );
    }

    #[test]
    fn content_type_charset_entre_comillas() {
        let label = charset_from_content_type(Some("text/html; charset=\"Shift_JIS\""));
        assert_eq!(label.as_deref(), Some("shift_jis"));
    }
}
