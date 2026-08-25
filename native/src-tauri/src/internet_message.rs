pub(crate) const MAX_MESSAGE_ID_BYTES: usize = 998;
pub(crate) const MAX_REFERENCES: usize = 20;
pub(crate) const MAX_REFERENCE_BYTES: usize = 8 * 1024;

pub(crate) fn canonicalize_message_id(value: &str) -> Result<String, ()> {
    let trimmed = value.trim();
    let canonical = if trimmed.starts_with('<') && trimmed.ends_with('>') {
        trimmed.to_string()
    } else {
        format!("<{trimmed}>")
    };
    validate_message_id(&canonical)?;
    Ok(canonical)
}

pub(crate) fn validate_message_id(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > MAX_MESSAGE_ID_BYTES
        || value.bytes().any(|byte| byte < b' ' || byte == 0x7f)
    {
        return Err(());
    }
    let Some(inner) = value
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
    else {
        return Err(());
    };
    let Some((left, right)) = inner.split_once('@') else {
        return Err(());
    };
    if left.is_empty()
        || right.is_empty()
        || right.contains('@')
        || !inner.is_ascii()
        || !is_message_id_left(left)
        || !is_message_id_right(right)
        || inner
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(());
    }
    Ok(())
}

pub(crate) fn validate_references(references: &[String]) -> Result<(), ()> {
    if references.len() > MAX_REFERENCES
        || references.iter().map(String::len).sum::<usize>() > MAX_REFERENCE_BYTES
        || references
            .iter()
            .any(|reference| validate_message_id(reference).is_err())
    {
        return Err(());
    }
    Ok(())
}

fn is_message_id_left(value: &str) -> bool {
    !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'.' | b'!'
                        | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'/'
                        | b'='
                        | b'?'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'{'
                        | b'|'
                        | b'}'
                        | b'~'
                )
        })
}

fn is_message_id_right(value: &str) -> bool {
    !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_parser_ids_and_rejects_unsafe_forms() {
        assert_eq!(
            canonicalize_message_id("message.1@example.test"),
            Ok("<message.1@example.test>".into())
        );
        assert_eq!(
            canonicalize_message_id(" <message.1@example.test> "),
            Ok("<message.1@example.test>".into())
        );
        for invalid in [
            "missing-at",
            "<two@@example.test>",
            "<space here@example.test>",
            "<line\r\nbreak@example.test>",
        ] {
            assert!(canonicalize_message_id(invalid).is_err());
        }
    }
}
