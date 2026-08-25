use rusqlite::types::Value;

const MAX_QUERY_LENGTH: usize = 2_048;
const MAX_TOKENS: usize = 256;
const MAX_NESTING: usize = 32;
const DAY_MS: i64 = 86_400_000;
const MAX_TIMEZONE_OFFSET_MINUTES: i32 = 14 * 60;
const FIELD_NAMES: [&str; 14] = [
    "from", "to", "subject", "account", "in", "label", "category", "is", "has", "after", "before",
    "date", "filename", "domain",
];

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Term(String),
    And,
    Or,
    Not,
    Left,
    Right,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    position: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum Expression {
    Text(String),
    Field(String, String),
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
}

#[derive(Debug)]
pub struct CompiledSearch {
    pub clause: String,
    pub parameters: Vec<Value>,
}

pub fn compile_search(
    input: &str,
    now_ms: i64,
    timezone_offset_minutes: i32,
) -> Result<CompiledSearch, String> {
    validate_timezone_offset(timezone_offset_minutes)?;
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Ok(CompiledSearch {
            clause: "1 = 1".into(),
            parameters: Vec::new(),
        });
    }
    let mut parser = Parser {
        source_length: input.trim().chars().count(),
        tokens,
        cursor: 0,
        nesting: 0,
    };
    let expression = parser.parse_or()?;
    if parser.cursor != parser.tokens.len() {
        return Err(format!(
            "Unexpected token at character {}",
            parser.tokens[parser.cursor].position
        ));
    }
    let mut compiled = CompiledSearch {
        clause: String::new(),
        parameters: Vec::new(),
    };
    compiled.clause = compile_expression(
        &expression,
        &mut compiled.parameters,
        now_ms,
        timezone_offset_minutes,
    )?;
    Ok(compiled)
}

fn validate_timezone_offset(timezone_offset_minutes: i32) -> Result<(), String> {
    if (-MAX_TIMEZONE_OFFSET_MINUTES..=MAX_TIMEZONE_OFFSET_MINUTES)
        .contains(&timezone_offset_minutes)
    {
        Ok(())
    } else {
        Err(format!(
            "Timezone offset must be between -{MAX_TIMEZONE_OFFSET_MINUTES} and {MAX_TIMEZONE_OFFSET_MINUTES} minutes"
        ))
    }
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let source = input.trim();
    let source_length = source.chars().count();
    if source_length > MAX_QUERY_LENGTH {
        return Err(format!(
            "Search query exceeds {MAX_QUERY_LENGTH} characters"
        ));
    }
    let characters = source.chars().enumerate().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < characters.len() {
        let (character_position, character) = characters[cursor];
        if character.is_whitespace() {
            cursor += 1;
            continue;
        }
        if character == '(' || character == ')' {
            tokens.push(Token {
                kind: if character == '(' {
                    TokenKind::Left
                } else {
                    TokenKind::Right
                },
                position: character_position,
            });
            cursor += 1;
            continue;
        }

        let start = character_position;
        let mut value = String::new();
        let mut quote = None;
        while cursor < characters.len() {
            let (_, current) = characters[cursor];
            if let Some(active_quote) = quote {
                if current == active_quote {
                    quote = None;
                    cursor += 1;
                    continue;
                }
                if current == '\\' && cursor + 1 < characters.len() {
                    value.push(characters[cursor + 1].1);
                    cursor += 2;
                    continue;
                }
                value.push(current);
                cursor += 1;
                continue;
            }
            if current == '"' || current == '\'' {
                quote = Some(current);
                cursor += 1;
                continue;
            }
            if current.is_whitespace() || current == '(' || current == ')' {
                break;
            }
            value.push(current);
            cursor += 1;
        }
        if quote.is_some() {
            return Err(format!("Unterminated quoted value at character {start}"));
        }
        if value.is_empty() {
            return Err(format!("Unexpected token at character {start}"));
        }
        let kind = match value.to_ascii_uppercase().as_str() {
            "AND" => TokenKind::And,
            "OR" => TokenKind::Or,
            "NOT" => TokenKind::Not,
            _ => TokenKind::Term(value),
        };
        tokens.push(Token {
            kind,
            position: start,
        });
        if tokens.len() > MAX_TOKENS {
            return Err(format!("Search query exceeds {MAX_TOKENS} tokens"));
        }
    }
    Ok(tokens)
}

