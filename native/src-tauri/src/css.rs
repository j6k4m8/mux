//! CSS filtering for message bodies.
//!
//! Message styles are rendered inside a sandboxed frame that cannot run script,
//! so the danger left in a stylesheet is not behaviour but reach: a declaration
//! that fetches a remote resource turns a rendered message into a read receipt.
//! Everything here exists to keep declarations descriptive and local.

const MAX_STYLESHEET_BYTES: usize = 128 * 1024;
const MAX_DECLARATION_BYTES: usize = 4 * 1024;
const MAX_NESTING: usize = 8;

/// Properties that only describe appearance. Anything able to position content
/// over the surrounding interface, animate, inject generated content, or name a
/// behaviour is absent by construction rather than by pattern match.
const ALLOWED_PROPERTIES: &[&str] = &[
    "background",
    "background-color",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-radius",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-top-style",
    "border-top-width",
    "border-width",
    "box-sizing",
    "caption-side",
    "clear",
    "color",
    "direction",
    "display",
    "empty-cells",
    "float",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-variant",
    "font-weight",
    "height",
    "letter-spacing",
    "line-height",
    "list-style",
    "list-style-position",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "opacity",
    "overflow",
    "overflow-wrap",
    "overflow-x",
    "overflow-y",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "table-layout",
    "text-align",
    "text-decoration",
    "text-indent",
    "text-transform",
    "text-wrap",
    "vertical-align",
    "visibility",
    "white-space",
    "width",
    "word-break",
    "word-spacing",
    "word-wrap",
];

/// At-rules that only group declarations. `@import` and `@font-face` are absent
/// because both name a resource to fetch.
const ALLOWED_AT_RULES: &[&str] = &["media", "supports"];

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FilteredCss {
    pub css: String,
    /// Declarations dropped because they named a remote resource. These are
    /// counted with the rest of a message's blocked remote content so the
    /// interface can say how much was withheld.
    pub blocked_remote_resources: i64,
}

/// A value Mux cannot read literally. CSS lets any identifier character be
/// written as a backslash escape, so `\75 rl(...)` is `url(...)` to the engine
/// and something else entirely to a substring match. This is not counted as
/// withheld remote content, because what it would have resolved to is exactly
/// what cannot be determined here.
fn value_carries_an_escape(value: &str) -> bool {
    value.contains('\\')
}

/// An unclosed string swallows everything after it, so the semicolons Mux would
/// split on are not declaration boundaries at all. Splitting anyway invents
/// declarations the sender never wrote, so a list that does not balance is not
/// read at all.
fn has_unbalanced_quotes(value: &str) -> bool {
    value.chars().filter(|c| *c == '"').count() % 2 == 1
        || value.chars().filter(|c| *c == '\'').count() % 2 == 1
}

/// True when a declaration value reaches outside the document. `url()` covers
/// images, fonts, and cursors alike; the rest are legacy script vectors that
/// cost nothing to keep out.
fn value_reaches_outward(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("url(")
        || lower.contains("image-set(")
        || lower.contains("expression(")
        || lower.contains("javascript:")
        || lower.contains("-moz-binding")
        || lower.contains("@import")
}

/// A declaration is kept only when its property is named in the allowlist and
/// its value is inert. Custom properties are dropped: their value is opaque
/// until `var()` resolves it somewhere else, so they cannot be checked here.
fn filtered_declaration(declaration: &str) -> Option<(String, bool)> {
    let declaration = declaration.trim();
    if declaration.is_empty() || declaration.len() > MAX_DECLARATION_BYTES {
        return None;
    }
    let (property, value) = declaration.split_once(':')?;
    let property = property.trim().to_ascii_lowercase();
    let value = value.trim();
    if value_carries_an_escape(value) {
        return None;
    }
    // Reach is checked before the property allowlist so a withheld fetch is
    // counted for the reader whatever property tried to make it.
    if value_reaches_outward(value) {
        return Some((String::new(), true));
    }
    if value.is_empty() || !ALLOWED_PROPERTIES.contains(&property.as_str()) {
        return None;
    }
    Some((format!("{property}: {value}"), false))
}

/// Filters the body of a `style` attribute.
pub(crate) fn filter_declarations(value: &str) -> FilteredCss {
    let value = strip_markup_delimiters(&strip_comments(value));
    if has_unbalanced_quotes(&value) {
        return FilteredCss::default();
    }
    let mut kept: Vec<String> = Vec::new();
    let mut blocked = 0;
    for declaration in value.split(';') {
        match filtered_declaration(declaration) {
            Some((_, true)) => blocked += 1,
            Some((text, false)) => kept.push(text),
            None => {}
        }
    }
    FilteredCss {
        css: kept.join("; "),
        blocked_remote_resources: blocked,
    }
}

/// Filters the text content of a `style` element, keeping selectors intact and
/// rewriting every declaration block through the same allowlist.
pub(crate) fn filter_stylesheet(value: &str) -> FilteredCss {
    if value.len() > MAX_STYLESHEET_BYTES {
        return FilteredCss::default();
    }
    let source = strip_markup_delimiters(&strip_comments(value));
    let mut output = String::new();
    let mut blocked = 0;
    filter_rules(&source, 0, &mut output, &mut blocked);
    FilteredCss {
        css: output,
        blocked_remote_resources: blocked,
    }
}

