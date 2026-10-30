use serde::{Deserialize, Serialize};

/// Objeto de media **reutilizable** (D2): `image/logo/video/audio/screenshot/pdf` comparten
/// forma. Todo `Option` para que cada detector rellene solo lo que sabe y el resto viaje
/// omitido (`skip_serializing_if`), en vez de aparecer como `null` ruidoso.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// MIME resuelto (content-type del origen o inferido del sufijo).
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `size` formateado ("84 KB"). Derivado, nunca una fuente de verdad aparte.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_pretty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Duración en segundos (video/audio).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

impl MediaInfo {
    pub fn with_url(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            ..Default::default()
        }
    }

    /// `true` cuando no aporta nada serializable (ni siquiera URL). La cascada descarta estos
    /// para no emitir objetos vacíos `{}` en la respuesta.
    pub fn is_empty(&self) -> bool {
        self.url.is_none()
            && self.mime.is_none()
            && self.width.is_none()
            && self.height.is_none()
            && self.size.is_none()
            && self.duration.is_none()
    }

    /// Rellena `size_pretty` a partir de `size` si este existe y aquel no. Se llama al final de
    /// la cascada, una vez conocido el tamaño real, para no repetir el formateo en cada detector.
    pub fn with_derived_size_pretty(mut self) -> Self {
        if let (Some(size), None) = (self.size, self.size_pretty.as_ref()) {
            self.size_pretty = Some(pretty_bytes(size));
        }
        self
    }
}

/// Formatea bytes a una cadena legible con una cifra decimal, base 1000 (convención
/// `link-preview-js` / microlink: "84 KB", no "82 KiB").
pub fn pretty_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    // Una cifra decimal, sin ceros colgantes ("1 MB" y no "1.0 MB").
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded - rounded.round()).abs() < f64::EPSILON {
        format!("{:.0} {}", rounded, UNITS[unit])
    } else {
        format!("{rounded:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_bytes_casos() {
        assert_eq!(pretty_bytes(0), "0 B");
        assert_eq!(pretty_bytes(999), "999 B");
        assert_eq!(pretty_bytes(1000), "1 KB");
        assert_eq!(pretty_bytes(84_123), "84.1 KB");
        assert_eq!(pretty_bytes(1_000_000), "1 MB");
        assert_eq!(pretty_bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn size_pretty_derivado() {
        let m = MediaInfo {
            url: Some("https://a/x.png".into()),
            size: Some(84_123),
            ..Default::default()
        }
        .with_derived_size_pretty();
        assert_eq!(m.size_pretty.as_deref(), Some("84.1 KB"));
    }

    #[test]
    fn size_pretty_no_sobrescribe_uno_existente() {
        let m = MediaInfo {
            size: Some(84_123),
            size_pretty: Some("manual".into()),
            ..Default::default()
        }
        .with_derived_size_pretty();
        assert_eq!(m.size_pretty.as_deref(), Some("manual"));
    }

    #[test]
    fn vacio_se_descarta() {
        assert!(MediaInfo::default().is_empty());
        assert!(!MediaInfo::with_url("https://a/x").is_empty());
    }

    #[test]
    fn serializa_en_camel_y_omite_nones() {
        let m = MediaInfo::with_url("https://a/x.png");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"url\""));
        assert!(!json.contains("sizePretty"), "los None no se serializan");
        assert!(!json.contains("size_pretty"));
    }

    #[test]
    fn type_es_palabra_reservada_en_serializacion() {
        let m = MediaInfo {
            mime: Some("image/png".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            json.contains("\"type\":\"image/png\""),
            "el campo viaja como `type`"
        );
    }
}