struct Parser {
    source_length: usize,
    tokens: Vec<Token>,
    cursor: usize,
    nesting: usize,
}

impl Parser {
    fn parse_or(&mut self) -> Result<Expression, String> {
        let mut expression = self.parse_and()?;
        while self.take(&TokenKind::Or) {
            expression = Expression::Or(Box::new(expression), Box::new(self.parse_and()?));
        }
        Ok(expression)
    }

    fn parse_and(&mut self) -> Result<Expression, String> {
        let mut expression = self.parse_not()?;
        loop {
            if self.take(&TokenKind::And) {
                expression = Expression::And(Box::new(expression), Box::new(self.parse_not()?));
                continue;
            }
            if self.can_start_expression() {
                expression = Expression::And(Box::new(expression), Box::new(self.parse_not()?));
                continue;
            }
            break;
        }
        Ok(expression)
    }

    fn parse_not(&mut self) -> Result<Expression, String> {
        if self.take(&TokenKind::Not) {
            return Ok(Expression::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expression, String> {
        if self.take(&TokenKind::Left) {
            self.nesting += 1;
            if self.nesting > MAX_NESTING {
                return Err(format!("Search nesting exceeds {MAX_NESTING} levels"));
            }
            let expression = self.parse_or()?;
            self.nesting -= 1;
            if !self.take(&TokenKind::Right) {
                return Err(format!(
                    "Expected closing parenthesis at character {}",
                    self.current_position()
                ));
            }
            return Ok(expression);
        }
        let token = self.tokens.get(self.cursor).cloned().ok_or_else(|| {
            format!(
                "Expected a search term at character {}",
                self.current_position()
            )
        })?;
        let TokenKind::Term(value) = token.kind else {
            return Err(format!(
                "Expected a search term at character {}",
                token.position
            ));
        };
        self.cursor += 1;
        if let Some((field, value)) = value.split_once(':') {
            let field = field.to_ascii_lowercase();
            if FIELD_NAMES.contains(&field.as_str()) && value.is_empty() {
                return Err(format!(
                    "Missing value for {field}: at character {}",
                    token.position
                ));
            }
            if FIELD_NAMES.contains(&field.as_str()) {
                return Ok(Expression::Field(field, value.to_string()));
            }
        }
        Ok(Expression::Text(value))
    }

    fn take(&mut self, expected: &TokenKind) -> bool {
        if self.tokens.get(self.cursor).map(|token| &token.kind) == Some(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn can_start_expression(&self) -> bool {
        matches!(
            self.tokens.get(self.cursor).map(|token| &token.kind),
            Some(TokenKind::Term(_)) | Some(TokenKind::Not) | Some(TokenKind::Left)
        )
    }

    fn current_position(&self) -> usize {
        self.tokens
            .get(self.cursor)
            .map(|token| token.position)
            .unwrap_or(self.source_length)
    }
}

fn compile_expression(
    expression: &Expression,
    parameters: &mut Vec<Value>,
    now_ms: i64,
    timezone_offset_minutes: i32,
) -> Result<String, String> {
    match expression {
        Expression::And(left, right) => Ok(format!(
            "({} AND {})",
            compile_expression(left, parameters, now_ms, timezone_offset_minutes)?,
            compile_expression(right, parameters, now_ms, timezone_offset_minutes)?
        )),
        Expression::Or(left, right) => Ok(format!(
            "({} OR {})",
            compile_expression(left, parameters, now_ms, timezone_offset_minutes)?,
            compile_expression(right, parameters, now_ms, timezone_offset_minutes)?
        )),
        Expression::Not(child) => Ok(format!(
            "NOT ({})",
            compile_expression(child, parameters, now_ms, timezone_offset_minutes)?
        )),
        Expression::Text(value) => {
            parameters.push(Value::Text(fts_literal(value)?));
            Ok("e.id IN (SELECT CAST(thread_id AS INTEGER) FROM messages_fts WHERE messages_fts MATCH ?)".into())
        }
        Expression::Field(field, value) => {
            compile_field(field, value, parameters, now_ms, timezone_offset_minutes)
        }
    }
}

fn compile_field(
    field: &str,
    value: &str,
    parameters: &mut Vec<Value>,
    now_ms: i64,
    timezone_offset_minutes: i32,
) -> Result<String, String> {
    let normalized_value = value.trim().to_lowercase();
    let like_value = || {
        Value::Text(format!(
            "%{}%",
            normalized_value.replace('%', "\\%").replace('_', "\\_")
        ))
    };
    match field {
        "from" => {
            parameters.push(like_value());
            parameters.push(like_value());
            Ok("EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = e.id AND m.remote_deleted = 0 AND (LOWER(m.sender_name) LIKE ? ESCAPE '\\' OR LOWER(m.sender_email) LIKE ? ESCAPE '\\'))".into())
        }
        "to" => {
            parameters.push(like_value());
            parameters.push(like_value());
            parameters.push(like_value());
            Ok("EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = e.id AND m.remote_deleted = 0 AND (LOWER(m.recipients) LIKE ? ESCAPE '\\' OR LOWER(m.cc_recipients) LIKE ? ESCAPE '\\' OR LOWER(m.bcc_recipients) LIKE ? ESCAPE '\\'))".into())
        }
        "subject" => {
            parameters.push(like_value());
            Ok("LOWER(e.subject) LIKE ? ESCAPE '\\'".into())
        }
        "account" => {
            parameters.push(like_value());
            parameters.push(like_value());
            parameters.push(like_value());
            Ok("EXISTS (SELECT 1 FROM accounts a WHERE a.id = e.account_id AND (LOWER(a.id) LIKE ? ESCAPE '\\' OR LOWER(a.name) LIKE ? ESCAPE '\\' OR LOWER(a.email) LIKE ? ESCAPE '\\'))".into())
        }
        "label" | "category" => {
            parameters.push(like_value());
            Ok("LOWER(e.category) LIKE ? ESCAPE '\\'".into())
        }
        "filename" => {
            parameters.push(like_value());
            Ok("LOWER(e.attachment_names) LIKE ? ESCAPE '\\'".into())
        }
        "domain" => {
            let domain = normalized_value
                .trim_start_matches('@')
                .replace('%', "\\%")
                .replace('_', "\\_");
            for _ in 0..4 {
                parameters.push(Value::Text(format!("%@{domain}%")));
            }
            Ok("EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = e.id AND m.remote_deleted = 0 AND (LOWER(m.sender_email) LIKE ? ESCAPE '\\' OR LOWER(m.recipients) LIKE ? ESCAPE '\\' OR LOWER(m.cc_recipients) LIKE ? ESCAPE '\\' OR LOWER(m.bcc_recipients) LIKE ? ESCAPE '\\'))".into())
        }
        "is" => match normalized_value.as_str() {
            "unread" => Ok("e.unread = 1".into()),
            "read" => Ok("e.unread = 0".into()),
            "starred" => Ok("e.starred = 1".into()),
            "unstarred" => Ok("e.starred = 0".into()),
            "snoozed" => {
                parameters.push(Value::Integer(now_ms));
                Ok("s.wake_at > ?".into())
            }
            "sent" | "me" => Ok("e.has_from_me = 1".into()),
            other => Err(format!("Unsupported is: value '{other}'")),
        },
        "has" => match normalized_value.as_str() {
            "attachment" => Ok("e.has_attachment = 1".into()),
            "invite" | "calendar" => Ok("e.has_invite = 1".into()),
            "link" => Ok("e.has_link = 1".into()),
            other => Err(format!("Unsupported has: value '{other}'")),
        },
        "in" => match normalized_value.as_str() {
            "inbox" => Ok("e.in_inbox = 1".into()),
            "archive" => Ok("e.in_inbox = 0".into()),
            "sent" => Ok("e.has_from_me = 1".into()),
            "all" => Ok("1 = 1".into()),
            "snoozed" => {
                parameters.push(Value::Integer(now_ms));
                Ok("s.wake_at > ?".into())
            }
            other => Err(format!("Unsupported in: value '{other}'")),
        },
        "after" | "before" => {
            let timestamp = parse_date(value, now_ms, timezone_offset_minutes)?;
            parameters.push(Value::Integer(timestamp));
            Ok(format!(
                "e.latest_at {} ?",
                if field == "after" { ">=" } else { "<" }
            ))
        }
        "date" => {
            let start = parse_date(value, now_ms, timezone_offset_minutes)?;
            let end = start
                .checked_add(DAY_MS)
                .ok_or_else(|| format!("Unsupported date value '{value}'"))?;
            parameters.push(Value::Integer(start));
            parameters.push(Value::Integer(end));
            Ok("(e.latest_at >= ? AND e.latest_at < ?)".into())
        }
        other => Err(format!("Unsupported search field '{other}:'")),
    }
}

fn parse_date(value: &str, now_ms: i64, timezone_offset_minutes: i32) -> Result<i64, String> {
    let unsupported = || format!("Unsupported date value '{value}'");
    let normalized = normalize_date_value(value);
    // The browser supplies its current fixed offset so local calendar dates do
    // not silently become UTC dates. A fixed offset cannot model a DST change
    // between `now` and a different date; that requires timezone rules.
    let timezone_offset_ms = i64::from(timezone_offset_minutes)
        .checked_mul(60_000)
        .ok_or_else(&unsupported)?;
    let local_now_ms = now_ms
        .checked_add(timezone_offset_ms)
        .ok_or_else(&unsupported)?;
    let today = local_now_ms
        .checked_sub(local_now_ms.rem_euclid(DAY_MS))
        .and_then(|local_midnight| local_midnight.checked_sub(timezone_offset_ms))
        .ok_or_else(&unsupported)?;
    match normalized.as_str() {
        "today" => return Ok(today),
        "yesterday" => {
            return today.checked_sub(DAY_MS).ok_or_else(&unsupported);
        }
        "last-week" => {
            return subtract_days(now_ms, 7).ok_or_else(&unsupported);
        }
        "last-month" => {
            return subtract_calendar_months(local_now_ms, 1)
                .and_then(|local| local.checked_sub(timezone_offset_ms))
                .ok_or_else(&unsupported);
        }
        "last-year" => {
            return subtract_calendar_years(local_now_ms, 1)
                .and_then(|local| local.checked_sub(timezone_offset_ms))
                .ok_or_else(&unsupported);
        }
        _ => {}
    }

    for unit in ["mo", "d", "w", "y"] {
        let Some(number) = normalized.strip_suffix(unit) else {
            continue;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let amount = number.parse::<i64>().map_err(|_| unsupported())?;
        if amount > 100_000 {
            return Err(unsupported());
        }
        let timestamp = match unit {
            "d" => subtract_days(now_ms, amount),
            "w" => amount
                .checked_mul(7)
                .and_then(|days| subtract_days(now_ms, days)),
            "mo" => subtract_calendar_months(local_now_ms, amount)
                .and_then(|local| local.checked_sub(timezone_offset_ms)),
            "y" => subtract_calendar_years(local_now_ms, amount)
                .and_then(|local| local.checked_sub(timezone_offset_ms)),
            _ => unreachable!(),
        };
        return timestamp.ok_or_else(&unsupported);
    }

    iso_date_to_millis(&normalized)
        .and_then(|local_midnight| local_midnight.checked_sub(timezone_offset_ms))
        .ok_or_else(unsupported)
}

fn iso_date_to_millis(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let year = value[0..4].parse::<i64>().ok()?;
    let month = value[5..7].parse::<i64>().ok()?;
    let day = value[8..10].parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)?).contains(&day) {
        return None;
    }
    civil_date_to_millis(year, month, day, 0)
}

fn normalize_date_value(value: &str) -> String {
    let mut normalized = String::new();
    let mut replacing_separator = false;
    for character in value.trim().chars().flat_map(char::to_lowercase) {
        if character.is_whitespace() || character == '_' {
            if !replacing_separator {
                normalized.push('-');
            }
            replacing_separator = true;
        } else {
            normalized.push(character);
            replacing_separator = false;
        }
    }
    normalized
}

fn subtract_days(timestamp: i64, days: i64) -> Option<i64> {
    timestamp.checked_sub(days.checked_mul(DAY_MS)?)
}

fn subtract_calendar_months(timestamp: i64, months: i64) -> Option<i64> {
    let day_number = timestamp.div_euclid(DAY_MS);
    let time_of_day = timestamp.rem_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(day_number);
    let month_index = year
        .checked_mul(12)?
        .checked_add(month.checked_sub(1)?)?
        .checked_sub(months)?;
    let target_year = month_index.div_euclid(12);
    let target_month = month_index.rem_euclid(12) + 1;
    civil_date_to_millis(target_year, target_month, day, time_of_day)
}

fn subtract_calendar_years(timestamp: i64, years: i64) -> Option<i64> {
    let day_number = timestamp.div_euclid(DAY_MS);
    let time_of_day = timestamp.rem_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(day_number);
    civil_date_to_millis(year.checked_sub(years)?, month, day, time_of_day)
}

fn civil_date_to_millis(year: i64, month: i64, day: i64, time_of_day: i64) -> Option<i64> {
    days_from_civil(year, month, day)
        .checked_mul(DAY_MS)?
        .checked_add(time_of_day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // The caller may deliberately pass a day beyond the target month's end to
    // preserve JavaScript Date's calendar-month/year rollover behavior.
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn days_in_month(year: i64, month: i64) -> Option<i64> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 if is_leap_year(year) => Some(29),
        2 => Some(28),
        _ => None,
    }
}

fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

fn fts_literal(value: &str) -> Result<String, String> {
    let clean = value.trim().replace('"', "\"\"");
    if clean.is_empty() {
        return Err("Empty full-text term".into());
    }
    Ok(format!("\"{clean}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_search(input: &str, now_ms: i64) -> Result<CompiledSearch, String> {
        super::compile_search(input, now_ms, 0)
    }

    fn parse_date(value: &str, now_ms: i64) -> Result<i64, String> {
        super::parse_date(value, now_ms, 0)
    }

    fn parse_expression(input: &str) -> Result<Expression, String> {
        let tokens = tokenize(input)?;
        let mut parser = Parser {
            source_length: input.trim().chars().count(),
            tokens,
            cursor: 0,
            nesting: 0,
        };
        let expression = parser.parse_or()?;
        if parser.cursor != parser.tokens.len() {
            return Err(format!(
                "Unexpected token at character {}",
                parser.tokens[parser.cursor].position
            ));
        }
        Ok(expression)
    }

    fn timestamp(value: &str) -> i64 {
        iso_date_to_millis(value).expect("valid test date")
    }

    #[test]
    fn tokenizer_preserves_exact_quotes_parentheses_and_character_positions() {
        let tokens = tokenize("from:alice AND (subject:\"Q4 report\" OR has:attachment)").unwrap();
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.kind.clone())
                .collect::<Vec<_>>(),
            vec![
                TokenKind::Term("from:alice".into()),
                TokenKind::And,
                TokenKind::Left,
                TokenKind::Term("subject:Q4 report".into()),
                TokenKind::Or,
                TokenKind::Term("has:attachment".into()),
                TokenKind::Right,
            ]
        );
        assert_eq!(
            tokens
                .iter()
                .map(|token| token.position)
                .collect::<Vec<_>>(),
            vec![0, 11, 15, 16, 36, 39, 53]
        );
        assert_eq!(
            tokenize("subject:'a \\'quoted\\' value'").unwrap()[0].kind,
            TokenKind::Term("subject:a 'quoted' value".into())
        );
    }

    #[test]
    fn parser_applies_not_before_and_before_or_with_implicit_and() {
        assert_eq!(
            parse_expression("alpha OR beta gamma").unwrap(),
            Expression::Or(
                Box::new(Expression::Text("alpha".into())),
                Box::new(Expression::And(
                    Box::new(Expression::Text("beta".into())),
                    Box::new(Expression::Text("gamma".into())),
                )),
            )
        );
        assert_eq!(
            parse_expression("NOT alpha beta").unwrap(),
            Expression::And(
                Box::new(Expression::Not(Box::new(Expression::Text("alpha".into())))),
                Box::new(Expression::Text("beta".into())),
            )
        );
    }

    #[test]
    fn malformed_queries_report_exact_unicode_character_positions() {
        assert_eq!(
            compile_search("(from:alice", 0).unwrap_err(),
            "Expected closing parenthesis at character 11"
        );
        assert_eq!(
            compile_search("subject:\"unterminated", 0).unwrap_err(),
            "Unterminated quoted value at character 0"
        );
        assert_eq!(
            compile_search("from:", 0).unwrap_err(),
            "Missing value for from: at character 0"
        );
        assert_eq!(
            compile_search("é OR )", 0).unwrap_err(),
            "Expected a search term at character 5"
        );
    }

    #[test]
    fn compiler_parameterizes_sender_recipient_and_structured_fields() {
        let compiled = compile_search(
            "from:Alice to:TEAM subject:\"Q4 Report\" has:attachment is:unread",
            1_800_000_000_000,
        )
        .unwrap();
        assert!(compiled.clause.contains("LOWER(m.sender_name) LIKE ?"));
        assert!(compiled.clause.contains("LOWER(m.sender_email) LIKE ?"));
        assert!(!compiled.clause.contains("e.participants LIKE"));
        assert!(compiled.clause.contains("LOWER(m.recipients) LIKE ?"));
        assert!(compiled.clause.contains("LOWER(m.cc_recipients) LIKE ?"));
        assert!(compiled.clause.contains("LOWER(m.bcc_recipients) LIKE ?"));
        assert_eq!(
            compiled.clause.matches("m.remote_deleted = 0").count(),
            2,
            "sender and recipient predicates ignore tombstoned messages"
        );
        assert!(compiled.clause.contains("LOWER(e.subject) LIKE ?"));
        assert!(compiled.clause.contains("e.has_attachment = 1"));
        assert!(compiled.clause.contains("e.unread = 1"));
        assert_eq!(
            compiled.parameters,
            vec![
                Value::Text("%alice%".into()),
                Value::Text("%alice%".into()),
                Value::Text("%team%".into()),
                Value::Text("%team%".into()),
                Value::Text("%team%".into()),
                Value::Text("%q4 report%".into()),
            ]
        );
    }

    #[test]
    fn neutral_all_and_structured_aliases_compile_without_weakening() {
        let all = compile_search("in:all", 42).unwrap();
        assert_eq!(all.clause, "1 = 1");
        assert!(all.parameters.is_empty());

        assert_eq!(
            compile_search("label:Finance", 42).unwrap().clause,
            "LOWER(e.category) LIKE ? ESCAPE '\\'"
        );
        assert_eq!(
            compile_search("is:unstarred", 42).unwrap().clause,
            "e.starred = 0"
        );
        for query in ["is:sent", "is:me"] {
            assert_eq!(
                compile_search(query, 42).unwrap().clause,
                "e.has_from_me = 1"
            );
        }
        assert_eq!(
            compile_search("has:calendar", 42).unwrap().clause,
            "e.has_invite = 1"
        );
        for query in ["is:snoozed", "in:snoozed"] {
            let compiled = compile_search(query, 42).unwrap();
            assert_eq!(compiled.clause, "s.wake_at > ?");
            assert_eq!(compiled.parameters, vec![Value::Integer(42)]);
        }

        let date = compile_search("date:2026-01-01", 42).unwrap();
        assert_eq!(date.clause, "(e.latest_at >= ? AND e.latest_at < ?)");
        assert_eq!(
            date.parameters,
            vec![
                Value::Integer(timestamp("2026-01-01")),
                Value::Integer(timestamp("2026-01-02")),
            ]
        );
    }

    #[test]
    fn unsupported_semantic_values_are_rejected_explicitly() {
        assert!(compile_search("is:important", 0)
            .unwrap_err()
            .contains("Unsupported is: value"));
        assert!(compile_search("has:image", 0)
            .unwrap_err()
            .contains("Unsupported has: value"));
        assert!(compile_search("in:trash", 0)
            .unwrap_err()
            .contains("Unsupported in: value"));
        assert!(compile_search("before:someday", 0)
            .unwrap_err()
            .contains("Unsupported date value"));
    }

    #[test]
    fn search_limits_are_exact_and_unicode_aware() {
        assert_eq!(tokenize(&"é".repeat(MAX_QUERY_LENGTH)).unwrap().len(), 1);
        assert_eq!(
            tokenize(&"é".repeat(MAX_QUERY_LENGTH + 1)).unwrap_err(),
            "Search query exceeds 2048 characters"
        );

        let max_tokens = std::iter::repeat_n("é", MAX_TOKENS)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(tokenize(&max_tokens).unwrap().len(), MAX_TOKENS);
        let too_many_tokens = std::iter::repeat_n("é", MAX_TOKENS + 1)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            tokenize(&too_many_tokens).unwrap_err(),
            "Search query exceeds 256 tokens"
        );

        let max_nesting = format!("{}é{}", "(".repeat(MAX_NESTING), ")".repeat(MAX_NESTING));
        assert!(compile_search(&max_nesting, 0).is_ok());
        let too_much_nesting = format!(
            "{}é{}",
            "(".repeat(MAX_NESTING + 1),
            ")".repeat(MAX_NESTING + 1)
        );
        assert_eq!(
            compile_search(&too_much_nesting, 0).unwrap_err(),
            "Search nesting exceeds 32 levels"
        );
    }

    #[test]
    fn natural_date_forms_preserve_day_or_calendar_semantics() {
        let noon = timestamp("2026-08-19") + 12 * 60 * 60 * 1_000;
        assert_eq!(parse_date("today", noon).unwrap(), timestamp("2026-08-19"));
        assert_eq!(
            parse_date("yesterday", noon).unwrap(),
            timestamp("2026-08-18")
        );
        for value in ["last week", "last_week"] {
            assert_eq!(
                parse_date(value, noon).unwrap(),
                timestamp("2026-08-12") + 12 * 60 * 60 * 1_000
            );
        }
        for value in ["last month", "last_month"] {
            assert_eq!(
                parse_date(value, noon).unwrap(),
                timestamp("2026-07-19") + 12 * 60 * 60 * 1_000
            );
        }
        for value in ["last year", "last_year"] {
            assert_eq!(
                parse_date(value, noon).unwrap(),
                timestamp("2025-08-19") + 12 * 60 * 60 * 1_000
            );
        }
        assert_eq!(parse_date("2d", noon).unwrap(), noon - 2 * DAY_MS);
        assert_eq!(parse_date("2w", noon).unwrap(), noon - 14 * DAY_MS);
        assert_eq!(
            parse_date("2mo", noon).unwrap(),
            timestamp("2026-06-19") + 12 * 60 * 60 * 1_000
        );
        assert_eq!(
            parse_date("2y", noon).unwrap(),
            timestamp("2024-08-19") + 12 * 60 * 60 * 1_000
        );

        let march_31 = timestamp("2026-03-31") + 12 * 60 * 60 * 1_000;
        assert_eq!(
            parse_date("last month", march_31).unwrap(),
            timestamp("2026-03-03") + 12 * 60 * 60 * 1_000
        );
    }

    #[test]
    fn gregorian_iso_dates_are_strict_and_offsets_are_bounded() {
        assert_eq!(iso_date_to_millis("1970-01-01"), Some(0));
        assert!(iso_date_to_millis("2024-02-29").is_some());
        assert!(iso_date_to_millis("2000-02-29").is_some());
        for invalid in [
            "2026-02-30",
            "2025-02-29",
            "1900-02-29",
            "2026-13-01",
            "2026-04-31",
            "２０２６-01-01",
        ] {
            assert_eq!(iso_date_to_millis(invalid), None, "{invalid}");
            assert!(parse_date(invalid, 0).is_err(), "{invalid}");
        }
        assert!(parse_date("100000d", 0).is_ok());
        assert!(parse_date("100001d", 0).is_err());
        assert!(parse_date("999999999999999999d", 0).is_err());
    }

    #[test]
    fn local_today_and_yesterday_use_the_browser_timezone_offset() {
        let shortly_after_utc_midnight = timestamp("2026-08-19") + 2 * 60 * 60 * 1_000;

        assert_eq!(
            super::parse_date("today", shortly_after_utc_midnight, -4 * 60).unwrap(),
            timestamp("2026-08-18") + 4 * 60 * 60 * 1_000
        );
        assert_eq!(
            super::parse_date("yesterday", shortly_after_utc_midnight, -4 * 60).unwrap(),
            timestamp("2026-08-17") + 4 * 60 * 60 * 1_000
        );
        assert_eq!(
            super::parse_date("today", shortly_after_utc_midnight, 5 * 60 + 30).unwrap(),
            timestamp("2026-08-18") + 18 * 60 * 60 * 1_000 + 30 * 60 * 1_000
        );
        assert_eq!(
            super::parse_date("last-month", shortly_after_utc_midnight, -4 * 60).unwrap(),
            timestamp("2026-07-19") + 2 * 60 * 60 * 1_000
        );
    }

    #[test]
    fn iso_date_day_ranges_use_local_midnight_for_nonzero_offsets() {
        let west = super::compile_search("date:2026-08-19", 0, -4 * 60).unwrap();
        assert_eq!(
            west.parameters,
            vec![
                Value::Integer(timestamp("2026-08-19") + 4 * 60 * 60 * 1_000),
                Value::Integer(timestamp("2026-08-20") + 4 * 60 * 60 * 1_000),
            ]
        );

        let east = super::compile_search("date:2026-08-19", 0, 5 * 60 + 30).unwrap();
        assert_eq!(
            east.parameters,
            vec![
                Value::Integer(timestamp("2026-08-18") + 18 * 60 * 60 * 1_000 + 30 * 60 * 1_000),
                Value::Integer(timestamp("2026-08-19") + 18 * 60 * 60 * 1_000 + 30 * 60 * 1_000),
            ]
        );
    }

    #[test]
    fn browser_timezone_offset_is_bounded_to_real_world_offsets() {
        assert!(super::compile_search("", 0, -14 * 60).is_ok());
        assert!(super::compile_search("", 0, 14 * 60).is_ok());
        assert_eq!(
            super::compile_search("", 0, -14 * 60 - 1).unwrap_err(),
            "Timezone offset must be between -840 and 840 minutes"
        );
        assert_eq!(
            super::compile_search("", 0, 14 * 60 + 1).unwrap_err(),
            "Timezone offset must be between -840 and 840 minutes"
        );
    }
}
