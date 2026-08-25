use dom_query::{Document, NodeRef};

use crate::navigation::normalize_external_destination;

const MAX_HTML_INPUT_BYTES: usize = 1024 * 1024;
const MAX_HTML_TREE_DEPTH: usize = 64;
pub(crate) const MAX_HTML_TREE_NODES: usize = 50_000;
pub(crate) const MAX_SANITIZED_HTML_BYTES: usize = MAX_HTML_INPUT_BYTES * 8 + 1024;
const HTML_INPUT_LIMIT_ERROR: &str = "MIME ingestion rejected: decoded HTML byte limit exceeded";
const HTML_TREE_LIMIT_ERROR: &str = "MIME ingestion rejected: decoded HTML tree limit exceeded";
pub(crate) const MAX_REMOTE_IMAGES_PER_MESSAGE: usize = 64;
const MAX_REMOTE_IMAGE_URL_BYTES: usize = 2_048;
const MAX_REMOTE_IMAGE_ALT_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct ParsedAttachment {
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub content_id: String,
    pub disposition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMailbox {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone)]
pub struct SafeMessageContent {
    pub subject: String,
    pub from: Vec<ParsedMailbox>,
    pub reply_to: Vec<ParsedMailbox>,
    pub to: Vec<ParsedMailbox>,
    pub cc: Vec<ParsedMailbox>,
    pub internet_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub body_text: String,
    pub body_html: String,
    pub blocked_remote_resources: i64,
    pub(crate) remote_images: Vec<RemoteImageCandidate>,
    pub attachments: Vec<ParsedAttachment>,
}

#[derive(Debug, Clone)]
struct SanitizedHtml {
    html: String,
    text: String,
    blocked_remote_resources: i64,
    remote_images: Vec<RemoteImageCandidate>,
}

/// An inert remote image reference retained only inside the trusted Rust MIME
/// projection. The URL is deliberately absent from every frontend summary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteImageCandidate {
    pub resource_id: i64,
    pub url: String,
    pub domain: String,
    pub alt_text: String,
}

pub fn parse_mime(raw: &[u8]) -> Result<SafeMessageContent, String> {
    let ingested = crate::mime_ingest::ingest_mime(raw).map_err(|error| error.to_string())?;
    let subject = ingested.subject;
    let from = project_mailboxes(ingested.from);
    let reply_to = project_mailboxes(ingested.reply_to);
    let to = project_mailboxes(ingested.to);
    let cc = project_mailboxes(ingested.cc);
    let sanitized = match ingested.html.as_deref() {
        Some(html) => sanitize_html(html)?,
        None => {
            let text = ingested.plain.clone().unwrap_or_default();
            SanitizedHtml {
                // Plain-only mail stays plain. The Svelte renderer's existing
                // bodyText fallback escapes it without an expansive HTML copy.
                html: String::new(),
                text,
                blocked_remote_resources: 0,
                remote_images: Vec::new(),
            }
        }
    };
    let body_text = ingested
        .plain
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| sanitized.text.clone());
    let attachments = ingested
        .attachments
        .into_iter()
        .map(|attachment| ParsedAttachment {
            filename: safe_filename(&attachment.filename),
            media_type: attachment.media_type,
            bytes: attachment.bytes,
            content_id: attachment.content_id,
            disposition: attachment.disposition,
        })
        .collect();
    Ok(SafeMessageContent {
        subject,
        from,
        reply_to,
        to,
        cc,
        internet_message_id: ingested.internet_message_id,
        in_reply_to: ingested.in_reply_to,
        references: ingested.references,
        body_text,
        body_html: sanitized.html,
        blocked_remote_resources: sanitized.blocked_remote_resources,
        remote_images: sanitized.remote_images,
        attachments,
    })
}

