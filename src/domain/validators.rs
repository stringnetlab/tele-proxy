use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::Url;

use crate::domain::errors::ProxyError;
use crate::domain::models::ClientConfigUpdate;

pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_private_ipv6(v6),
    }
}

fn is_private_ipv4(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();

    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
    {
        return true;
    }

    if octets[0] == 100 && (octets[1] & 0xC0) == 64 {
        return true;
    }

    if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
        return true;
    }

    if octets[0] == 192 && octets[1] == 0 && octets[2] == 2 {
        return true;
    }

    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return true;
    }

    if octets[0] == 198 && octets[1] == 51 && octets[2] == 100 {
        return true;
    }

    if octets[0] == 203 && octets[1] == 0 && octets[2] == 113 {
        return true;
    }

    if (octets[0] & 0xF0) == 240 {
        return true;
    }

    if octets[0] == 169 && octets[1] == 254 && octets[2] == 169 && octets[3] == 254 {
        return true;
    }

    false
}

fn is_private_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }

    let segments = ip.segments();

    if (segments[0] & 0xFE00) == 0xFC00 {
        return true;
    }

    if (segments[0] & 0xFFC0) == 0xFE80 {
        return true;
    }

    if (segments[0] & 0xFF00) == 0xFF00 {
        return true;
    }

    if segments[0] == 0x2001 && segments[1] == 0x0DB8 {
        return true;
    }

    if segments[0] == 0x2001 && segments[1] == 0x0002 {
        return true;
    }

    if segments[0] == 0x2001 && segments[1] == 0x0010 {
        return true;
    }

    if let Some(mapped) = ip.to_ipv4() {
        return is_private_ipv4(&mapped);
    }

    false
}

pub fn validate_url_strict(url_str: &str) -> Result<Url, ProxyError> {
    let url = Url::parse(url_str).map_err(|e| ProxyError::InvalidUrlFormat {
        url: url_str.to_string(),
        reason: format!("Failed to parse URL: {}", e),
    })?;

    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: format!(
                "Invalid scheme '{}': only http and https are allowed",
                url.scheme()
            ),
        });
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: "URL contains credentials (username/password)".to_string(),
        });
    }

    if url.fragment().is_some() {
        return Err(ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: "URL contains fragment identifier (#)".to_string(),
        });
    }

    let host = url.host_str().ok_or_else(|| ProxyError::InvalidUrlFormat {
        url: url_str.to_string(),
        reason: "URL has no host".to_string(),
    })?;

    if host.is_empty() {
        return Err(ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: "URL host is empty".to_string(),
        });
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(&ip) {
            return Err(ProxyError::SsrfBlocked {
                url: url_str.to_string(),
                resolved_ip: ip.to_string(),
                reason: "URL directly references private IP".to_string(),
            });
        }
    }

    Ok(url)
}

pub fn extract_domain(url: &Url) -> Result<String, ProxyError> {
    let host = url.host_str().ok_or_else(|| ProxyError::InvalidUrlFormat {
        url: url.to_string(),
        reason: "URL has no host".to_string(),
    })?;

    Ok(host.to_lowercase())
}

pub fn domain_matches_whitelist(domain: &str, whitelist: &[String]) -> bool {
    let domain_lower = domain.to_lowercase();
    whitelist.iter().any(|allowed| {
        let allowed_lower = allowed.to_lowercase();
        domain_lower == allowed_lower || domain_lower.ends_with(&format!(".{}", allowed_lower))
    })
}

/// Cotas de `ClientConfigUpdate` (`docs/api_contract.yaml`), comprobadas **antes** de escribir en
/// CouchDB para que un `GET` nunca devuelva un valor que el propio contrato rechazaría.
pub const MAX_WHITELIST_ENTRIES: usize = 100;
pub const MAX_REQUESTS_CEILING: u32 = 10_000;
pub const WINDOW_SECONDS_CEILING: u64 = 3_600;
pub const MIN_SCRIPTING_BODY_BYTES: u64 = 1_048_576;
pub const MAX_SCRIPTING_BODY_BYTES: u64 = 52_428_800;
const MAX_HOST_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;
const SHA256_HEX_BYTES: usize = 64;

