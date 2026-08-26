use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};

const MAX_REMOTE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LoadRemoteImageInput {
    pub message_id: i64,
    pub resource_id: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoadedRemoteImage {
    pub message_id: i64,
    pub resource_id: i64,
    pub data_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MessageRemoteContentInput {
    pub message_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DomainRemoteContentInput {
    pub message_id: i64,
    pub domain: String,
}

pub(crate) fn fetch_remote_image(
    image: crate::store::StoredRemoteImage,
) -> Result<LoadedRemoteImage, String> {
    let mut url = validate_url(&image.url)?;
    for redirect_count in 0..=MAX_REDIRECTS {
        let host = url
            .host_str()
            .ok_or_else(|| "Remote image URL is invalid".to_string())?
            .to_string();
        let address = resolve_public_address(&host)?;
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .resolve(&host, SocketAddr::new(address, 443))
            .build()
            .map_err(|_| "Remote image could not be loaded".to_string())?;
        let response = client
            .get(url.clone())
            .send()
            .map_err(|_| "Remote image could not be loaded".to_string())?;
        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err("Remote image redirect limit exceeded".into());
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "Remote image redirect was invalid".to_string())?;
            url = validated_redirect(&url, location)?;
            continue;
        }
        if !response.status().is_success() {
            return Err("Remote image could not be loaded".into());
        }
        let media_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| {
                matches!(
                    *value,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                )
            })
            .map(str::to_string)
            .ok_or_else(|| "Remote image content type is not allowed".to_string())?;
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > MAX_REMOTE_IMAGE_BYTES)
        {
            return Err("Remote image byte limit exceeded".into());
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_REMOTE_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Remote image could not be loaded".to_string())?;
        if bytes.len() as u64 > MAX_REMOTE_IMAGE_BYTES {
            return Err("Remote image byte limit exceeded".into());
        }
        return Ok(LoadedRemoteImage {
            message_id: image.message_id,
            resource_id: image.resource_id,
            data_url: format!("data:{media_type};base64,{}", BASE64_STANDARD.encode(bytes)),
        });
    }
    Err("Remote image could not be loaded".into())
}

fn validate_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Remote image URL is invalid".to_string())?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.host_str().is_none()
    {
        return Err("Remote image URL is not allowed".into());
    }
    Ok(url)
}

fn validated_redirect(from: &reqwest::Url, location: &str) -> Result<reqwest::Url, String> {
    let redirected = validate_url(
        from.join(location)
            .map_err(|_| "Remote image redirect was invalid".to_string())?
            .as_str(),
    )?;
    if redirected.host_str() != from.host_str() {
        return Err("Remote image cross-host redirect was denied".into());
    }
    Ok(redirected)
}

fn resolve_public_address(host: &str) -> Result<IpAddr, String> {
    let addresses = (host, 443)
        .to_socket_addrs()
        .map_err(|_| "Remote image host could not be resolved".to_string())?
        .map(|address| address.ip())
        .collect::<Vec<_>>();
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(*address)) {
        return Err("Remote image host is not public".into());
    }
    Ok(addresses[0])
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => {
            let [a, b, c, _] = value.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(value) => {
            // Any address that carries an IPv4 destination is judged as that IPv4 address,
            // so a AAAA record cannot smuggle a private target past the v4 rules above.
            if let Some(embedded) = value.to_ipv4_mapped().or_else(|| ipv4_compatible(value)) {
                return is_public_ip(IpAddr::V4(embedded));
            }
            let segments = value.segments();
            !(value.is_loopback()
                || value.is_unspecified()
                || value.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] & 0xffc0) == 0xfec0
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                // Transition, translation, and discard prefixes tunnel or drop an
                // attacker-chosen IPv4 destination rather than naming a public host.
                || segments[0] == 0x2002
                || (segments[0] == 0x2001 && segments[1] == 0x0000)
                || (segments[0] == 0x0064 && segments[1] == 0xff9b)
                || (segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0))
        }
    }
}

fn ipv4_compatible(value: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = value.segments();
    if segments[..6] != [0, 0, 0, 0, 0, 0] {
        return None;
    }
    Some(Ipv4Addr::from(
        (u32::from(segments[6]) << 16) | u32::from(segments[7]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_image_urls_and_private_networks_are_denied_before_fetch() {
        for url in [
            "http://example.com/image.png",
            "https://user:secret@example.com/image.png",
            "https://example.com:8443/image.png",
            "file:///tmp/image.png",
        ] {
            assert!(validate_url(url).is_err(), "accepted {url}");
        }
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "::10.0.0.1",
            "2002:7f00:1::",
            "2001:0:53aa:64c:2c:1e2f:5efe:a01",
            "64:ff9b::a00:1",
            "100::1",
        ] {
            assert!(
                !is_public_ip(address.parse().unwrap()),
                "accepted {address}"
            );
        }
        assert!(is_public_ip("93.184.216.34".parse().unwrap()));
        assert!(is_public_ip(
            "2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()
        ));
        let origin = validate_url("https://images.example.test/a.png").unwrap();
        assert_eq!(
            validated_redirect(&origin, "/b.png").unwrap().as_str(),
            "https://images.example.test/b.png"
        );
        assert!(validated_redirect(&origin, "https://tracker.example.test/b.png").is_err());
    }
}
