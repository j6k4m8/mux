use serde::Serialize;
use tauri::Url;

const MAX_EXTERNAL_DESTINATION_BYTES: usize = 2_048;
const PRODUCTION_SCHEME: &str = "tauri";
const PRODUCTION_HOST: &str = "localhost";
const DEV_SCHEME: &str = "http";
const DEV_HOST: &str = "127.0.0.1";
const DEV_PORT: u16 = 1_420;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalLinkDestination {
    pub destination: String,
    pub scheme: String,
    pub host: String,
}

pub fn normalize_external_destination(value: &str) -> Result<ExternalLinkDestination, String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_EXTERNAL_DESTINATION_BYTES
        || trimmed
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || contains_percent_encoded_control(trimmed.as_bytes())
    {
        return Err(denied_external_destination());
    }

    let url = Url::parse(trimmed).map_err(|_| denied_external_destination())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(denied_external_destination());
    }

    Ok(ExternalLinkDestination {
        destination: url.to_string(),
        scheme: url.scheme().to_string(),
        host: url.host_str().unwrap_or_default().to_string(),
    })
}

pub fn open_external_destination<F, E>(
    value: &str,
    opener: F,
) -> Result<ExternalLinkDestination, String>
where
    F: FnOnce(&str) -> Result<(), E>,
{
    let destination = normalize_external_destination(value)?;
    opener(&destination.destination).map_err(|_| "Mux could not open that link".to_string())?;
    Ok(destination)
}

/// The message reader renders into a sandboxed frame whose document is written
/// from the `srcdoc` attribute, which WebKit loads as its own navigation.
const SRCDOC_URL: &str = "about:srcdoc";

/// Nothing here may navigate to a remote destination. Production may load only
/// the exact bundled-app origin; development may additionally load the single
/// loopback Vite origin configured in tauri.conf.json; and either may load the
/// reader frame's own srcdoc document, whose content Mux itself wrote.
pub fn allows_webview_navigation(url: &Url) -> bool {
    let no_credentials = url.username().is_empty() && url.password().is_none();
    if !no_credentials {
        return false;
    }
    if url.as_str() == SRCDOC_URL {
        return true;
    }

    (url.scheme() == PRODUCTION_SCHEME
        && url.host_str() == Some(PRODUCTION_HOST)
        && url.port().is_none())
        || (url.scheme() == DEV_SCHEME
            && url.host_str() == Some(DEV_HOST)
            && url.port() == Some(DEV_PORT))
}

fn denied_external_destination() -> String {
    "Only absolute HTTP and HTTPS message links can be opened".to_string()
}

fn contains_percent_encoded_control(value: &[u8]) -> bool {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'%' {
            decoded.push(value[index]);
            index += 1;
            continue;
        }
        if index + 2 >= value.len() {
            return true;
        }
        let Some(high) = hex_value(value[index + 1]) else {
            return true;
        };
        let Some(low) = hex_value(value[index + 2]) else {
            return true;
        };
        decoded.push((high << 4) | low);
        index += 3;
    }
    std::str::from_utf8(&decoded)
        .map(|decoded| decoded.chars().any(char::is_control))
        .unwrap_or(true)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_destinations_are_absolute_bounded_and_normalized() {
        let normalized =
            normalize_external_destination("HTTPS://ExAmPle.com:443/a/../plan?q=hello#part")
                .unwrap();
        assert_eq!(
            normalized.destination,
            "https://example.com/plan?q=hello#part"
        );
        assert_eq!(normalized.scheme, "https");
        assert_eq!(normalized.host, "example.com");
        assert_eq!(
            normalize_external_destination("https://example.com/%E2%9C%93")
                .unwrap()
                .destination,
            "https://example.com/%E2%9C%93"
        );

        for denied in [
            "",
            "example.com/path",
            "//example.com/path",
            "mailto:person@example.com",
            "javascript:alert(1)",
            "java\nscript:alert(1)",
            "data:text/html,hello",
            "file:///etc/passwd",
            "blob:https://example.com/id",
            "https://user:secret@example.com/",
            "https://example.com/%0aheader",
            "https://example.com/%C2%85control",
            "https://example.com/%zz",
            "https://example.com/a b",
        ] {
            assert!(
                normalize_external_destination(denied).is_err(),
                "accepted {denied:?}"
            );
        }
        assert!(normalize_external_destination(&format!(
            "https://example.com/{}",
            "x".repeat(MAX_EXTERNAL_DESTINATION_BYTES)
        ))
        .is_err());
    }

    #[test]
    fn webview_navigation_is_an_exact_app_and_dev_origin_allowlist() {
        for allowed in [
            "tauri://localhost/",
            "tauri://localhost/assets/index.js",
            "http://127.0.0.1:1420/",
            "http://127.0.0.1:1420/src/main.ts",
            "about:srcdoc",
        ] {
            assert!(allows_webview_navigation(&Url::parse(allowed).unwrap()));
        }
        for denied in [
            "https://example.com/",
            "http://localhost:1420/",
            "http://127.0.0.1:1421/",
            "http://127.0.0.2:1420/",
            "http://127.0.0.1.evil.test:1420/",
            "https://127.0.0.1:1420/",
            "tauri://evil.test/",
            "tauri://user@localhost/",
            "file:///tmp/index.html",
            "data:text/html,hello",
            "about:blank",
            "about:srcdoc?x",
            "about:config",
        ] {
            assert!(
                !allows_webview_navigation(&Url::parse(denied).unwrap()),
                "allowed {denied:?}"
            );
        }
    }

    #[test]
    fn opener_receives_only_a_revalidated_normalized_http_destination() {
        let mut opened = Vec::new();
        let result =
            open_external_destination("HTTPS://Example.COM:443/a/../plan", |destination| {
                opened.push(destination.to_string());
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(result.destination, "https://example.com/plan");
        assert_eq!(opened, ["https://example.com/plan"]);

        let mut denied_opener_called = false;
        assert!(open_external_destination("javascript:alert(1)", |_| {
            denied_opener_called = true;
            Ok::<_, ()>(())
        })
        .is_err());
        assert!(!denied_opener_called);
    }
}