pub(crate) fn validate_restricted_html(value: &str, blocked: i64) -> Result<(), String> {
    if !(0..=MAX_HTML_TREE_NODES as i64).contains(&blocked) {
        return Err("Restricted message HTML is outside its bounded contract".into());
    }
    // A hostile HTML body can consist entirely of blocked resource elements.
    // Its safe render fragment is then empty while the resource count remains
    // useful local metadata. Plain-only mail is the normal empty/zero case.
    if value.is_empty() {
        return Ok(());
    }
    if value.len() > MAX_SANITIZED_HTML_BYTES {
        return Err("Restricted message HTML is outside its bounded contract".into());
    }
    let sanitized = sanitize_html_with_markers(value, true)?;
    if sanitized.html != value {
        return Err("Restricted message HTML is not sanitizer-canonical".into());
    }
    Ok(())
}

pub(crate) fn validate_remote_images(
    html: &str,
    blocked: i64,
    images: &[RemoteImageCandidate],
) -> Result<(), String> {
    validate_restricted_html(html, blocked)?;
    if images.len() > MAX_REMOTE_IMAGES_PER_MESSAGE || blocked < images.len() as i64 {
        return Err("Restricted remote images are outside their bounded contract".into());
    }
    if html.matches("<mux-remote-image data-id=\"").count() != images.len() {
        return Err("Restricted remote image markers do not match their candidates".into());
    }
    for (index, image) in images.iter().enumerate() {
        let expected_id = index as i64 + 1;
        if image.resource_id != expected_id
            || image.url.len() > MAX_REMOTE_IMAGE_URL_BYTES
            || image.alt_text.chars().count() > MAX_REMOTE_IMAGE_ALT_CHARS
        {
            return Err("Restricted remote images are outside their bounded contract".into());
        }
        let url = reqwest::Url::parse(&image.url)
            .map_err(|_| "Restricted remote image URL is invalid".to_string())?;
        let domain = url
            .host_str()
            .map(|value| value.trim_end_matches('.').to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "Restricted remote image URL is invalid".to_string())?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
            || domain.parse::<std::net::IpAddr>().is_ok()
            || domain != image.domain
            || !html.contains(&format!(
                "<mux-remote-image data-id=\"{}\"></mux-remote-image>",
                image.resource_id
            ))
        {
            return Err("Restricted remote image URL is invalid".into());
        }
    }
    Ok(())
}

fn project_mailboxes(mailboxes: Vec<crate::mime_ingest::IngestedMailbox>) -> Vec<ParsedMailbox> {
    mailboxes
        .into_iter()
        .map(|mailbox| ParsedMailbox {
            name: mailbox.name,
            address: mailbox.address,
        })
        .collect()
}

/*
 * HTML is deliberately sanitized after MIME/charset/transfer decoding. The
 * standards parser and the renderer therefore never share authority: MIME
 * ingestion produces inert strings and bytes, and this module alone decides
 * the renderable subset.
 */

fn sanitize_html(value: &str) -> Result<SanitizedHtml, String> {
    sanitize_html_with_markers(value, false)
}

fn sanitize_html_with_markers(
    value: &str,
    preserve_trusted_markers: bool,
) -> Result<SanitizedHtml, String> {
    if value.len() > MAX_HTML_INPUT_BYTES {
        return Err(HTML_INPUT_LIMIT_ERROR.to_string());
    }
    let document = Document::fragment(value);
    let mut state = HtmlSanitizerState {
        preserve_trusted_markers,
        ..HtmlSanitizerState::default()
    };
    for node in document.root().children() {
        append_sanitized_node(node, 0, &mut state);
    }
    if state.limit_exceeded {
        return Err(HTML_TREE_LIMIT_ERROR.to_string());
    }

    let text = normalize_plain_text(&state.text);
    let html = if state.html.len() <= MAX_SANITIZED_HTML_BYTES {
        state.html
    } else {
        // This branch is defensive against an unexpectedly expansive parser
        // representation. It preserves inert readable text rather than ever
        // returning an unbounded or partially serialized element tree.
        plain_text_to_html(&text)
    };
    Ok(SanitizedHtml {
        html,
        text,
        blocked_remote_resources: state.blocked_remote_resources,
        remote_images: state.remote_images,
    })
}

#[derive(Default)]
struct HtmlSanitizerState {
    html: String,
    text: String,
    blocked_remote_resources: i64,
    remote_images: Vec<RemoteImageCandidate>,
    visited_nodes: usize,
    limit_exceeded: bool,
    preserve_trusted_markers: bool,
}