fn filter_rules(source: &str, depth: usize, output: &mut String, blocked: &mut i64) {
    if depth > MAX_NESTING {
        return;
    }
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut prelude_start = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => {
                let prelude = source[prelude_start..index].trim();
                let Some(end) = matching_brace(bytes, index) else {
                    return;
                };
                let block = &source[index + 1..end];
                emit_rule(prelude, block, depth, output, blocked);
                index = end + 1;
                prelude_start = index;
            }
            b'}' => {
                index += 1;
                prelude_start = index;
            }
            b';' => {
                // A statement rather than a block: `@import url(...)`, or junk.
                // None are kept, and only an import names something to fetch.
                let statement = &source[prelude_start..index];
                if statement.trim_start().starts_with('@')
                    && statement.to_ascii_lowercase().contains("import")
                {
                    *blocked += 1;
                }
                index += 1;
                prelude_start = index;
            }
            _ => index += 1,
        }
    }
}

fn emit_rule(prelude: &str, block: &str, depth: usize, output: &mut String, blocked: &mut i64) {
    if prelude.is_empty() {
        return;
    }
    if let Some(at_rule) = prelude.strip_prefix('@') {
        let name = at_rule
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !ALLOWED_AT_RULES.contains(&name.as_str()) {
            if name == "font-face" || name == "import" {
                *blocked += 1;
            }
            return;
        }
        let mut nested = String::new();
        filter_rules(block, depth + 1, &mut nested, blocked);
        if nested.is_empty() {
            return;
        }
        output.push_str(prelude);
        output.push('{');
        output.push_str(&nested);
        output.push('}');
        return;
    }

    let declarations = filter_declarations(block);
    *blocked += declarations.blocked_remote_resources;
    if declarations.css.is_empty() {
        return;
    }
    output.push_str(prelude);
    output.push('{');
    output.push_str(&declarations.css);
    output.push('}');
}

fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_comments(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("/*") {
        output.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return output,
        }
    }
    output.push_str(rest);
    output
}

/// A `style` element holds raw text, so a stylesheet containing `</style>` would
/// otherwise close its own element and return the rest of the message to the
/// HTML parser as markup. Angle brackets are meaningless in the CSS Mux keeps.
fn strip_markup_delimiters(value: &str) -> String {
    value.replace(['<', '>'], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_declarations_keep_appearance_and_drop_reach() {
        let filtered = filter_declarations(
            "color: #112233; POSITION: fixed; background-color:red; z-index: 99; font-size: 14px",
        );
        assert_eq!(
            filtered.css,
            "color: #112233; background-color: red; font-size: 14px"
        );
        assert_eq!(filtered.blocked_remote_resources, 0);

        let remote = filter_declarations("background: url(https://tracker.test/p.gif); color: red");
        assert_eq!(remote.css, "color: red");
        assert_eq!(remote.blocked_remote_resources, 1);
    }

    #[test]
    fn declarations_never_carry_behaviour_or_custom_properties() {
        for hostile in [
            "width: expression(alert(1))",
            "background-color: url(javascript:alert(1))",
            "-moz-binding: url(evil.xml)",
            "--leak: https://tracker.test",
            "behavior: url(#default#time2)",
        ] {
            assert!(
                !filter_declarations(hostile).css.contains(':'),
                "kept {hostile:?}"
            );
        }
    }

    #[test]
    fn stylesheets_keep_selectors_and_grouping_but_not_resources() {
        let filtered = filter_stylesheet(
            "@import url('https://tracker.test/a.css');
             .header { color: blue; position: absolute }
             @media (max-width: 600px) { .header { font-size: 12px } }
             @font-face { font-family: Spy; src: url(https://tracker.test/f.woff) }
             .empty { z-index: 4 }",
        );
        assert_eq!(
            filtered.css,
            ".header{color: blue}@media (max-width: 600px){.header{font-size: 12px}}"
        );
        assert_eq!(filtered.blocked_remote_resources, 2);
    }

    #[test]
    fn a_stylesheet_can_never_close_its_own_element() {
        let filtered = filter_stylesheet("p { color: red } </style><img src=x onerror=alert(1)>");
        assert!(!filtered.css.contains('<'));
        assert!(!filtered.css.contains('>'));
        assert_eq!(filtered.css, "p{color: red}");
    }

    #[test]
    fn an_escape_cannot_spell_a_property_or_a_fetch() {
        // `\75` is `u`, so this is `url(...)` by the time the engine reads it.
        assert_eq!(
            filter_declarations(r"background: \75 rl(https://tracker.test/x)").css,
            ""
        );
        assert_eq!(filter_declarations(r"color: re\64").css, "");
        // A value that opens a string and never closes it swallows what follows.
        let unbalanced = filter_declarations("font-family: \"}; color: red");
        assert_eq!(unbalanced.css, "");
        assert_eq!(unbalanced.blocked_remote_resources, 0);
        assert_eq!(
            filter_declarations("font-family: \"Helvetica Neue\"").css,
            "font-family: \"Helvetica Neue\""
        );
    }

    #[test]
    fn a_stray_statement_does_not_glue_itself_to_the_next_selector() {
        assert_eq!(
            filter_stylesheet("@charset \"utf-8\"; .a { color: red }").css,
            ".a{color: red}"
        );
    }

    #[test]
    fn comments_cannot_hide_a_blocked_property() {
        let filtered = filter_declarations("colo/*x*/r: red; background/**/-color: blue");
        assert_eq!(filtered.css, "color: red; background-color: blue");
    }

    #[test]
    fn oversized_and_unbalanced_input_is_bounded() {
        assert_eq!(
            filter_stylesheet(&"a".repeat(MAX_STYLESHEET_BYTES + 1)).css,
            ""
        );
        assert_eq!(filter_stylesheet(".a { color: red").css, "");
        assert_eq!(
            filter_declarations(&format!("color: {}", "x".repeat(MAX_DECLARATION_BYTES))).css,
            ""
        );
    }
}
