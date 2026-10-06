use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use reqwest::Client;

use crate::domain::errors::ProxyError;

/// Cliente HTTP **pinned**: el TCP se conecta a la IP ya validada por el anti-SSRF mientras la
/// URL conserva el hostname real, así reqwest envía el SNI, el `Host` y la verificación de
/// certificado correctos. Reescribir el host a la IP cruda omite el SNI y las CDNs (Cloudflare)
/// cortan el handshake, que arriba como un `502` opaco.
///
/// `redirect::Policy::none()` es obligatorio: seguir un `Location:` revalidaría un destino que
/// nadie autorizó y convertiría un host de la whitelist en un proxy hacia redes internas.
pub fn pinned_client(
    hostname: &str,
    resolved_ip: IpAddr,
    port: u16,
    timeout: Duration,
) -> Result<Client, ProxyError> {
    let pinned = SocketAddr::new(resolved_ip, port);

    Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .resolve(hostname, pinned)
        .build()
        .map_err(|e| ProxyError::Internal {
            reason: format!("Failed to build HTTP client: {e}"),
        })
}

/// Lee el cuerpo completo aplicando el tope **mientras llega** el stream: un origen que anuncia
/// un `Content-Length` pequeño y envía gigabytes no puede reservar memoria de más.
pub async fn read_body_capped(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, ProxyError> {
    if let Some(len) = response.content_length() {
        if len > max_bytes {
            return Err(ProxyError::PayloadTooLarge {
                content_length: len,
                max_allowed: max_bytes,
            });
        }
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| ProxyError::Internal {
        reason: format!("Failed to read response body: {e}"),
    })? {
        if (body.len() + chunk.len()) as u64 > max_bytes {
            return Err(ProxyError::PayloadTooLarge {
                content_length: (body.len() + chunk.len()) as u64,
                max_allowed: max_bytes,
            });
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}