fn append_sanitized_node(node: NodeRef<'_>, depth: usize, state: &mut HtmlSanitizerState) {
    if state.limit_exceeded {
        return;
    }
    if state.visited_nodes >= MAX_HTML_TREE_NODES || depth > MAX_HTML_TREE_DEPTH {
        state.limit_exceeded = true;
        return;
    }
    state.visited_nodes += 1;

    if node.is_text() {
        let value = node.text();
        state.html.push_str(&escape_text(value.as_ref()));
        state.text.push_str(value.as_ref());
        return;
    }
    if !node.is_element() {
        for child in node.children() {
            append_sanitized_node(child, depth + 1, state);
        }
        return;
    }

    let name = node
        .node_name()
        .map(|value| value.to_string())
        .unwrap_or_default();
    let html_namespace = node
        .qual_name_ref()
        .is_some_and(|qualified| qualified.ns.as_ref() == "http://www.w3.org/1999/xhtml");
    if html_namespace && name == "img" {
        append_remote_image_marker(node, state);
        return;
    }
    if html_namespace && name == "mux-remote-image" && state.preserve_trusted_markers {
        if let Some(resource_id) = node
            .attr("data-id")
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|value| (1..=MAX_REMOTE_IMAGES_PER_MESSAGE as i64).contains(value))
        {
            state.html.push_str("<mux-remote-image data-id=\"");
            state.html.push_str(&resource_id.to_string());
            state.html.push_str("\"></mux-remote-image>");
            return;
        }
    }
    if !html_namespace || drops_entire_subtree(&name) {
        state.blocked_remote_resources = state
            .blocked_remote_resources
            .saturating_add(count_remote_resources(node, depth, state));
        return;
    }

    let tag = match name.as_str() {
        "b" => Some("strong"),
        "i" => Some("em"),
        "p" | "div" | "strong" | "em" | "u" | "ul" | "ol" | "li" | "blockquote" | "br" | "a" => {
            Some(name.as_str())
        }
        _ => None,
    };
    let Some(tag) = tag else {
        for child in node.children() {
            append_sanitized_node(child, depth + 1, state);
        }
        return;
    };

    if tag == "br" {
        state.html.push_str("<br>");
        append_plain_break(&mut state.text);
        return;
    }

    if tag == "a" {
        let destination = node
            .attr("href")
            .and_then(|href| normalize_external_destination(href.as_ref()).ok());
        if let Some(destination) = destination {
            state.html.push_str("<a href=\"");
            state
                .html
                .push_str(&escape_attribute(&destination.destination));
            state.html.push_str("\">");
            for child in node.children() {
                append_sanitized_node(child, depth + 1, state);
            }
            state.html.push_str("</a>");
        } else {
            for child in node.children() {
                append_sanitized_node(child, depth + 1, state);
            }
        }
        return;
    }

    state.html.push('<');
    state.html.push_str(tag);
    state.html.push('>');
    for child in node.children() {
        append_sanitized_node(child, depth + 1, state);
    }
    state.html.push_str("</");
    state.html.push_str(tag);
    state.html.push('>');
    if matches!(tag, "p" | "div" | "li" | "blockquote") {
        append_plain_break(&mut state.text);
    }
}