/// Validación de `PUT /api/v1/clients/config` (`docs/spec.md`, Fase 5). Solo mira el payload
/// recibido: los campos `None` no se tocan, porque el repo los fusiona sobre el documento
/// existente.
pub fn validate_config_update(update: &ClientConfigUpdate) -> Result<(), ProxyError> {
    if let Some(whitelist) = &update.whitelist {
        if whitelist.len() > MAX_WHITELIST_ENTRIES {
            return Err(invalid_config(
                "whitelist",
                format!(
                    "{} entries exceed the {} allowed",
                    whitelist.len(),
                    MAX_WHITELIST_ENTRIES
                ),
            ));
        }
        for entry in whitelist {
            validate_whitelist_entry(entry)?;
        }
    }

    if let Some(rate_limit) = &update.rate_limit {
        if !(1..=MAX_REQUESTS_CEILING).contains(&rate_limit.max_requests) {
            return Err(invalid_config(
                "rate_limit.max_requests",
                format!(
                    "{} is outside 1..={}",
                    rate_limit.max_requests, MAX_REQUESTS_CEILING
                ),
            ));
        }
        if !(1..=WINDOW_SECONDS_CEILING).contains(&rate_limit.window_seconds) {
            return Err(invalid_config(
                "rate_limit.window_seconds",
                format!(
                    "{} is outside 1..={}",
                    rate_limit.window_seconds, WINDOW_SECONDS_CEILING
                ),
            ));
        }
    }

    if let Some(max_bytes) = update.max_scripting_body_bytes {
        if !(MIN_SCRIPTING_BODY_BYTES..=MAX_SCRIPTING_BODY_BYTES).contains(&max_bytes) {
            return Err(invalid_config(
                "max_scripting_body_bytes",
                format!(
                    "{} is outside {}..={}",
                    max_bytes, MIN_SCRIPTING_BODY_BYTES, MAX_SCRIPTING_BODY_BYTES
                ),
            ));
        }
    }

    if let Some(scripting) = &update.scripting {
        let has_code = scripting
            .code
            .as_deref()
            .is_some_and(|code| !code.is_empty());
        if has_code && scripting.code_hash.is_none() {
            return Err(invalid_config(
                "scripting.code_hash",
                "required when scripting.code is provided".to_string(),
            ));
        }
        if let Some(code_hash) = &scripting.code_hash {
            validate_sha256_hash("scripting.code_hash", code_hash)?;
        }
    }

    // `error_handling.mode` no se comprueba aquí: `ErrorMode` es un enum serde y un valor
    // distinto de transparent/wrapped ya se rechaza al deserializar el cuerpo.
    if let Some(error_handling) = &update.error_handling {
        for (mime, resource) in &error_handling.fallback_urls {
            validate_url_strict(&resource.url)?;
            validate_sha256_hash(
                &format!("error_handling.fallback_urls[{}].hash", mime),
                &resource.hash,
            )?;
        }
    }

    Ok(())
}

fn invalid_config(field: &str, reason: String) -> ProxyError {
    ProxyError::InvalidConfig {
        field: field.to_string(),
        reason,
    }
}

/// Una entrada de `whitelist` tiene que ser el host desnudo: si admite `:`, `/`, `@`, espacios o
/// caracteres de control, el cliente puede meter una cabecera `Host`/`Authorization` entera
/// (`docs/BDD.md`, Feature 6); si admite una IP privada, fija la puerta anti-SSRF desde la config.
fn validate_whitelist_entry(entry: &str) -> Result<(), ProxyError> {
    if let Ok(ip) = entry.parse::<IpAddr>() {
        if is_private_ip(&ip) {
            return Err(invalid_config(
                "whitelist",
                format!(
                    "'{}' is a private or reserved address",
                    display_entry(entry)
                ),
            ));
        }
        return Ok(());
    }

    if !is_valid_hostname(entry) {
        return Err(invalid_config(
            "whitelist",
            format!("'{}' is not a bare domain name", display_entry(entry)),
        ));
    }

    Ok(())
}

/// El valor vuelve al cliente en el `message` del 400 y al log en `fields`, así que se escapa
/// (un CRLF no puede inyectar líneas) y se recorta (no alarga el registro arbitrariamente).
fn display_entry(entry: &str) -> String {
    entry
        .escape_debug()
        .take(MAX_HOST_BYTES)
        .collect::<String>()
}

fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_HOST_BYTES {
        return false;
    }

    // `rsplit_once` exige al menos dos etiquetas, así que `localhost` no entra, y el TLD
    // estrictamente alfabético descarta cuasi-IPs como `1.2.3`, que `IpAddr` no reconoce.
    let Some((prefix, tld)) = host.rsplit_once('.') else {
        return false;
    };

    !tld.is_empty()
        && tld.len() <= MAX_LABEL_BYTES
        && tld.bytes().all(|byte| byte.is_ascii_alphabetic())
        && prefix.split('.').all(is_valid_label)
}

fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_BYTES
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// `sha256:<64 hex>` es el único formato documentado (`docs/spec.md`, § Modelos de Datos) para
/// `scripting.code_hash` y para el `hash` de cada fallback.
fn validate_sha256_hash(field: &str, hash: &str) -> Result<(), ProxyError> {
    let digest = hash.strip_prefix("sha256:").ok_or_else(|| {
        invalid_config(
            field,
            format!(
                "expected the sha256:<{} hex digits> format",
                SHA256_HEX_BYTES
            ),
        )
    })?;

    if digest.len() != SHA256_HEX_BYTES || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_config(
            field,
            format!(
                "digest must be exactly {} hex digits, got {}",
                SHA256_HEX_BYTES,
                digest.len()
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::domain::models::{
        ErrorHandlingConfig, ErrorMode, FallbackResource, RateLimitConfig, ScriptingUpdate,
    };

    use super::*;

    fn base_update() -> ClientConfigUpdate {
        ClientConfigUpdate {
            whitelist: None,
            rate_limit: None,
            max_scripting_body_bytes: None,
            scripting: None,
            error_handling: None,
        }
    }

    fn invalid_field(result: Result<(), ProxyError>) -> String {
        match result {
            Err(ProxyError::InvalidConfig { field, .. }) => field,
            other => panic!("expected invalid_config, got {other:?}"),
        }
    }

    #[test]
    fn test_private_ipv4_ranges() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.255.255.255".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.31.255.255".parse().unwrap()));
        assert!(is_private_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.255.255".parse().unwrap()));
        assert!(is_private_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.169.254".parse().unwrap()));
        assert!(is_private_ip(&"0.0.0.0".parse().unwrap()));
        assert!(is_private_ip(&"255.255.255.255".parse().unwrap()));
        assert!(is_private_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_ip(&"100.127.255.255".parse().unwrap()));
        assert!(is_private_ip(&"192.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.0.2.1".parse().unwrap()));
        assert!(is_private_ip(&"198.18.0.1".parse().unwrap()));
        assert!(is_private_ip(&"198.51.100.1".parse().unwrap()));
        assert!(is_private_ip(&"203.0.113.1".parse().unwrap()));
        assert!(is_private_ip(&"240.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_public_ipv4() {
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"93.184.216.34".parse().unwrap()));
        assert!(!is_private_ip(&"172.15.255.255".parse().unwrap()));
        assert!(!is_private_ip(&"172.32.0.0".parse().unwrap()));
    }

    #[test]
    fn test_private_ipv6() {
        assert!(is_private_ip(&"::1".parse().unwrap()));
        assert!(is_private_ip(&"::".parse().unwrap()));
        assert!(is_private_ip(&"fc00::1".parse().unwrap()));
        assert!(is_private_ip(&"fd00::1".parse().unwrap()));
        assert!(is_private_ip(&"fe80::1".parse().unwrap()));
        assert!(is_private_ip(&"ff02::1".parse().unwrap()));
        assert!(is_private_ip(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn test_public_ipv6() {
        assert!(!is_private_ip(&"2606:4700:4700::1111".parse().unwrap()));
        assert!(!is_private_ip(&"2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn test_validate_url_valid() {
        assert!(validate_url_strict("https://example.com/image.jpg").is_ok());
        assert!(validate_url_strict("http://example.com/path?q=1").is_ok());
        assert!(validate_url_strict("https://sub.domain.com:8080/file").is_ok());
    }

    #[test]
    fn test_validate_url_invalid_scheme() {
        let result = validate_url_strict("ftp://example.com/file");
        assert!(matches!(result, Err(ProxyError::InvalidUrlFormat { .. })));

        let result = validate_url_strict("file:///etc/passwd");
        assert!(matches!(result, Err(ProxyError::InvalidUrlFormat { .. })));

        let result = validate_url_strict("javascript:alert(1)");
        assert!(matches!(result, Err(ProxyError::InvalidUrlFormat { .. })));
    }

    #[test]
    fn test_validate_url_with_credentials() {
        let result = validate_url_strict("https://user:pass@example.com/");
        assert!(matches!(result, Err(ProxyError::InvalidUrlFormat { .. })));

        let result = validate_url_strict("https://user@example.com/");
        assert!(matches!(result, Err(ProxyError::InvalidUrlFormat { .. })));
    }

    #[test]
    fn test_validate_url_with_fragment() {
        let result = validate_url_strict("https://example.com/page#section");
        assert!(matches!(result, Err(ProxyError::InvalidUrlFormat { .. })));
    }

    #[test]
    fn test_validate_url_private_ip_direct() {
        let result = validate_url_strict("http://127.0.0.1/admin");
        assert!(matches!(result, Err(ProxyError::SsrfBlocked { .. })));

        let result = validate_url_strict("http://169.254.169.254/latest/meta-data/");
        assert!(matches!(result, Err(ProxyError::SsrfBlocked { .. })));

        let result = validate_url_strict("http://10.0.0.1/internal");
        assert!(matches!(result, Err(ProxyError::SsrfBlocked { .. })));

        let result = validate_url_strict("http://192.168.1.1/router");
        assert!(matches!(result, Err(ProxyError::SsrfBlocked { .. })));
    }

    #[test]
    fn test_extract_domain() {
        let url = Url::parse("https://www.example.com/path").unwrap();
        assert_eq!(extract_domain(&url).unwrap(), "www.example.com");

        let url = Url::parse("https://SUB.DOMAIN.COM/").unwrap();
        assert_eq!(extract_domain(&url).unwrap(), "sub.domain.com");
    }

    #[test]
    fn test_domain_matches_whitelist() {
        let whitelist = vec![
            "shutterstock.com".to_string(),
            "gettyimages.com".to_string(),
        ];

        assert!(domain_matches_whitelist("shutterstock.com", &whitelist));
        assert!(domain_matches_whitelist("www.shutterstock.com", &whitelist));
        assert!(domain_matches_whitelist("cdn.shutterstock.com", &whitelist));
        assert!(domain_matches_whitelist("SHUTTERSTOCK.COM", &whitelist));
        assert!(domain_matches_whitelist("gettyimages.com", &whitelist));

        assert!(!domain_matches_whitelist("malicious.com", &whitelist));
        assert!(!domain_matches_whitelist("notshutterstock.com", &whitelist));
        assert!(!domain_matches_whitelist(
            "shutterstock.com.evil.com",
            &whitelist
        ));
    }

    #[test]
    fn test_validate_config_update_accepts_empty_and_in_range() {
        assert!(validate_config_update(&base_update()).is_ok());

        let update = ClientConfigUpdate {
            whitelist: Some(vec![
                "shutterstock.com".to_string(),
                "GETTYIMAGES.COM".to_string(),
                "cdn.bare-sub.example.co.uk".to_string(),
                "8.8.8.8".to_string(),
            ]),
            rate_limit: Some(RateLimitConfig {
                max_requests: 10_000,
                window_seconds: 3_600,
            }),
            max_scripting_body_bytes: Some(MIN_SCRIPTING_BODY_BYTES),
            scripting: Some(ScriptingUpdate {
                enabled: Some(true),
                code: Some("function handle(req, res) end".to_string()),
                code_hash: Some(format!("sha256:{}", "ab".repeat(SHA256_HEX_BYTES / 2))),
            }),
            error_handling: None,
        };
        assert!(validate_config_update(&update).is_ok());
    }

    #[test]
    fn test_validate_config_update_rejects_rate_limits_out_of_range() {
        let cases = [
            (0, 60, "rate_limit.max_requests"),
            (MAX_REQUESTS_CEILING + 1, 60, "rate_limit.max_requests"),
            (50, 0, "rate_limit.window_seconds"),
            (50, WINDOW_SECONDS_CEILING + 1, "rate_limit.window_seconds"),
        ];

        for (max_requests, window_seconds, expected_field) in cases {
            let update = ClientConfigUpdate {
                rate_limit: Some(RateLimitConfig {
                    max_requests,
                    window_seconds,
                }),
                ..base_update()
            };
            assert_eq!(
                invalid_field(validate_config_update(&update)),
                expected_field
            );
        }
    }

    #[test]
    fn test_validate_config_update_rejects_oversized_whitelist() {
        let whitelist = (0..=MAX_WHITELIST_ENTRIES)
            .map(|n| format!("host{n}.example.com"))
            .collect();
        let update = ClientConfigUpdate {
            whitelist: Some(whitelist),
            ..base_update()
        };

        assert_eq!(invalid_field(validate_config_update(&update)), "whitelist");
    }

    #[test]
    fn test_validate_config_update_rejects_whitelist_entries_that_are_not_bare_hosts() {
        let entries = [
            "shutterstock.com:8080",
            "user:pass@shutterstock.com",
            "https://shutterstock.com",
            "shutterstock.com/admin",
            "shutterstock.com\r\nAuthorization: Bearer stolen",
            "not shutterstock.com",
            "localhost",
            "-leading.example.com",
            "trailing-.example.com",
            "1.2.3",
            "",
        ];

        for entry in entries {
            let update = ClientConfigUpdate {
                whitelist: Some(vec![entry.to_string()]),
                ..base_update()
            };
            assert_eq!(
                invalid_field(validate_config_update(&update)),
                "whitelist",
                "{entry:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_validate_config_update_rejects_private_ips_in_whitelist() {
        for entry in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "::1",
            "fe80::1",
        ] {
            let update = ClientConfigUpdate {
                whitelist: Some(vec![entry.to_string()]),
                ..base_update()
            };
            assert_eq!(
                invalid_field(validate_config_update(&update)),
                "whitelist",
                "{entry:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_validate_config_update_rejects_scripting_bytes_out_of_range() {
        for max_bytes in [
            0,
            MIN_SCRIPTING_BODY_BYTES - 1,
            MAX_SCRIPTING_BODY_BYTES + 1,
        ] {
            let update = ClientConfigUpdate {
                max_scripting_body_bytes: Some(max_bytes),
                ..base_update()
            };
            assert_eq!(
                invalid_field(validate_config_update(&update)),
                "max_scripting_body_bytes"
            );
        }
    }

    #[test]
    fn test_validate_config_update_requires_code_hash_with_code() {
        let with_code = ClientConfigUpdate {
            scripting: Some(ScriptingUpdate {
                enabled: Some(true),
                code: Some("return body".to_string()),
                code_hash: None,
            }),
            ..base_update()
        };
        assert_eq!(
            invalid_field(validate_config_update(&with_code)),
            "scripting.code_hash"
        );

        // Borrar el código no requiere hash: es el camino de desactivar scripting.
        let cleared = ClientConfigUpdate {
            scripting: Some(ScriptingUpdate {
                enabled: Some(false),
                code: Some(String::new()),
                code_hash: None,
            }),
            ..base_update()
        };
        assert!(validate_config_update(&cleared).is_ok());
    }

    #[test]
    fn test_validate_config_update_rejects_malformed_hashes() {
        for hash in [
            "",
            "abc123",
            "sha256:zz",
            &format!("sha256:{}", "ab".repeat(31)),
        ] {
            let update = ClientConfigUpdate {
                scripting: Some(ScriptingUpdate {
                    enabled: None,
                    code: None,
                    code_hash: Some(hash.to_string()),
                }),
                ..base_update()
            };
            assert_eq!(
                invalid_field(validate_config_update(&update)),
                "scripting.code_hash",
                "{hash:?} should be rejected"
            );
        }
    }

    #[test]
    fn test_validate_config_update_validates_fallback_urls() {
        let valid_hash = format!("sha256:{}", "cd".repeat(SHA256_HEX_BYTES / 2));

        let good = ClientConfigUpdate {
            error_handling: Some(ErrorHandlingConfig {
                mode: ErrorMode::Wrapped,
                fallback_urls: [(
                    "image/*".to_string(),
                    FallbackResource {
                        url: "https://cdn.example.com/error.png".to_string(),
                        hash: valid_hash.clone(),
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..base_update()
        };
        assert!(validate_config_update(&good).is_ok());

        let bad_scheme = ClientConfigUpdate {
            error_handling: Some(ErrorHandlingConfig {
                mode: ErrorMode::Wrapped,
                fallback_urls: [(
                    "image/*".to_string(),
                    FallbackResource {
                        url: "ftp://cdn.example.com/error.png".to_string(),
                        hash: valid_hash.clone(),
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..base_update()
        };
        assert!(matches!(
            validate_config_update(&bad_scheme),
            Err(ProxyError::InvalidUrlFormat { .. })
        ));

        let missing_hash = ClientConfigUpdate {
            error_handling: Some(ErrorHandlingConfig {
                mode: ErrorMode::Wrapped,
                fallback_urls: [(
                    "image/*".to_string(),
                    FallbackResource {
                        url: "https://cdn.example.com/error.png".to_string(),
                        hash: String::new(),
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..base_update()
        };
        assert_eq!(
            invalid_field(validate_config_update(&missing_hash)),
            "error_handling.fallback_urls[image/*].hash"
        );
    }
}
