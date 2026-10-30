use std::io::Cursor;

use url::Url;

use crate::domain::preview::result::ContentInfo;

/// Extracción del texto principal (Fase 3, capacidad `content`). `readability` (port de Mozilla)
/// aísla el artículo y devuelve tanto el texto plano como el HTML limpio; `htmd` convierte ese
/// HTML a Markdown. **Best-effort**: si la extracción falla (página sin cuerpo de artículo, DOM
/// degenerado) devuelve `ContentInfo` vacío en vez de propagar el error — unos metadatos sin
/// contenido valen más que un 500.
pub fn extract(html: &str, base_url: &Url) -> ContentInfo {
    let mut reader = Cursor::new(html.as_bytes());
    let Ok(article) = readability::extractor::extract(&mut reader, base_url) else {
        return ContentInfo::default();
    };

    let text = clean(article.text);
    // Markdown desde el HTML limpio de readability; si htmd falla, el texto plano ya está.
    let markdown = htmd::convert(&article.content).ok().and_then(clean);

    ContentInfo { text, markdown }
}

/// Recorta y normaliza; devuelve `None` si no queda nada útil.
fn clean(raw: String) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/post").unwrap()
    }

    #[test]
    fn extrae_texto_de_un_articulo() {
        let html = r#"<html><head><title>Post</title></head><body>
        <article>
            <p>Este es el primer párrafo del artículo, suficientemente largo para que el algoritmo
            de readability lo considere contenido principal y no lo descarte por breve.</p>
            <p>Un segundo párrafo con más texto para reforzar la densidad de contenido del bloque
            principal y que readability lo elija frente al ruido de navegación.</p>
            <p>Y un tercer párrafo de cierre que aporta aún más palabras al cuerpo del artículo.</p>
        </article>
        <nav><a href="/">Inicio</a><a href="/about">Acerca</a></nav>
        </body></html>"#;
        let content = extract(html, &base());
        let text = content.text.expect("debe extraer texto");
        assert!(text.contains("primer párrafo"), "got: {text}");
    }

    #[test]
    fn documento_vacio_no_rompe() {
        let content = extract("<html><body></body></html>", &base());
        assert!(content.is_empty());
    }

    #[test]
    fn clean_devuelve_none_para_blancos() {
        assert_eq!(clean("   \n  ".to_string()), None);
        assert_eq!(clean("  hola  ".to_string()), Some("hola".to_string()));
    }
}