fn append_remote_image_marker(node: NodeRef<'_>, state: &mut HtmlSanitizerState) {
    if !element_names_remote_resource(node) {
        return;
    }
    state.blocked_remote_resources = state.blocked_remote_resources.saturating_add(1);
    let Some(src) = node.attr("src") else {
        return;
    };
    if !is_remote_resource(src.as_ref()) {
        return;
    }
    if state.remote_images.len() >= MAX_REMOTE_IMAGES_PER_MESSAGE
        || src.len() > MAX_REMOTE_IMAGE_URL_BYTES
    {
        return;
    }
    let Ok(url) = reqwest::Url::parse(src.trim()) else {
        return;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return;
    }
    let Some(domain) = url.host_str() else {
        return;
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || domain.parse::<std::net::IpAddr>().is_ok() {
        return;
    }
    let resource_id = state.remote_images.len() as i64 + 1;
    let alt_text = node
        .attr("alt")
        .map(|value| value.chars().take(MAX_REMOTE_IMAGE_ALT_CHARS).collect())
        .unwrap_or_default();
    state.html.push_str("<mux-remote-image data-id=\"");
    state.html.push_str(&resource_id.to_string());
    state.html.push_str("\"></mux-remote-image>");
    state.remote_images.push(RemoteImageCandidate {
        resource_id,
        url: url.to_string(),
        domain,
        alt_text,
    });
}

fn drops_entire_subtree(name: &str) -> bool {
    matches!(
        name,
        "applet"
            | "audio"
            | "base"
            | "button"
            | "canvas"
            | "embed"
            | "form"
            | "frame"
            | "frameset"
            | "head"
            | "iframe"
            | "img"
            | "input"
            | "link"
            | "math"
            | "meta"
            | "noscript"
            | "object"
            | "option"
            | "picture"
            | "script"
            | "select"
            | "source"
            | "style"
            | "svg"
            | "template"
            | "textarea"
            | "track"
            | "video"
    )
}

fn count_remote_resources(
    root: NodeRef<'_>,
    root_depth: usize,
    state: &mut HtmlSanitizerState,
) -> i64 {
    let mut count = i64::from(root.is_element() && element_names_remote_resource(root));
    let mut pending = root
        .children()
        .into_iter()
        .map(|child| (child, root_depth + 1))
        .collect::<Vec<_>>();
    while let Some((node, depth)) = pending.pop() {
        if state.visited_nodes >= MAX_HTML_TREE_NODES || depth > MAX_HTML_TREE_DEPTH {
            state.limit_exceeded = true;
            break;
        }
        state.visited_nodes += 1;
        if node.is_element() && element_names_remote_resource(node) {
            count = count.saturating_add(1);
        }
        pending.extend(node.children().into_iter().map(|child| (child, depth + 1)));
    }
    count
}

fn element_names_remote_resource(node: NodeRef<'_>) -> bool {
    let element_name = node
        .node_name()
        .map(|value| value.to_string())
        .unwrap_or_default();
    node.attrs().into_iter().any(|attribute| {
        let name = attribute.name.local.as_ref();
        let value = attribute.value.as_ref();
        let resource_attribute = matches!(
            name,
            "action" | "background" | "data" | "formaction" | "ping" | "poster" | "src" | "srcset"
        ) || (name == "href" && element_name != "a")
            || name == "style";
        resource_attribute && is_remote_resource(value)
    })
}

fn is_remote_resource(value: &str) -> bool {
    value.split(',').any(|candidate| {
        let lower = candidate.trim().to_ascii_lowercase();
        lower.starts_with("https://")
            || lower.starts_with("http://")
            || lower.starts_with("//")
            || lower.contains("url(https://")
            || lower.contains("url(http://")
            || lower.contains("url(//")
    })
}

fn append_plain_break(text: &mut String) {
    if !text.ends_with('\n') {
        text.push('\n');
    }
}

fn normalize_plain_text(value: &str) -> String {
    value
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attribute(value: &str) -> String {
    escape_text(value).replace('"', "&quot;")
}

fn plain_text_to_html(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(|paragraph| format!("<p>{}</p>", escape_text(paragraph).replace('\n', "<br>")))
        .collect()
}

fn safe_filename(value: &str) -> String {
    let filename = value
        .replace(['/', '\\', '\0', '\r', '\n'], "_")
        .trim()
        .chars()
        .take(180)
        .collect::<String>();
    if filename.is_empty() {
        "attachment".into()
    } else {
        filename
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_html_is_reduced_to_the_restricted_render_model() {
        let safe = sanitize_html(
            r#"<style>body{background:url(https://bad.test/x)}</style>
               <p onclick="steal()"><strong>Hello &amp; welcome</strong>
               <script>alert(1)</script><a href="javascript:alert(2)">bad</a>
               <a href="https://mux.example/plan" onmouseover="steal()">plan</a>
               <img src="https://tracker.example/pixel" onerror="steal()"></p>"#,
        )
        .expect("bounded HTML");
        assert_eq!(safe.blocked_remote_resources, 1);
        assert!(safe.html.contains("<strong>Hello &amp; welcome</strong>"));
        assert!(safe.html.contains("href=\"https://mux.example/plan\""));
        for forbidden in [
            "script",
            "style",
            "javascript:",
            "onclick",
            "onmouseover",
            "img",
            "tracker",
        ] {
            assert!(!safe.html.contains(forbidden), "found {forbidden}");
        }
    }

    #[test]
    fn remote_images_become_bounded_inert_markers_and_private_urls_never_cross_ipc() {
        let safe = sanitize_html(
            r#"<p>Before<img src="https://Images.Example.test/pixel.png?user=secret" alt="Receipt">after</p>
                <img src="http://insecure.example.test/x.png">
                <img src="https://127.0.0.1/private.png">"#,
        )
        .expect("bounded HTML");
        assert_eq!(safe.blocked_remote_resources, 3);
        assert_eq!(safe.remote_images.len(), 1);
        let image = &safe.remote_images[0];
        assert_eq!(image.resource_id, 1);
        assert_eq!(image.domain, "images.example.test");
        assert_eq!(image.alt_text, "Receipt");
        assert!(safe
            .html
            .contains("<mux-remote-image data-id=\"1\"></mux-remote-image>"));
        assert!(!safe.html.contains("pixel.png"));
        validate_remote_images(
            &safe.html,
            safe.blocked_remote_resources,
            &safe.remote_images,
        )
        .expect("trusted sidecar");
        let serialized = serde_json::to_string(&safe.remote_images).unwrap();
        assert!(serialized.contains("pixel.png"));
        assert!(!safe.html.contains(&serialized));
    }

    #[test]
    fn blocked_only_html_and_forged_candidate_mismatches_remain_safe() {
        let safe = sanitize_html(r#"<img src="https://tracker.example.test/p.gif">"#)
            .expect("blocked-only HTML");
        assert_eq!(safe.remote_images.len(), 1);
        assert!(!safe.html.is_empty());
        let mut forged = safe.remote_images.clone();
        forged[0].domain = "different.example.test".into();
        assert!(
            validate_remote_images(&safe.html, safe.blocked_remote_resources, &forged).is_err()
        );
        let hostile_marker = sanitize_html(
            "<mux-remote-image data-id=\"1\"></mux-remote-image><p>Still readable</p>",
        )
        .unwrap();
        assert!(!hostile_marker.html.contains("mux-remote-image"));
        assert!(hostile_marker.remote_images.is_empty());
    }

    #[test]
    fn html5_tree_builder_denies_active_foreign_form_and_resource_content() {
        let safe = sanitize_html(
            r#"<base href="https://base.test/">
               <meta http-equiv="refresh" content="0;url=https://refresh.test/">
               <p><b>Safe <i>formatting</b> survives malformed nesting</i>.</p>
               <a href="jav&#x61;script:alert(1)">encoded script</a>
               <a href="https://user:secret@example.test/">credential URL</a>
               <a href="HTTPS://Example.COM:443/a/../plan">normalized plan</a>
               <form action="https://post.test/"><input name="secret">form copy</form>
               <script src="https://script.test/x.js">script copy</script>
               <object data="https://object.test/x">object copy</object>
               <svg><image href="https://svg.test/tracker"></image><script>svg copy</script></svg>
               <img srcset="data:image/png,x 1x, https://tracker.test/x 2x">
               <style>@import url(https://style.test/x);</style>"#,
        )
        .expect("bounded HTML");

        assert!(safe.html.contains("Safe"));
        assert!(safe.html.contains("href=\"https://example.com/plan\""));
        assert_eq!(safe.blocked_remote_resources, 6);
        for forbidden in [
            "base.test",
            "refresh.test",
            "javascript",
            "user:secret",
            "form copy",
            "script copy",
            "object copy",
            "svg copy",
            "tracker.test",
            "style.test",
            "<form",
            "<img",
            "<script",
            "<style",
            "<svg",
        ] {
            assert!(!safe.html.contains(forbidden), "found {forbidden}");
        }
    }

    #[test]
    fn html_projection_has_explicit_input_depth_node_and_output_bounds() {
        // html5ever's fragment document contributes two synthetic container
        // nodes; the source below therefore reaches the exact traversal cap.
        let exact_node_cap = "<span></span>".repeat(MAX_HTML_TREE_NODES - 2);
        let safe = sanitize_html(&exact_node_cap).expect("exact node cap");
        assert!(safe.html.len() <= MAX_SANITIZED_HTML_BYTES);
        assert!(safe.text.len() <= MAX_HTML_INPUT_BYTES);

        let node_sentinel = "DO-NOT-ECHO-NODE-TAIL";
        let node_overflow = format!("{exact_node_cap}<span>{node_sentinel}</span>");
        let node_error = match sanitize_html(&node_overflow) {
            Ok(_) => panic!("HTML node overflow was accepted"),
            Err(error) => error,
        };
        assert_eq!(node_error, HTML_TREE_LIMIT_ERROR);
        assert!(!node_error.contains(node_sentinel));

        let exact_depth_cap = format!(
            "{}{}",
            "<div>".repeat(MAX_HTML_TREE_DEPTH - 1),
            "</div>".repeat(MAX_HTML_TREE_DEPTH - 1)
        );
        sanitize_html(&exact_depth_cap).expect("exact tree depth cap");

        let depth_sentinel = "DO-NOT-ECHO-DEPTH-TAIL";
        let depth_overflow = format!(
            "{}{}{}",
            "<div>".repeat(MAX_HTML_TREE_DEPTH),
            depth_sentinel,
            "</div>".repeat(MAX_HTML_TREE_DEPTH)
        );
        let depth_error = match sanitize_html(&depth_overflow) {
            Ok(_) => panic!("HTML depth overflow was accepted"),
            Err(error) => error,
        };
        assert_eq!(depth_error, HTML_TREE_LIMIT_ERROR);
        assert!(!depth_error.contains(depth_sentinel));
    }

    #[test]
    fn decoded_html_limit_accepts_the_cap_and_rejects_cap_plus_one_without_echoing() {
        let accepted_body = "x".repeat(MAX_HTML_INPUT_BYTES);
        let accepted_raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{accepted_body}"
        );
        let accepted = parse_mime(accepted_raw.as_bytes()).expect("exact HTML cap");
        assert_eq!(accepted.body_html.len(), MAX_HTML_INPUT_BYTES);
        assert_eq!(accepted.body_text.len(), MAX_HTML_INPUT_BYTES);

        let sentinel = "DO-NOT-ECHO-HOSTILE-TAIL";
        let rejected_body = format!(
            "{}{}",
            "x".repeat(MAX_HTML_INPUT_BYTES + 1 - sentinel.len()),
            sentinel
        );
        assert_eq!(rejected_body.len(), MAX_HTML_INPUT_BYTES + 1);
        let rejected_raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{rejected_body}"
        );
        let error = match parse_mime(rejected_raw.as_bytes()) {
            Ok(_) => panic!("oversized decoded HTML was accepted"),
            Err(error) => error,
        };
        assert_eq!(error, HTML_INPUT_LIMIT_ERROR);
        assert!(!error.contains(sentinel));
    }

    #[test]
    fn large_plain_only_mime_preserves_escaped_tail_without_an_html_copy() {
        const PLAIN_BODY_BYTES: usize = MAX_HTML_INPUT_BYTES * 2;
        let sentinel = "PLAIN-TAIL-MUST-SURVIVE";
        let plain_body = format!(
            "{}{}",
            "&".repeat(PLAIN_BODY_BYTES - sentinel.len()),
            sentinel
        );
        assert_eq!(plain_body.len(), PLAIN_BODY_BYTES);
        let raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{plain_body}"
        );
        let parsed = parse_mime(raw.as_bytes()).expect("bounded large plain MIME");
        assert!(parsed.body_html.is_empty());
        assert_eq!(parsed.body_text.len(), PLAIN_BODY_BYTES);
        assert!(parsed.body_text.ends_with(sentinel));
    }

    #[test]
    fn multipart_mime_extracts_safe_body_inline_image_and_attachment() {
        let raw = concat!(
            "MIME-Version: 1.0\r\n",
            "Subject: =?UTF-8?Q?Design_=E2=9C=93?=\r\n",
            "From: =?UTF-8?Q?Jos=C3=A9?= <jose@example.test>\r\n",
            "To: team@example.test\r\n",
            "Message-ID: <design-3@example.test>\r\n",
            "In-Reply-To: <design-2@example.test>\r\n",
            "References: <design-1@example.test> <design-2@example.test>\r\n",
            "Content-Type: multipart/related; boundary=outer\r\n\r\n",
            "--outer\r\n",
            "Content-Type: multipart/alternative; boundary=inner\r\n\r\n",
            "--inner\r\nContent-Type: text/plain\r\n\r\nHello from plain text.\r\n",
            "--inner\r\nContent-Type: text/html\r\n\r\n<p><strong>Hello</strong> from HTML.<img src=\"https://tracker.test/p\"><img src=\"cid:chart\"></p>\r\n",
            "--inner--\r\n",
            "--outer\r\nContent-Type: image/png; name=\"chart.png\"\r\nContent-Disposition: inline; filename=\"chart.png\"\r\nContent-ID: <chart>\r\nContent-Transfer-Encoding: base64\r\n\r\naW1hZ2U=\r\n",
            "--outer\r\nContent-Type: text/plain; name=\"notes.txt\"\r\nContent-Disposition: attachment; filename=\"../notes.txt\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nReview=20notes\r\n",
            "--outer--\r\n"
        );
        let parsed = parse_mime(raw.as_bytes()).expect("safe MIME projection");
        assert_eq!(parsed.subject, "Design ✓");
        assert_eq!(parsed.from[0].name, "José");
        assert_eq!(parsed.from[0].address, "jose@example.test");
        assert_eq!(parsed.to[0].address, "team@example.test");
        assert!(parsed.reply_to.is_empty());
        assert!(parsed.cc.is_empty());
        assert_eq!(
            parsed.internet_message_id.as_deref(),
            Some("<design-3@example.test>")
        );
        assert_eq!(
            parsed.in_reply_to.as_deref(),
            Some("<design-2@example.test>")
        );
        assert_eq!(
            parsed.references,
            ["<design-1@example.test>", "<design-2@example.test>"]
        );
        assert_eq!(parsed.body_text.trim(), "Hello from plain text.");
        assert!(parsed.body_html.contains("<strong>Hello</strong>"));
        assert_eq!(parsed.blocked_remote_resources, 1);
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].content_id, "chart");
        assert_eq!(parsed.attachments[0].bytes, b"image");
        assert_eq!(parsed.attachments[1].filename, ".._notes.txt");
        assert_eq!(parsed.attachments[1].bytes, b"Review notes");
    }

    #[test]
    fn threading_headers_reject_malformed_ids_and_bound_reference_history() {
        let references = (0..25)
            .map(|index| format!("<reference-{index}@example.test>"))
            .collect::<Vec<_>>();
        let raw = format!(
            "Message-ID: <current@example.test>\r\nReferences: {}\r\n\r\nBody",
            references.join(" ")
        );
        let parsed = parse_mime(raw.as_bytes()).expect("bounded References chain");
        assert_eq!(parsed.references.len(), 20);
        assert_eq!(parsed.references.first(), Some(&references[5]));
        assert_eq!(parsed.references.last(), Some(&references[24]));

        let large_references = (0..12)
            .map(|index| format!("<{}-{index}@example.test>", "r".repeat(850)))
            .collect::<Vec<_>>();
        let raw = format!(
            "Message-ID: <current@example.test>\r\nReferences: {}\r\n\r\nBody",
            large_references.join(" ")
        );
        let parsed = parse_mime(raw.as_bytes()).expect("byte-bounded References chain");
        assert!(
            parsed.references.iter().map(String::len).sum::<usize>()
                <= crate::internet_message::MAX_REFERENCE_BYTES
        );
        assert_eq!(parsed.references.last(), large_references.last());
        assert!(parsed.references.len() < large_references.len());

        let malformed = b"Message-ID: missing-at\r\n\r\nBody";
        assert!(parse_mime(malformed).is_err());
    }

    #[test]
    fn malformed_multipart_is_rejected() {
        let malformed = b"Content-Type: multipart/mixed\r\n\r\nmissing boundary";
        assert!(parse_mime(malformed).is_err());
    }
}
