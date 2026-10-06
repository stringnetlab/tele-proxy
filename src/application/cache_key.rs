use sha2::{Digest, Sha256};

pub fn response_cache_key(internal_id: &str, config_version: u64, url: &str) -> String {
    let url_hash = sha256_hex(url);
    format!("px:{}:{}:{}", internal_id, config_version, url_hash)
}

pub fn ip_cache_key(hostname: &str) -> String {
    format!("ip_cache:{}", hostname)
}

pub fn rate_limit_key(crypt_id: &str) -> String {
    format!("rl:{}", crypt_id)
}

pub fn fallback_cache_key(mime: &str) -> String {
    format!("px:defaults:{}", mime)
}

pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    // digest 0.11 dropped the LowerHex impl on the output array, so hex-encode manually.
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_cache_key_format() {
        let key = response_cache_key("client_int_123", 5, "https://example.com/img.jpg");
        assert!(key.starts_with("px:client_int_123:5:"));
        assert_eq!(key.split(':').count(), 4);
    }

    #[test]
    fn test_response_cache_key_changes_with_version() {
        let key_v1 = response_cache_key("client_1", 1, "https://example.com/img.jpg");
        let key_v2 = response_cache_key("client_1", 2, "https://example.com/img.jpg");
        assert_ne!(key_v1, key_v2);
    }

    #[test]
    fn test_response_cache_key_changes_with_url() {
        let key_a = response_cache_key("client_1", 1, "https://example.com/a.jpg");
        let key_b = response_cache_key("client_1", 1, "https://example.com/b.jpg");
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_response_cache_key_deterministic() {
        let key1 = response_cache_key("client_1", 1, "https://example.com/img.jpg");
        let key2 = response_cache_key("client_1", 1, "https://example.com/img.jpg");
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_ip_cache_key_format() {
        assert_eq!(ip_cache_key("example.com"), "ip_cache:example.com");
    }

    #[test]
    fn test_rate_limit_key_format() {
        assert_eq!(rate_limit_key("V1StGXR8_Z5j"), "rl:V1StGXR8_Z5j");
    }

    #[test]
    fn test_fallback_cache_key_format() {
        assert_eq!(fallback_cache_key("image/png"), "px:defaults:image/png");
    }

    #[test]
    fn test_sha256_hex_length() {
        let hash = sha256_hex("test");
        assert_eq!(hash.len(), 64);
        assert_eq!(
            hash,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }
}
