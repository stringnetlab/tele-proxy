use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::antibot::AntibotInfo;
use super::media::MediaInfo;

/// Contenido extraído (Fase 3, capacidad `content`). `markdown` es la conversión de `text` vía
/// `htmd`; ambos `Option` porque la extracción puede fallar sin tumbar la respuesta de metadatos.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContentInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
}

impl ContentInfo {
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.markdown.is_none()
    }
}

/// Bloque OpenGraph / Twitter Cards. Son mapas planos (prefijo ya retirado) porque la forma de las
/// claves es abierta (`og:image:width`, `article:section`, ...): un struct rígido perdería las
/// desconocidas y el plan pide preservarlas.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MetaMap {
    #[serde(flatten)]
    pub values: HashMap<String, String>,
}

impl MetaMap {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Datos de preview de una URL (D2, sin oEmbed en esta entrega). Todo `Option`/`skip_if_none` para
/// que la cascada rellene solo lo que encontró y la respuesta no lleve `null` de relleno.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewData {
    /// URL **final** tras los saltos de redirect revalidados.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<MediaInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<MediaInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<MediaInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<MediaInfo>,
    /// Favicons (con fallback `/favicon.ico` si el HTML no declara ninguno).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favicons: Option<Vec<MediaInfo>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_graph: Option<MetaMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub twitter: Option<MetaMap>,
    /// JSON-LD básico: los objetos `@graph`/`@type` tal cual se extrajeron del `<script>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_ld: Option<Vec<serde_json::Value>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInfo>,

    /// Siempre presente (D3): `detected=false` es información útil, no ruido.
    pub antibot: AntibotInfo,
}

/// Envolvente JSend (D2). `data` es `serde_json::Value` porque el éxito lleva `PreviewData` y los
/// errores propios ya se renderizan por el camino de `ProxyError`; aun así se modela el error JSend
/// para los fallos de preview que no son 5xx opacos.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewResponse {
    /// `"success"` | `"fail"` | `"error"`.
    pub status: &'static str,
    pub status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<PreviewData>,
}

impl PreviewResponse {
    pub fn success(data: PreviewData) -> Self {
        Self {
            status: "success",
            status_code: 200,
            data: Some(data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_lleva_status_y_codigo() {
        let resp = PreviewResponse::success(PreviewData {
            title: Some("Hola".into()),
            antibot: AntibotInfo::none(),
            ..Default::default()
        });
        assert_eq!(resp.status, "success");
        assert_eq!(resp.status_code, 200);
    }

    #[test]
    fn data_omite_campos_vacios_y_emite_antibot() {
        let resp = PreviewResponse::success(PreviewData {
            title: Some("T".into()),
            antibot: AntibotInfo::none(),
            ..Default::default()
        });
        let json = serde_json::to_value(&resp).unwrap();
        let data = &json["data"];
        assert_eq!(data["title"], "T");
        assert!(data.get("description").is_none(), "los None se omiten");
        assert!(data.get("antibot").is_some(), "antibot siempre presente");
        assert_eq!(data["antibot"]["detected"], false);
    }

    #[test]
    fn content_y_metamap_vacios_se_descartan() {
        assert!(ContentInfo::default().is_empty());
        assert!(MetaMap::default().is_empty());
        let mm = MetaMap {
            values: [("title".to_string(), "x".to_string())]
                .into_iter()
                .collect(),
        };
        assert!(!mm.is_empty());
    }
}
