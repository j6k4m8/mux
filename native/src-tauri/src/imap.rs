//! Bounded, read-only IMAP synchronization.
//!
//! Provider wire state and credential material remain in Rust. Each durable
//! worker execution opens an implicit-TLS session, re-reads capabilities after
//! authentication, issues UID-only search/fetch commands, and emits one
//! provider-neutral atomic page. SMTP is intentionally a separate adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rusqlite::{params, Connection, TransactionBehavior};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use rustls_platform_verifier::ConfigVerifierExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::content::{parse_mime, ParsedMailbox, SafeMessageContent};
use crate::provider::{
    ContainerDisplayName, ContainerKind, ContainerMembership, ContainerMembershipChange,
    ContainerRole, MuxAccountId, NormalizedPlainBody, OpaqueSyncCursor, ProviderBatch,
    ProviderBatchId, ProviderBodyState, ProviderCapabilities, ProviderCapability, ProviderCategory,
    ProviderContainer, ProviderEmail, ProviderMessageKeyword, ProviderMessageUpsert,
    ProviderParticipants, ProviderRecipients, ProviderRevision, ProviderSenderName,
    ProviderSnippet, ProviderSubject, ProviderSyncCursor, ProviderThreadMessageCount,
    ProviderThreadUpsert, RemoteContainerId, RemoteContainerIdentity, RemoteMessageId,
    RemoteMessageIdentity, RemoteThreadId, RemoteThreadIdentity, SyncCursorScope, UnixMillis,
};
use crate::worker::{
    enqueue_in_transaction, ClaimedWork, NewWorkItem, ProviderReconciliationPage,
    ProviderSyncContinuation, ProviderSyncPage, ReconciliationObjectKind, RestrictedMessageContent,
    WorkKind, WorkerAdapter, WorkerError, WorkerExecutionContext, WorkerOutcome, WorkerProjection,
};

const WORK_FORMAT_VERSION: u8 = 1;
pub(crate) const IMAP_ACCOUNT_SCOPE: &str = "sync:imap:account:v1";
const MAX_HOST_ADDRESSES: usize = 16;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_FRAMES: usize = 5_000;
const MAX_RESPONSE_BYTES: usize = 34 * 1024 * 1024;
const MAX_LITERAL_BYTES: usize = crate::mime_ingest::MAX_RAW_MESSAGE_BYTES;
const MAX_CAPABILITIES: usize = 256;
const MAX_CAPABILITY_BYTES: usize = 128;
const MAX_FOLDERS: usize = 1_000;
const MAX_FOLDER_BYTES: usize = 1_024;
const UID_SEARCH_WINDOW: u32 = 500;
const UID_FETCH_PAGE: usize = 8;
const MAX_UIDS_PER_SEARCH: usize = UID_SEARCH_WINDOW as usize;
const MAX_WORK_BYTES: usize = 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 16 * 1024;
const MAX_GENERATION_BYTES: usize = 256;

pub(crate) struct ImapAccessGrant {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Zeroizing<String>,
    pub remote_account_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapAccessError {
    CredentialUnavailable,
    ReauthorizationRequired,
    Retryable,
    Permanent,
}

pub(crate) trait ImapAccessSource {
    fn access_for_account(&self, account_id: &str) -> Result<ImapAccessGrant, ImapAccessError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapError {
    Authentication,
    Transient,
    Permanent,
    Protocol,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImapCapabilities {
    values: BTreeSet<String>,
}

impl ImapCapabilities {
    fn new(values: impl IntoIterator<Item = String>) -> Result<Self, ImapError> {
        let mut normalized = BTreeSet::new();
        for value in values {
            if value.is_empty()
                || value.len() > MAX_CAPABILITY_BYTES
                || !value.is_ascii()
                || value
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte == b' ')
            {
                return Err(ImapError::Protocol);
            }
            normalized.insert(value.to_ascii_uppercase());
            if normalized.len() > MAX_CAPABILITIES {
                return Err(ImapError::Protocol);
            }
        }
        if !normalized.contains("IMAP4REV1") && !normalized.contains("IMAP4REV2") {
            return Err(ImapError::Permanent);
        }
        Ok(Self { values: normalized })
    }

    fn contains(&self, value: &str) -> bool {
        self.values.contains(&value.to_ascii_uppercase())
    }

    fn supports_condstore(&self) -> bool {
        self.contains("CONDSTORE") || self.contains("QRESYNC")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImapFolder {
    wire_name: String,
    display_name: String,
    delimiter: Option<char>,
    flags: BTreeSet<String>,
}

impl ImapFolder {
    fn role(&self) -> Option<ContainerRole> {
        for (flag, role) in [
            ("\\INBOX", ContainerRole::Inbox),
            ("\\SENT", ContainerRole::Sent),
            ("\\DRAFTS", ContainerRole::Drafts),
            ("\\TRASH", ContainerRole::Trash),
            ("\\JUNK", ContainerRole::Spam),
            ("\\SPAM", ContainerRole::Spam),
            ("\\ARCHIVE", ContainerRole::Archive),
            ("\\ALL", ContainerRole::AllMail),
            ("\\FLAGGED", ContainerRole::Starred),
        ] {
            if self.flags.contains(flag) {
                return Some(role);
            }
        }
        self.wire_name
            .eq_ignore_ascii_case("INBOX")
            .then_some(ContainerRole::Inbox)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImapSelectedFolder {
    uid_validity: u32,
    uid_next: u32,
    highest_modseq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ImapFetchedMessage {
    uid: u32,
    flags: BTreeSet<String>,
    internal_date: String,
    modseq: Option<u64>,
    raw: Vec<u8>,
}

trait ImapSession {
    fn capabilities(&self) -> &ImapCapabilities;
    fn list_folders(&mut self) -> Result<Vec<ImapFolder>, ImapError>;
    fn examine(
        &mut self,
        folder: &str,
        request_condstore: bool,
    ) -> Result<ImapSelectedFolder, ImapError>;
    fn search_uid_range(
        &mut self,
        start: u32,
        end: u32,
        changed_since: Option<u64>,
    ) -> Result<Vec<u32>, ImapError>;
    fn fetch_messages(
        &mut self,
        uids: &[u32],
        request_modseq: bool,
    ) -> Result<Vec<ImapFetchedMessage>, ImapError>;
}

trait ImapSessionFactory {
    fn open(&self, grant: &ImapAccessGrant) -> Result<Box<dyn ImapSession>, ImapError>;
}

trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

struct ImapWireSession {
    io: BufReader<Box<dyn ReadWrite>>,
    next_tag: u32,
    capabilities: ImapCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WireFrame {
    line: Vec<u8>,
    literal: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WireResponse {
    frames: Vec<WireFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Greeting {
    Ok,
    Preauthenticated,
}

pub(crate) struct NativeImapSessionFactory {
    tls: Arc<ClientConfig>,
}

impl NativeImapSessionFactory {
    pub(crate) fn new() -> Result<Self, WorkerError> {
        let tls = ClientConfig::with_platform_verifier().map_err(|_| {
            WorkerError::Conflict("Could not initialize IMAP TLS verification".into())
        })?;
        Ok(Self { tls: Arc::new(tls) })
    }

    fn connect(&self, grant: &ImapAccessGrant) -> Result<Box<dyn ReadWrite>, ImapError> {
        let addresses = (grant.host.as_str(), grant.port)
            .to_socket_addrs()
            .map_err(|_| ImapError::Transient)?
            .take(MAX_HOST_ADDRESSES + 1)
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.len() > MAX_HOST_ADDRESSES {
            return Err(ImapError::Permanent);
        }
        let mut connected = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(_) => continue,
            }
        }
        let stream = connected.ok_or(ImapError::Transient)?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|_| ImapError::Transient)?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|_| ImapError::Transient)?;
        let server_name = rustls::pki_types::ServerName::try_from(grant.host.clone())
            .map_err(|_| ImapError::Permanent)?;
        let connection = ClientConnection::new(Arc::clone(&self.tls), server_name)
            .map_err(|_| ImapError::Permanent)?;
        Ok(Box::new(StreamOwned::new(connection, stream)))
    }
}

impl ImapSessionFactory for NativeImapSessionFactory {
    fn open(&self, grant: &ImapAccessGrant) -> Result<Box<dyn ImapSession>, ImapError> {
        let io = self.connect(grant)?;
        let mut session = ImapWireSession::new(io)?;
        session.authenticate(&grant.username, &grant.password)?;
        Ok(Box::new(session))
    }
}

impl ImapWireSession {
    fn new(io: Box<dyn ReadWrite>) -> Result<Self, ImapError> {
        let mut session = Self {
            io: BufReader::new(io),
            next_tag: 1,
            capabilities: ImapCapabilities {
                values: BTreeSet::new(),
            },
        };
        match session.read_greeting()? {
            Greeting::Ok => {}
            // A server-side ambient authenticated identity cannot be bound to
            // the typed vault subject, so PREAUTH is rejected for this slice.
            Greeting::Preauthenticated => return Err(ImapError::Permanent),
        }
        Ok(session)
    }

    fn read_greeting(&mut self) -> Result<Greeting, ImapError> {
        let line = read_bounded_line(&mut self.io)?;
        let upper = ascii_upper(&line)?;
        if upper.starts_with("* OK") {
            Ok(Greeting::Ok)
        } else if upper.starts_with("* PREAUTH") {
            Ok(Greeting::Preauthenticated)
        } else if upper.starts_with("* BYE") {
            Err(ImapError::Transient)
        } else {
            Err(ImapError::Protocol)
        }
    }

    fn authenticate(&mut self, username: &str, password: &str) -> Result<(), ImapError> {
        let pre_auth = self.read_capabilities()?;
        let payload = Zeroizing::new(format!("\0{username}\0{password}"));
        let encoded = Zeroizing::new(STANDARD.encode(payload.as_bytes()));
        if pre_auth.contains("AUTH=PLAIN") && pre_auth.contains("SASL-IR") {
            let command = Zeroizing::new(format!("AUTHENTICATE PLAIN {}", encoded.as_str()));
            self.command(&command, 0)?;
        } else if pre_auth.contains("AUTH=PLAIN") {
            self.authenticate_plain_continuation(&encoded)?;
        } else if !pre_auth.contains("LOGINDISABLED") {
            let quoted_user = quote_imap_string(username)?;
            let quoted_password = quote_imap_string(password)?;
            let command = Zeroizing::new(format!("LOGIN {quoted_user} {quoted_password}"));
            self.command(&command, 0)?;
        } else {
            return Err(ImapError::Permanent);
        }
        // RFC capabilities can change at authentication. Never trust the
        // pre-auth set for CONDSTORE or later command selection.
        self.capabilities = self.read_capabilities()?;
        Ok(())
    }

    fn authenticate_plain_continuation(&mut self, encoded: &str) -> Result<(), ImapError> {
        let tag = self.next_tag();
        self.write_line(&format!("{tag} AUTHENTICATE PLAIN"))?;
        let continuation = read_bounded_line(&mut self.io)?;
        if !continuation.starts_with(b"+") {
            return classify_unexpected_auth_line(&continuation);
        }
        self.write_line(encoded)?;
        let response = self.read_response(&tag, 0)?;
        if response.frames.is_empty() {
            return Err(ImapError::Protocol);
        }
        Ok(())
    }

    fn read_capabilities(&mut self) -> Result<ImapCapabilities, ImapError> {
        let response = self.command("CAPABILITY", 0)?;
        let mut values = Vec::new();
        for frame in response.frames {
            let line = ascii_string(&frame.line)?;
            if let Some(rest) = strip_ascii_prefix(&line, "* CAPABILITY ") {
                values.extend(rest.split_ascii_whitespace().map(str::to_owned));
            }
        }
        ImapCapabilities::new(values)
    }

    fn next_tag(&mut self) -> String {
        let tag = format!("M{:06}", self.next_tag);
        self.next_tag = self.next_tag.saturating_add(1);
        tag
    }

    fn command(&mut self, command: &str, max_literal: usize) -> Result<WireResponse, ImapError> {
        if command.is_empty()
            || command.len() > MAX_LINE_BYTES / 2
            || command.contains('\r')
            || command.contains('\n')
            || command.contains('\0')
        {
            return Err(ImapError::Permanent);
        }
        let tag = self.next_tag();
        self.write_line(&format!("{tag} {command}"))?;
        self.read_response(&tag, max_literal)
    }

    fn write_line(&mut self, line: &str) -> Result<(), ImapError> {
        if line.len() > MAX_LINE_BYTES || line.contains('\r') || line.contains('\n') {
            return Err(ImapError::Permanent);
        }
        let io = self.io.get_mut();
        io.write_all(line.as_bytes())
            .and_then(|()| io.write_all(b"\r\n"))
            .and_then(|()| io.flush())
            .map_err(|_| ImapError::Transient)
    }

    fn read_response(&mut self, tag: &str, max_literal: usize) -> Result<WireResponse, ImapError> {
        let mut frames = Vec::new();
        let mut total_bytes = 0usize;
        loop {
            if frames.len() >= MAX_RESPONSE_FRAMES {
                return Err(ImapError::Protocol);
            }
            let line = read_bounded_line(&mut self.io)?;
            total_bytes = total_bytes
                .checked_add(line.len())
                .filter(|value| *value <= MAX_RESPONSE_BYTES)
                .ok_or(ImapError::Protocol)?;
            validate_response_structure(&line)?;
            if starts_with_ascii_case_bytes(&line, b"* BYE") {
                return Err(ImapError::Transient);
            }
            if line.starts_with(tag.as_bytes()) && line.get(tag.len()) == Some(&b' ') {
                return match tagged_status(&line[tag.len() + 1..])? {
                    TaggedStatus::Ok => Ok(WireResponse { frames }),
                    TaggedStatus::No => Err(classify_tagged_failure(&line)),
                    TaggedStatus::Bad => Err(ImapError::Protocol),
                };
            }
            let literal_length = literal_length_at_end(&line)?;
            let literal = if let Some(length) = literal_length {
                if length > max_literal || length > MAX_LITERAL_BYTES {
                    return Err(ImapError::Protocol);
                }
                total_bytes = total_bytes
                    .checked_add(length)
                    .filter(|value| *value <= MAX_RESPONSE_BYTES)
                    .ok_or(ImapError::Protocol)?;
                let mut bytes = vec![0; length];
                self.io
                    .read_exact(&mut bytes)
                    .map_err(|_| ImapError::Transient)?;
                Some(bytes)
            } else {
                None
            };
            frames.push(WireFrame { line, literal });
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaggedStatus {
    Ok,
    No,
    Bad,
}

fn tagged_status(value: &[u8]) -> Result<TaggedStatus, ImapError> {
    if starts_with_ascii_case_bytes(value, b"OK")
        && value.get(2).is_none_or(u8::is_ascii_whitespace)
    {
        Ok(TaggedStatus::Ok)
    } else if starts_with_ascii_case_bytes(value, b"NO")
        && value.get(2).is_none_or(u8::is_ascii_whitespace)
    {
        Ok(TaggedStatus::No)
    } else if starts_with_ascii_case_bytes(value, b"BAD")
        && value.get(3).is_none_or(u8::is_ascii_whitespace)
    {
        Ok(TaggedStatus::Bad)
    } else {
        Err(ImapError::Protocol)
    }
}

fn classify_tagged_failure(line: &[u8]) -> ImapError {
    let upper = String::from_utf8_lossy(line).to_ascii_uppercase();
    if upper.contains("AUTHENTICATIONFAILED")
        || upper.contains("AUTHORIZATIONFAILED")
        || upper.contains("EXPIRED")
    {
        ImapError::Authentication
    } else if upper.contains("UNAVAILABLE") || upper.contains("INUSE") {
        ImapError::Transient
    } else {
        ImapError::Permanent
    }
}

fn classify_unexpected_auth_line(line: &[u8]) -> Result<(), ImapError> {
    if starts_with_ascii_case_bytes(line, b"* BYE") {
        Err(ImapError::Transient)
    } else {
        Err(classify_tagged_failure(line))
    }
}

fn read_bounded_line<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, ImapError> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().map_err(|_| ImapError::Transient)?;
        if buffer.is_empty() {
            return Err(ImapError::Transient);
        }
        let take = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffer.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_LINE_BYTES + 2 {
            return Err(ImapError::Protocol);
        }
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if line.ends_with(b"\n") {
            if !line.ends_with(b"\r\n") {
                return Err(ImapError::Protocol);
            }
            line.truncate(line.len() - 2);
            return Ok(line);
        }
    }
}

fn literal_length_at_end(line: &[u8]) -> Result<Option<usize>, ImapError> {
    if !line.ends_with(b"}") {
        return Ok(None);
    }
    let open = line
        .iter()
        .rposition(|byte| *byte == b'{')
        .ok_or(ImapError::Protocol)?;
    let mut digits = &line[open + 1..line.len() - 1];
    if digits.ends_with(b"+") {
        digits = &digits[..digits.len() - 1];
    }
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(ImapError::Protocol);
    }
    let value = std::str::from_utf8(digits)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(ImapError::Protocol)?;
    Ok(Some(value))
}

fn quote_imap_string(value: &str) -> Result<String, ImapError> {
    if value.len() > MAX_FOLDER_BYTES
        || value
            .chars()
            .any(|character| character == '\0' || character == '\r' || character == '\n')
    {
        return Err(ImapError::Permanent);
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    Ok(quoted)
}

fn ascii_string(value: &[u8]) -> Result<String, ImapError> {
    if !value.is_ascii() {
        return Err(ImapError::Protocol);
    }
    String::from_utf8(value.to_vec()).map_err(|_| ImapError::Protocol)
}

fn ascii_upper(value: &[u8]) -> Result<String, ImapError> {
    Ok(ascii_string(value)?.to_ascii_uppercase())
}

fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

fn starts_with_ascii_case_bytes(value: &[u8], prefix: &[u8]) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn validate_response_structure(line: &[u8]) -> Result<(), ImapError> {
    const MAX_RESPONSE_DEPTH: usize = 32;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in line {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match *byte {
            b'"' => quoted = true,
            b'(' => {
                depth = depth.checked_add(1).ok_or(ImapError::Protocol)?;
                if depth > MAX_RESPONSE_DEPTH {
                    return Err(ImapError::Protocol);
                }
            }
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if quoted || escaped {
        return Err(ImapError::Protocol);
    }
    Ok(())
}

// The high-level wire commands, durable sync phases, projection, and tests are
// kept below so the security framing above stays reusable but not generic IPC.

impl ImapSession for ImapWireSession {
    fn capabilities(&self) -> &ImapCapabilities {
        &self.capabilities
    }

    fn list_folders(&mut self) -> Result<Vec<ImapFolder>, ImapError> {
        let response = self.command(r#"LIST "" "*""#, MAX_FOLDER_BYTES)?;
        let mut folders = Vec::new();
        for frame in response.frames {
            if starts_with_ascii_case_bytes(&frame.line, b"* LIST ") {
                folders.push(parse_list_frame(&frame)?);
                if folders.len() > MAX_FOLDERS {
                    return Err(ImapError::Protocol);
                }
            }
        }
        let mut names = BTreeSet::new();
        for folder in &folders {
            if !names.insert(folder.wire_name.as_str()) {
                return Err(ImapError::Protocol);
            }
        }
        folders.sort_by(|left, right| left.wire_name.cmp(&right.wire_name));
        Ok(folders)
    }

    fn examine(
        &mut self,
        folder: &str,
        request_condstore: bool,
    ) -> Result<ImapSelectedFolder, ImapError> {
        let mailbox = quote_imap_string(folder)?;
        let command = if request_condstore && self.capabilities.supports_condstore() {
            format!("EXAMINE {mailbox} (CONDSTORE)")
        } else {
            format!("EXAMINE {mailbox}")
        };
        let response = self.command(&command, 0)?;
        let mut uid_validity = None;
        let mut uid_next = None;
        let mut highest_modseq = None;
        let mut no_modseq = false;
        for frame in response.frames {
            let upper = String::from_utf8_lossy(&frame.line).to_ascii_uppercase();
            if let Some(value) = response_code_number(&upper, "UIDVALIDITY")? {
                uid_validity = Some(u32::try_from(value).map_err(|_| ImapError::Protocol)?);
            }
            if let Some(value) = response_code_number(&upper, "UIDNEXT")? {
                uid_next = Some(u32::try_from(value).map_err(|_| ImapError::Protocol)?);
            }
            if let Some(value) = response_code_number(&upper, "HIGHESTMODSEQ")? {
                highest_modseq = Some(value);
            }
            no_modseq |= upper.contains("[NOMODSEQ]");
        }
        let uid_validity = uid_validity
            .filter(|value| *value > 0)
            .ok_or(ImapError::Protocol)?;
        let uid_next = uid_next
            .filter(|value| *value > 0)
            .ok_or(ImapError::Protocol)?;
        if no_modseq {
            highest_modseq = None;
        }
        if highest_modseq == Some(0) {
            return Err(ImapError::Protocol);
        }
        Ok(ImapSelectedFolder {
            uid_validity,
            uid_next,
            highest_modseq,
        })
    }

    fn search_uid_range(
        &mut self,
        start: u32,
        end: u32,
        changed_since: Option<u64>,
    ) -> Result<Vec<u32>, ImapError> {
        if start == 0 || end < start || end.saturating_sub(start) >= UID_SEARCH_WINDOW {
            return Err(ImapError::Permanent);
        }
        let command = match changed_since {
            Some(modseq) if modseq > 0 => format!("UID SEARCH UID {start}:{end} MODSEQ {modseq}"),
            Some(_) => return Err(ImapError::Permanent),
            None => format!("UID SEARCH UID {start}:{end}"),
        };
        let response = self.command(&command, 0)?;
        let mut uids = Vec::new();
        for frame in response.frames {
            let line = ascii_string(&frame.line)?;
            let Some(rest) = strip_ascii_prefix(&line, "* SEARCH") else {
                continue;
            };
            for token in rest.split_ascii_whitespace() {
                if token.starts_with('(') {
                    break;
                }
                let uid = token.parse::<u32>().map_err(|_| ImapError::Protocol)?;
                if uid < start || uid > end || uid == 0 {
                    return Err(ImapError::Protocol);
                }
                uids.push(uid);
                if uids.len() > MAX_UIDS_PER_SEARCH {
                    return Err(ImapError::Protocol);
                }
            }
        }
        uids.sort_unstable();
        if uids.windows(2).any(|window| window[0] == window[1]) {
            return Err(ImapError::Protocol);
        }
        Ok(uids)
    }

    fn fetch_messages(
        &mut self,
        uids: &[u32],
        request_modseq: bool,
    ) -> Result<Vec<ImapFetchedMessage>, ImapError> {
        if uids.is_empty()
            || uids.len() > UID_FETCH_PAGE
            || uids.contains(&0)
            || uids.windows(2).any(|window| window[0] >= window[1])
        {
            return Err(ImapError::Permanent);
        }
        let uid_set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let fields = if request_modseq && self.capabilities.supports_condstore() {
            "UID FLAGS INTERNALDATE MODSEQ BODY.PEEK[]"
        } else {
            "UID FLAGS INTERNALDATE BODY.PEEK[]"
        };
        let response = self.command(
            &format!("UID FETCH {uid_set} ({fields})"),
            MAX_LITERAL_BYTES,
        )?;
        let requested = uids.iter().copied().collect::<BTreeSet<_>>();
        let mut messages = Vec::new();
        for frame in response.frames {
            if !starts_with_ascii_case_bytes(&frame.line, b"* ")
                || !String::from_utf8_lossy(&frame.line)
                    .to_ascii_uppercase()
                    .contains(" FETCH ")
            {
                continue;
            }
            let message = parse_fetch_frame(&frame)?;
            if !requested.contains(&message.uid) {
                return Err(ImapError::Protocol);
            }
            messages.push(message);
        }
        messages.sort_by_key(|message| message.uid);
        if messages
            .iter()
            .map(|message| message.uid)
            .ne(uids.iter().copied())
        {
            return Err(ImapError::Protocol);
        }
        Ok(messages)
    }
}

fn parse_list_frame(frame: &WireFrame) -> Result<ImapFolder, ImapError> {
    let line = std::str::from_utf8(&frame.line).map_err(|_| ImapError::Protocol)?;
    let rest = strip_ascii_prefix(line, "* LIST ").ok_or(ImapError::Protocol)?;
    let close = rest.find(')').ok_or(ImapError::Protocol)?;
    if !rest.starts_with('(') {
        return Err(ImapError::Protocol);
    }
    let flags = rest[1..close]
        .split_ascii_whitespace()
        .map(|value| value.to_ascii_uppercase())
        .collect::<BTreeSet<_>>();
    if flags.len() > 128 {
        return Err(ImapError::Protocol);
    }
    let mut rest = rest[close + 1..].trim_start();
    let (delimiter_value, after_delimiter) = parse_nstring(rest)?;
    rest = after_delimiter.trim_start();
    let delimiter = match delimiter_value {
        None => None,
        Some(value) => {
            let value = std::str::from_utf8(&value).map_err(|_| ImapError::Protocol)?;
            let mut chars = value.chars();
            let delimiter = chars.next().ok_or(ImapError::Protocol)?;
            if chars.next().is_some() || delimiter.is_control() {
                return Err(ImapError::Protocol);
            }
            Some(delimiter)
        }
    };
    let mailbox_bytes = if rest.starts_with('{') {
        frame.literal.clone().ok_or(ImapError::Protocol)?
    } else {
        if frame.literal.is_some() {
            return Err(ImapError::Protocol);
        }
        let (value, trailing) = parse_nstring(rest)?;
        if !trailing.trim().is_empty() {
            return Err(ImapError::Protocol);
        }
        value.ok_or(ImapError::Protocol)?
    };
    if mailbox_bytes.is_empty() || mailbox_bytes.len() > MAX_FOLDER_BYTES {
        return Err(ImapError::Protocol);
    }
    let wire_name = String::from_utf8(mailbox_bytes).map_err(|_| ImapError::Protocol)?;
    if wire_name.chars().any(|character| character.is_control()) {
        return Err(ImapError::Protocol);
    }
    let display_name = decode_modified_utf7(&wire_name)?;
    Ok(ImapFolder {
        wire_name,
        display_name,
        delimiter,
        flags,
    })
}

fn parse_nstring(value: &str) -> Result<(Option<Vec<u8>>, &str), ImapError> {
    let value = value.trim_start();
    if value
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("NIL"))
        && value.as_bytes().get(3).is_none_or(u8::is_ascii_whitespace)
    {
        return Ok((None, &value[3..]));
    }
    if let Some(mut rest) = value.strip_prefix('"') {
        let mut bytes = Vec::new();
        let mut escaped = false;
        for (index, character) in rest.char_indices() {
            if escaped {
                bytes.extend_from_slice(character.to_string().as_bytes());
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                rest = &rest[index + 1..];
                return Ok((Some(bytes), rest));
            } else if character.is_control() {
                return Err(ImapError::Protocol);
            } else {
                bytes.extend_from_slice(character.to_string().as_bytes());
            }
            if bytes.len() > MAX_FOLDER_BYTES {
                return Err(ImapError::Protocol);
            }
        }
        return Err(ImapError::Protocol);
    }
    let end = value.find(char::is_whitespace).unwrap_or(value.len());
    let atom = &value[..end];
    if atom.is_empty() || atom.len() > MAX_FOLDER_BYTES || atom.chars().any(char::is_control) {
        return Err(ImapError::Protocol);
    }
    Ok((Some(atom.as_bytes().to_vec()), &value[end..]))
}

fn decode_modified_utf7(value: &str) -> Result<String, ImapError> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        let end = rest.find('-').ok_or(ImapError::Protocol)?;
        let encoded = &rest[..end];
        rest = &rest[end + 1..];
        if encoded.is_empty() {
            output.push('&');
            continue;
        }
        if !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b','))
        {
            return Err(ImapError::Protocol);
        }
        let mut standard = encoded.replace(',', "/");
        while !standard.len().is_multiple_of(4) {
            standard.push('=');
        }
        let bytes = STANDARD.decode(standard).map_err(|_| ImapError::Protocol)?;
        if bytes.len() % 2 != 0 {
            return Err(ImapError::Protocol);
        }
        let units = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        for character in char::decode_utf16(units) {
            output.push(character.map_err(|_| ImapError::Protocol)?);
        }
    }
    output.push_str(rest);
    if output.len() > MAX_FOLDER_BYTES || output.chars().any(char::is_control) {
        return Err(ImapError::Protocol);
    }
    Ok(output)
}

fn response_code_number(line_upper: &str, code: &str) -> Result<Option<u64>, ImapError> {
    let needle = format!("[{code} ");
    let Some(start) = line_upper.find(&needle) else {
        return Ok(None);
    };
    let digits = &line_upper[start + needle.len()..];
    let end = digits.find(']').ok_or(ImapError::Protocol)?;
    if end == 0 || !digits[..end].bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ImapError::Protocol);
    }
    digits[..end]
        .parse::<u64>()
        .map(Some)
        .map_err(|_| ImapError::Protocol)
}

fn parse_fetch_frame(frame: &WireFrame) -> Result<ImapFetchedMessage, ImapError> {
    let line = std::str::from_utf8(&frame.line).map_err(|_| ImapError::Protocol)?;
    let upper = line.to_ascii_uppercase();
    let uid = parse_fetch_number(&upper, "UID ")?
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or(ImapError::Protocol)?;
    let flag_start = upper.find("FLAGS (").ok_or(ImapError::Protocol)? + "FLAGS (".len();
    let flag_end = upper[flag_start..]
        .find(')')
        .map(|offset| flag_start + offset)
        .ok_or(ImapError::Protocol)?;
    let flags = line[flag_start..flag_end]
        .split_ascii_whitespace()
        .map(|value| value.to_ascii_uppercase())
        .collect::<BTreeSet<_>>();
    if flags.len() > 256 || flags.iter().any(|flag| flag.len() > 256) {
        return Err(ImapError::Protocol);
    }
    let internal_marker = "INTERNALDATE \"";
    let date_start =
        upper.find(internal_marker).ok_or(ImapError::Protocol)? + internal_marker.len();
    let date_end = line[date_start..]
        .find('"')
        .map(|offset| date_start + offset)
        .ok_or(ImapError::Protocol)?;
    let internal_date = line[date_start..date_end].to_owned();
    if internal_date.is_empty() || internal_date.len() > 128 {
        return Err(ImapError::Protocol);
    }
    let modseq = parse_parenthesized_fetch_number(&upper, "MODSEQ (")?;
    if modseq == Some(0) {
        return Err(ImapError::Protocol);
    }
    let raw = frame.literal.clone().ok_or(ImapError::Protocol)?;
    if raw.is_empty() || raw.len() > MAX_LITERAL_BYTES {
        return Err(ImapError::Protocol);
    }
    Ok(ImapFetchedMessage {
        uid,
        flags,
        internal_date,
        modseq,
        raw,
    })
}

fn parse_fetch_number(line_upper: &str, marker: &str) -> Result<Option<u64>, ImapError> {
    let Some(start) = line_upper.find(marker) else {
        return Ok(None);
    };
    let digits = &line_upper[start + marker.len()..];
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return Err(ImapError::Protocol);
    }
    digits[..end]
        .parse::<u64>()
        .map(Some)
        .map_err(|_| ImapError::Protocol)
}

fn parse_parenthesized_fetch_number(
    line_upper: &str,
    marker: &str,
) -> Result<Option<u64>, ImapError> {
    let Some(start) = line_upper.find(marker) else {
        return Ok(None);
    };
    let digits = &line_upper[start + marker.len()..];
    let end = digits.find(')').ok_or(ImapError::Protocol)?;
    if end == 0 || !digits[..end].bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ImapError::Protocol);
    }
    digits[..end]
        .parse::<u64>()
        .map(Some)
        .map_err(|_| ImapError::Protocol)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImapSyncWork {
    version: u8,
    batch_id: String,
    expected_prior_cursor: Option<String>,
    phase: ImapSyncPhase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum ImapSyncPhase {
    Discover {
        generation_id: String,
        prior_folders: Vec<StoredFolderCursor>,
    },
    Select {
        generation_id: String,
        account_cursor: String,
        folders: Vec<FolderPlan>,
        folder_index: usize,
    },
    Scan {
        generation_id: String,
        account_cursor: String,
        folders: Vec<FolderPlan>,
        folder_index: usize,
        selected: SelectedPlan,
        stage: ScanStage,
        next_uid: u32,
        pending_uids: Vec<u32>,
    },
    Sweep {
        generation_id: String,
        sweep_index: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredFolderCursor {
    container_id: String,
    cursor: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FolderPlan {
    wire_name: String,
    display_name: String,
    delimiter: Option<char>,
    flags: Vec<String>,
    container_id: String,
    prior_cursor: Option<String>,
}

impl FolderPlan {
    fn selectable(&self) -> bool {
        !self.flags.iter().any(|flag| flag == "\\NOSELECT")
    }

    fn role(&self) -> Option<ContainerRole> {
        ImapFolder {
            wire_name: self.wire_name.clone(),
            display_name: self.display_name.clone(),
            delimiter: self.delimiter,
            flags: self.flags.iter().cloned().collect(),
        }
        .role()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SelectedPlan {
    uid_validity: u32,
    uid_next: u32,
    highest_modseq: Option<u64>,
    baseline_modseq: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ScanStage {
    Full,
    Delta,
    Inventory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableFolderCursor {
    version: u8,
    provider: String,
    container_id: String,
    uid_validity: u32,
    uid_next: u32,
    highest_modseq: Option<u64>,
    complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
}

impl DurableFolderCursor {
    fn decode(value: &str, expected_container_id: &str) -> Result<Self, ImapError> {
        if value.is_empty() || value.len() > MAX_CURSOR_BYTES {
            return Err(ImapError::Permanent);
        }
        let cursor = serde_json::from_str::<Self>(value).map_err(|_| ImapError::Permanent)?;
        if cursor.version != WORK_FORMAT_VERSION
            || cursor.provider != "imap"
            || cursor.container_id != expected_container_id
            || cursor.uid_validity == 0
            || cursor.uid_next == 0
            || cursor.highest_modseq == Some(0)
            || cursor.complete != cursor.checkpoint.is_none()
            || cursor.checkpoint.as_ref().is_some_and(|checkpoint| {
                checkpoint.len() != 64 || !checkpoint.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(ImapError::Permanent);
        }
        Ok(cursor)
    }

    fn encode(&self) -> Result<String, ImapError> {
        let value = serde_json::to_string(self).map_err(|_| ImapError::Permanent)?;
        if value.len() > MAX_CURSOR_BYTES {
            return Err(ImapError::Permanent);
        }
        Ok(value)
    }
}

// Each of these bounds one field of durable work before it is trusted. Work rows
// survive restarts and upgrades, so a row coming back in is treated as untrusted
// input, exactly like a provider response.

fn validate_generation(generation_id: &str) -> Result<(), ImapError> {
    if generation_id.is_empty()
        || generation_id.len() > MAX_GENERATION_BYTES
        || generation_id.chars().any(char::is_control)
    {
        return Err(ImapError::Permanent);
    }
    Ok(())
}

fn validate_cursor_value(cursor: &str) -> Result<(), ImapError> {
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES || cursor.chars().any(char::is_control)
    {
        return Err(ImapError::Permanent);
    }
    Ok(())
}

fn validate_container_id(container_id: &str) -> Result<(), ImapError> {
    if container_id.is_empty()
        || container_id.len() > MAX_FOLDER_BYTES
        || container_id.chars().any(char::is_control)
    {
        return Err(ImapError::Permanent);
    }
    Ok(())
}

/// Prior folder cursors carried through a discover pass. Duplicated containers
/// would let one folder's cursor overwrite another's when the page is applied.
fn validate_stored_folders(folders: &[StoredFolderCursor]) -> Result<(), ImapError> {
    if folders.len() > MAX_FOLDERS {
        return Err(ImapError::Permanent);
    }
    let mut seen = BTreeSet::new();
    for folder in folders {
        validate_container_id(&folder.container_id)?;
        validate_cursor_value(&folder.cursor)?;
        if !DurableFolderCursor::decode(&folder.cursor, &folder.container_id)?.complete {
            return Err(ImapError::Permanent);
        }
        if !seen.insert(folder.container_id.as_str()) {
            return Err(ImapError::Permanent);
        }
    }
    Ok(())
}

/// The folder list a sync walks. The order is the plan, so it has to be stable
/// and free of duplicates for `folder_index` to mean anything across executions.
fn validate_folder_plans(folders: &[FolderPlan]) -> Result<(), ImapError> {
    if folders.is_empty() || folders.len() > MAX_FOLDERS {
        return Err(ImapError::Permanent);
    }
    let mut seen = BTreeSet::new();
    for folder in folders {
        validate_container_id(&folder.container_id)?;
        let mut flags = BTreeSet::new();
        if folder.wire_name.is_empty()
            || folder.wire_name.len() > MAX_FOLDER_BYTES
            || folder.display_name.len() > MAX_FOLDER_BYTES
            || folder.wire_name.chars().any(char::is_control)
            || folder.display_name.chars().any(char::is_control)
            || folder.flags.len() > MAX_CAPABILITIES
            || folder.flags.iter().any(|flag| {
                flag.is_empty()
                    || flag.len() > MAX_CAPABILITY_BYTES
                    || !flag.is_ascii()
                    || flag
                        .bytes()
                        .any(|byte| byte.is_ascii_control() || byte == b' ')
                    || flag != &flag.to_ascii_uppercase()
                    || !flags.insert(flag.as_str())
            })
            || folder
                .delimiter
                .is_some_and(|value| !value.is_ascii() || value.is_control())
        {
            return Err(ImapError::Permanent);
        }
        if let Some(cursor) = folder.prior_cursor.as_deref() {
            validate_cursor_value(cursor)?;
        }
        if !seen.insert(folder.container_id.as_str()) {
            return Err(ImapError::Permanent);
        }
    }
    Ok(())
}

impl ImapSyncWork {
    fn validate(&self) -> Result<(), ImapError> {
        if self.version != WORK_FORMAT_VERSION
            || self.batch_id.is_empty()
            || self.batch_id.len() > 256
            || self
                .expected_prior_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES)
        {
            return Err(ImapError::Permanent);
        }
        let encoded = serde_json::to_vec(self).map_err(|_| ImapError::Permanent)?;
        if encoded.len() > MAX_WORK_BYTES {
            return Err(ImapError::Permanent);
        }
        match &self.phase {
            ImapSyncPhase::Discover {
                generation_id,
                prior_folders,
            } => {
                validate_generation(generation_id)?;
                validate_stored_folders(prior_folders)?;
            }
            ImapSyncPhase::Select {
                generation_id,
                account_cursor,
                folders,
                folder_index,
            } => {
                validate_generation(generation_id)?;
                validate_cursor_value(account_cursor)?;
                validate_folder_plans(folders)?;
                if *folder_index >= folders.len() || !folders[*folder_index].selectable() {
                    return Err(ImapError::Permanent);
                }
            }
            ImapSyncPhase::Scan {
                generation_id,
                account_cursor,
                folders,
                folder_index,
                selected,
                stage,
                next_uid,
                pending_uids,
            } => {
                validate_generation(generation_id)?;
                validate_cursor_value(account_cursor)?;
                validate_folder_plans(folders)?;
                if *folder_index >= folders.len()
                    || !folders[*folder_index].selectable()
                    || selected.uid_validity == 0
                    || selected.uid_next == 0
                    || selected.highest_modseq == Some(0)
                    || selected.baseline_modseq == Some(0)
                    || *next_uid == 0
                    || *next_uid > selected.uid_next
                    || pending_uids.len() > MAX_UIDS_PER_SEARCH
                    || pending_uids.contains(&0)
                    || pending_uids.windows(2).any(|window| window[0] >= window[1])
                    || pending_uids
                        .iter()
                        .any(|uid| *uid >= selected.uid_next || *uid >= *next_uid)
                    || selected
                        .baseline_modseq
                        .zip(selected.highest_modseq)
                        .is_some_and(|(baseline, current)| baseline > current)
                    // Only Delta reads a modseq. The inventory walks the whole
                    // UID range with no CHANGEDSINCE, so demanding a baseline
                    // here made reconciliation unreachable on a server without
                    // CONDSTORE — which is most of them.
                    || (*stage == ScanStage::Delta
                        && (selected.baseline_modseq.is_none()
                            || selected.highest_modseq.is_none()))
                    || (*stage == ScanStage::Full && selected.baseline_modseq.is_some())
                {
                    return Err(ImapError::Permanent);
                }
            }
            ImapSyncPhase::Sweep {
                generation_id,
                sweep_index,
            } => {
                validate_generation(generation_id)?;
                if *sweep_index > 1_000_000 {
                    return Err(ImapError::Permanent);
                }
            }
        }
        Ok(())
    }
}

/// Proves a set of connection details against the server before anything is
/// written down, and reports how many folders it could see. Without this an
/// account looks added and then fails much later inside the worker, where
/// nobody is watching — a typed mistake would read as a broken mailbox.
pub(crate) fn verify_authority(grant: &ImapAccessGrant) -> Result<usize, ImapError> {
    let factory = NativeImapSessionFactory::new().map_err(|_| ImapError::Permanent)?;
    verify_authority_with(&factory, grant)
}

/// Split out so the tests can prove the order — connect, log in, then list —
/// without a server.
fn verify_authority_with<F: ImapSessionFactory>(
    factory: &F,
    grant: &ImapAccessGrant,
) -> Result<usize, ImapError> {
    let mut session = factory.open(grant)?;
    // LIST proves more than a login does: it is the first thing a sync needs,
    // and a mailbox that cannot be listed cannot be synchronized.
    Ok(session.list_folders()?.len())
}

pub(crate) fn is_imap_sync_work(work: &ClaimedWork) -> bool {
    work.kind == WorkKind::Sync && work.scope == IMAP_ACCOUNT_SCOPE
}

pub(crate) fn schedule_initial_syncs(
    database_path: &Path,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    schedule_syncs(database_path, now_ms, true, false, None)
}

pub(crate) fn schedule_resumable_syncs(
    database_path: &Path,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    schedule_syncs(database_path, now_ms, false, false, None)
}

pub(crate) fn sync_account_now(
    database_path: &Path,
    account_id: &str,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    schedule_syncs(database_path, now_ms, false, false, Some(account_id))
}

pub(crate) fn schedule_due_syncs(database_path: &Path, now_ms: i64) -> Result<usize, WorkerError> {
    schedule_syncs(database_path, now_ms, false, true, None)
}

fn schedule_syncs(
    database_path: &Path,
    now_ms: i64,
    initial_only: bool,
    only_due: bool,
    only_account: Option<&str>,
) -> Result<usize, WorkerError> {
    if now_ms < 0 {
        return Err(WorkerError::Validation(
            "IMAP sync timestamp is invalid".into(),
        ));
    }
    let mut connection = Connection::open(database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Launch schedules the first sync of every mailbox that has never had one;
    // the timer and the resume path leave those alone. "Sync now" names one
    // mailbox and means it whatever its history — a mailbox that has never
    // synced is exactly the one someone presses it for, and the one a fresh
    // provisioning hands here.
    let state = if initial_only {
        "account.sync_state = 'never_synced'"
    } else if only_account.is_some() {
        "account.sync_state IN ('never_synced', 'idle', 'scheduled')"
    } else {
        "account.sync_state IN ('idle', 'scheduled')"
    };
    let query = format!(
        "SELECT account.account_id,
                (SELECT cursor FROM provider_sync_cursors cursor
                 WHERE cursor.account_id = account.account_id AND cursor.scope = 'a:v1')
         FROM provider_accounts account
         WHERE account.provider_kind = 'imap'
           AND account.auth_state = 'ready'
           AND account.credential_ref IS NOT NULL
           AND {state}
           AND (?3 = 0 OR account.last_sync_at IS NULL
             OR ?2 - account.last_sync_at >= account.refresh_seconds * 1000)
           AND (?4 IS NULL OR account.account_id = ?4)
           AND NOT EXISTS (
             SELECT 1 FROM provider_work_items work
             WHERE work.account_id = account.account_id
               AND work.kind = 'sync' AND work.scope = ?1
               AND work.state IN (
                 'queued', 'executing', 'retry_wait', 'rate_limited',
                 'authentication_blocked'
               )
           )
         ORDER BY account.account_id"
    );
    let accounts = {
        let mut statement = transaction.prepare(&query)?;
        let values = statement
            .query_map(
                params![
                    IMAP_ACCOUNT_SCOPE,
                    now_ms,
                    i64::from(only_due),
                    only_account
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    for (index, (account_id, account_cursor)) in accounts.iter().enumerate() {
        let prior_folders = read_stored_folder_cursors(&transaction, account_id)?;
        let terminal_count = transaction.query_row(
            "SELECT COUNT(*) FROM provider_work_items
             WHERE account_id = ?1 AND kind = 'sync' AND scope = ?2",
            params![account_id, IMAP_ACCOUNT_SCOPE],
            |row| row.get::<_, i64>(0),
        )?;
        let generation_id = format!("imap-scan-{now_ms}-{index}-{terminal_count}");
        let request = make_work(
            ImapSyncPhase::Discover {
                generation_id,
                prior_folders,
            },
            account_cursor.clone(),
        )
        .map_err(|_| WorkerError::Validation("IMAP work could not be constructed".into()))?;
        enqueue_in_transaction(
            &transaction,
            work_item(account_id, request, now_ms)?,
            now_ms,
        )?;
        transaction.execute(
            "UPDATE provider_accounts SET sync_state = 'scheduled', updated_at = ?2
             WHERE account_id = ?1",
            params![account_id, now_ms],
        )?;
    }
    transaction.commit()?;
    Ok(accounts.len())
}

fn read_stored_folder_cursors(
    transaction: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> Result<Vec<StoredFolderCursor>, WorkerError> {
    let mut statement = transaction.prepare(
        "SELECT substr(scope, 6), cursor
         FROM provider_sync_cursors
         WHERE account_id = ?1 AND scope LIKE 'c:v1:%'
         ORDER BY scope
         LIMIT ?2",
    )?;
    let values = statement
        .query_map(params![account_id, (MAX_FOLDERS + 1) as i64], |row| {
            Ok(StoredFolderCursor {
                container_id: row.get(0)?,
                cursor: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() > MAX_FOLDERS {
        return Err(WorkerError::Validation(
            "IMAP folder cursor inventory exceeds its bound".into(),
        ));
    }
    validate_stored_folders(&values)
        .map_err(|_| WorkerError::Validation("IMAP folder cursor is invalid".into()))?;
    Ok(values)
}

pub(crate) struct ImapAdapter<A, F, C> {
    access: A,
    sessions: F,
    clock: C,
}

impl<A, F, C> ImapAdapter<A, F, C> {
    pub(crate) fn new(access: A, sessions: F, clock: C) -> Self {
        Self {
            access,
            sessions,
            clock,
        }
    }
}

impl<A, F, C> WorkerAdapter for ImapAdapter<A, F, C>
where
    A: ImapAccessSource + Sync,
    F: ImapSessionFactory + Sync,
    C: Fn() -> i64 + Sync,
{
    fn execute(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome {
        if !is_imap_sync_work(work) {
            return WorkerOutcome::PermanentFailure {
                code: "imap_invalid_work_kind".into(),
            };
        }
        let request = match serde_json::from_str::<ImapSyncWork>(&work.payload_json) {
            Ok(request) if request.validate().is_ok() && request.batch_id == work.ordering_key => {
                request
            }
            _ => {
                return WorkerOutcome::PermanentFailure {
                    code: "imap_invalid_work_payload".into(),
                }
            }
        };
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let grant = match self.access.access_for_account(&work.account_id) {
            Ok(grant) => grant,
            Err(error) => return access_error_outcome(error),
        };
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let mut session = match self.sessions.open(&grant) {
            Ok(session) => session,
            Err(error) => return imap_error_outcome(error),
        };
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let now_ms = (self.clock)();
        match execute_sync_page(
            session.as_mut(),
            &work.account_id,
            &grant.remote_account_id,
            request,
            now_ms,
        ) {
            Ok(page) => WorkerOutcome::Succeeded {
                projection: WorkerProjection::ProviderSyncPage(Box::new(page)),
            },
            Err(error) => imap_error_outcome(error),
        }
    }
}

fn execute_sync_page(
    session: &mut dyn ImapSession,
    account_id: &str,
    remote_account_id: &str,
    request: ImapSyncWork,
    now_ms: i64,
) -> Result<ProviderSyncPage, ImapError> {
    if now_ms < 0 {
        return Err(ImapError::Permanent);
    }
    match request.phase.clone() {
        ImapSyncPhase::Discover {
            generation_id,
            prior_folders,
        } => execute_discover(
            session,
            account_id,
            request,
            generation_id,
            prior_folders,
            now_ms,
        ),
        ImapSyncPhase::Select {
            generation_id,
            account_cursor,
            folders,
            folder_index,
        } => execute_select(
            session,
            account_id,
            request,
            generation_id,
            account_cursor,
            folders,
            folder_index,
            now_ms,
        ),
        ImapSyncPhase::Scan {
            generation_id,
            account_cursor,
            folders,
            folder_index,
            selected,
            stage,
            next_uid,
            pending_uids,
        } => execute_scan(
            session,
            account_id,
            remote_account_id,
            request,
            generation_id,
            account_cursor,
            folders,
            folder_index,
            selected,
            stage,
            next_uid,
            pending_uids,
            now_ms,
        ),
        ImapSyncPhase::Sweep {
            generation_id,
            sweep_index,
        } => execute_sweep(account_id, request, generation_id, sweep_index, now_ms),
    }
}

fn make_work(
    phase: ImapSyncPhase,
    expected_prior_cursor: Option<String>,
) -> Result<ImapSyncWork, ImapError> {
    let identity =
        serde_json::to_vec(&(&phase, &expected_prior_cursor)).map_err(|_| ImapError::Permanent)?;
    let batch_id = format!("imap-{}", &sha256_hex(&identity)[..40]);
    let request = ImapSyncWork {
        version: WORK_FORMAT_VERSION,
        batch_id,
        expected_prior_cursor,
        phase,
    };
    request.validate()?;
    Ok(request)
}

fn work_item(
    account_id: &str,
    request: ImapSyncWork,
    now_ms: i64,
) -> Result<NewWorkItem, WorkerError> {
    if account_id.is_empty()
        || account_id.len() > 256
        || account_id.chars().any(char::is_control)
        || now_ms < 0
    {
        return Err(WorkerError::Validation(
            "IMAP work identity is invalid".into(),
        ));
    }
    request
        .validate()
        .map_err(|_| WorkerError::Validation("IMAP work payload is invalid".into()))?;
    let payload_json = serde_json::to_string(&request)
        .map_err(|_| WorkerError::Validation("IMAP work serialization failed".into()))?;
    Ok(NewWorkItem {
        id: format!("work-{}", request.batch_id),
        account_id: account_id.into(),
        operation_id: None,
        kind: WorkKind::Sync,
        scope: IMAP_ACCOUNT_SCOPE.into(),
        ordering_key: request.batch_id,
        payload_json,
        priority: 0,
        available_at: now_ms,
        max_attempts: 8,
    })
}

fn continuation_for(
    request: ImapSyncWork,
    now_ms: i64,
) -> Result<ProviderSyncContinuation, ImapError> {
    request.validate()?;
    let payload_json = serde_json::to_string(&request).map_err(|_| ImapError::Permanent)?;
    if payload_json.len() > MAX_WORK_BYTES {
        return Err(ImapError::Permanent);
    }
    Ok(ProviderSyncContinuation {
        id: format!("work-{}", request.batch_id),
        ordering_key: request.batch_id,
        payload_json,
        priority: 0,
        available_at: now_ms,
        max_attempts: 8,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImapAccountCursor<'a> {
    version: u8,
    provider: &'static str,
    generation_id: &'a str,
    sweep_index: u32,
}

fn account_cursor(generation_id: &str, sweep_index: u32) -> Result<String, ImapError> {
    validate_generation(generation_id)?;
    let value = serde_json::to_string(&ImapAccountCursor {
        version: WORK_FORMAT_VERSION,
        provider: "imap",
        generation_id,
        sweep_index,
    })
    .map_err(|_| ImapError::Permanent)?;
    validate_cursor_value(&value)?;
    Ok(value)
}

fn folder_cursor(
    folder: &FolderPlan,
    selected: &SelectedPlan,
    next_phase: Option<&ImapSyncPhase>,
) -> Result<String, ImapError> {
    let checkpoint = next_phase
        .map(|phase| serde_json::to_vec(phase).map(|value| sha256_hex(&value)))
        .transpose()
        .map_err(|_| ImapError::Permanent)?;
    DurableFolderCursor {
        version: WORK_FORMAT_VERSION,
        provider: "imap".into(),
        container_id: folder.container_id.clone(),
        uid_validity: selected.uid_validity,
        uid_next: selected.uid_next,
        highest_modseq: selected.highest_modseq,
        complete: checkpoint.is_none(),
        checkpoint,
    }
    .encode()
}

#[allow(clippy::too_many_arguments)]
fn build_sync_page(
    account_id: &str,
    request: ImapSyncWork,
    cursor_scope: SyncCursorScope,
    cursor_value: String,
    now_ms: i64,
    thread_upserts: Vec<ProviderThreadUpsert>,
    message_upserts: Vec<ProviderMessageUpsert>,
    container_upserts: Vec<ProviderContainer>,
    membership_changes: Vec<ContainerMembershipChange>,
    restricted_message_content: Vec<RestrictedMessageContent>,
    continuation: Option<ImapSyncWork>,
    complete: bool,
    capabilities: Option<ProviderCapabilities>,
    reconciliation: Option<ProviderReconciliationPage>,
) -> Result<ProviderSyncPage, ImapError> {
    let mux_account_id = MuxAccountId::new(account_id.to_owned()).map_err(contract_error)?;
    let batch = ProviderBatch {
        mux_account_id: mux_account_id.clone(),
        batch_id: ProviderBatchId::new(request.batch_id).map_err(contract_error)?,
        expected_prior_cursor: request
            .expected_prior_cursor
            .map(OpaqueSyncCursor::new)
            .transpose()
            .map_err(contract_error)?,
        cursor: ProviderSyncCursor {
            mux_account_id,
            scope: cursor_scope,
            value: OpaqueSyncCursor::new(cursor_value).map_err(contract_error)?,
        },
        observed_at: UnixMillis::new(now_ms).map_err(contract_error)?,
        thread_upserts,
        message_upserts,
        container_upserts,
        membership_changes,
        tombstones: Vec::new(),
    };
    batch.validate().map_err(contract_error)?;
    Ok(ProviderSyncPage {
        batch: Box::new(batch),
        restricted_message_content,
        continuation: continuation
            .map(|request| continuation_for(request, now_ms))
            .transpose()?,
        complete,
        capabilities,
        replace_memberships_for_upserted_messages: true,
        derive_thread_state_from_messages: true,
        reconciliation,
    })
}

fn contract_error<T: std::fmt::Debug>(_error: T) -> ImapError {
    #[cfg(test)]
    eprintln!("IMAP provider contract rejected a projected value: {_error:?}");
    ImapError::Permanent
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn normalize_single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn access_error_outcome(error: ImapAccessError) -> WorkerOutcome {
    match error {
        ImapAccessError::CredentialUnavailable => WorkerOutcome::CredentialUnavailable,
        ImapAccessError::ReauthorizationRequired => WorkerOutcome::AuthenticationExpired {
            code: "imap_reauthorization_required".into(),
        },
        ImapAccessError::Retryable => WorkerOutcome::RetryableFailure {
            code: "imap_access_retryable".into(),
        },
        ImapAccessError::Permanent => WorkerOutcome::PermanentFailure {
            code: "imap_access_invalid".into(),
        },
    }
}

fn imap_error_outcome(error: ImapError) -> WorkerOutcome {
    match error {
        ImapError::Authentication => WorkerOutcome::AuthenticationExpired {
            code: "imap_authentication_expired".into(),
        },
        ImapError::Transient => WorkerOutcome::RetryableFailure {
            code: "imap_transient".into(),
        },
        ImapError::Permanent => WorkerOutcome::PermanentFailure {
            code: "imap_permanent".into(),
        },
        ImapError::Protocol => WorkerOutcome::PermanentFailure {
            code: "imap_protocol_invalid".into(),
        },
    }
}

fn execute_discover(
    session: &mut dyn ImapSession,
    account_id: &str,
    request: ImapSyncWork,
    generation_id: String,
    prior_folders: Vec<StoredFolderCursor>,
    now_ms: i64,
) -> Result<ProviderSyncPage, ImapError> {
    let mux_account_id = MuxAccountId::new(account_id.to_owned()).map_err(contract_error)?;
    let mut prior_by_container = prior_folders
        .into_iter()
        .map(|folder| (folder.container_id, folder.cursor))
        .collect::<BTreeMap<_, _>>();
    let listed = session.list_folders()?;
    let mut plans = Vec::with_capacity(listed.len());
    let mut containers = Vec::with_capacity(listed.len());
    let mut container_ids = BTreeSet::new();
    for folder in listed {
        let container_id = format!(
            "imap-folder-{}",
            &sha256_hex(folder.wire_name.as_bytes())[..40]
        );
        if !container_ids.insert(container_id.clone()) {
            return Err(ImapError::Protocol);
        }
        let plan = FolderPlan {
            wire_name: folder.wire_name,
            display_name: folder.display_name,
            delimiter: folder.delimiter,
            flags: folder.flags.into_iter().collect(),
            prior_cursor: prior_by_container.remove(&container_id),
            container_id: container_id.clone(),
        };
        containers.push(ProviderContainer {
            identity: RemoteContainerIdentity {
                mux_account_id: mux_account_id.clone(),
                remote_container_id: RemoteContainerId::new(container_id)
                    .map_err(contract_error)?,
            },
            display_name: ContainerDisplayName::new(truncate_utf8(&plan.display_name, 512))
                .map_err(contract_error)?,
            kind: ContainerKind::Folder,
            role: plan.role(),
            parent_remote_container_id: None,
            selectable: plan.selectable(),
        });
        plans.push(plan);
    }
    if plans.len() > MAX_FOLDERS {
        return Err(ImapError::Protocol);
    }
    if !plans.is_empty() {
        validate_folder_plans(&plans)?;
    }
    let account_cursor = account_cursor(&generation_id, 0)?;
    let first_selectable = plans.iter().position(FolderPlan::selectable);
    let (next, expected_prior_cursor) = match first_selectable {
        Some(folder_index) => {
            let prior = plans[folder_index].prior_cursor.clone();
            (
                ImapSyncPhase::Select {
                    generation_id: generation_id.clone(),
                    account_cursor: account_cursor.clone(),
                    folders: plans,
                    folder_index,
                },
                prior,
            )
        }
        None => (
            ImapSyncPhase::Sweep {
                generation_id: generation_id.clone(),
                sweep_index: 0,
            },
            Some(account_cursor.clone()),
        ),
    };
    let next = make_work(next, expected_prior_cursor)?;
    let mut capabilities = BTreeSet::new();
    if session.capabilities().supports_condstore() {
        capabilities.insert(ProviderCapability::DeltaSync);
    }
    build_sync_page(
        account_id,
        request,
        SyncCursorScope::Account,
        account_cursor,
        now_ms,
        Vec::new(),
        Vec::new(),
        containers,
        Vec::new(),
        Vec::new(),
        Some(next),
        false,
        Some(ProviderCapabilities::new(capabilities)),
        Some(ProviderReconciliationPage {
            generation_id,
            begin: true,
            reset_seen_containers: false,
            complete_kinds: BTreeSet::new(),
            sweep_kinds: BTreeSet::new(),
            seen_remote_messages: Vec::new(),
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_select(
    session: &mut dyn ImapSession,
    account_id: &str,
    request: ImapSyncWork,
    generation_id: String,
    account_cursor: String,
    folders: Vec<FolderPlan>,
    folder_index: usize,
    now_ms: i64,
) -> Result<ProviderSyncPage, ImapError> {
    validate_folder_plans(&folders)?;
    let folder = folders
        .get(folder_index)
        .cloned()
        .ok_or(ImapError::Permanent)?;
    if !folder.selectable() || request.expected_prior_cursor != folder.prior_cursor {
        return Err(ImapError::Permanent);
    }
    let current = session.examine(
        &folder.wire_name,
        session.capabilities().supports_condstore(),
    )?;
    let prior = folder
        .prior_cursor
        .as_deref()
        .map(|cursor| DurableFolderCursor::decode(cursor, &folder.container_id))
        .transpose()?;
    let baseline_modseq = prior.as_ref().and_then(|prior| {
        (prior.complete
            && prior.uid_validity == current.uid_validity
            && session.capabilities().supports_condstore()
            && prior.highest_modseq.is_some()
            && current.highest_modseq.is_some()
            && prior.highest_modseq <= current.highest_modseq)
            .then_some(prior.highest_modseq)
            .flatten()
    });
    let stage = if baseline_modseq.is_some() {
        ScanStage::Delta
    } else {
        ScanStage::Full
    };
    let selected = SelectedPlan {
        uid_validity: current.uid_validity,
        uid_next: current.uid_next,
        highest_modseq: current.highest_modseq,
        baseline_modseq,
    };
    let next_phase = ImapSyncPhase::Scan {
        generation_id: generation_id.clone(),
        account_cursor,
        folders,
        folder_index,
        selected: selected.clone(),
        stage,
        next_uid: 1,
        pending_uids: Vec::new(),
    };
    let cursor_value = folder_cursor(&folder, &selected, Some(&next_phase))?;
    let next = make_work(next_phase, Some(cursor_value.clone()))?;
    build_sync_page(
        account_id,
        request,
        SyncCursorScope::Container {
            remote_container_id: RemoteContainerId::new(folder.container_id.clone())
                .map_err(contract_error)?,
        },
        cursor_value,
        now_ms,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Some(next),
        false,
        None,
        Some(ProviderReconciliationPage {
            generation_id,
            begin: false,
            reset_seen_containers: false,
            complete_kinds: BTreeSet::new(),
            sweep_kinds: BTreeSet::new(),
            seen_remote_messages: Vec::new(),
        }),
    )
}

fn remote_message_identity(
    account_id: &MuxAccountId,
    folder: &FolderPlan,
    uid_validity: u32,
    uid: u32,
    remote_thread_id: Option<RemoteThreadId>,
) -> Result<RemoteMessageIdentity, ImapError> {
    Ok(RemoteMessageIdentity {
        mux_account_id: account_id.clone(),
        remote_message_id: RemoteMessageId::new(format!(
            "imap:{}:{uid_validity}:{uid}",
            folder.container_id
        ))
        .map_err(contract_error)?,
        remote_thread_id,
    })
}

fn format_mailboxes(values: &[ParsedMailbox]) -> String {
    values
        .iter()
        .map(|value| {
            if value.name.is_empty() {
                value.address.clone()
            } else {
                format!("{} <{}>", value.name, value.address)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn internal_date_ms(value: &str) -> Result<i64, ImapError> {
    if value.is_empty() || value.len() > 128 || !value.is_ascii() {
        return Err(ImapError::Protocol);
    }
    let mut fields = value.split_ascii_whitespace();
    let date_field = fields.next().ok_or(ImapError::Protocol)?;
    let time_field = fields.next().ok_or(ImapError::Protocol)?;
    let zone_field = fields.next().ok_or(ImapError::Protocol)?;
    if fields.next().is_some() {
        return Err(ImapError::Protocol);
    }
    let mut date_parts = date_field.split('-');
    let day = parse_decimal::<u8>(date_parts.next(), 1, 2)?;
    let month = match date_parts.next() {
        Some("Jan") => 1,
        Some("Feb") => 2,
        Some("Mar") => 3,
        Some("Apr") => 4,
        Some("May") => 5,
        Some("Jun") => 6,
        Some("Jul") => 7,
        Some("Aug") => 8,
        Some("Sep") => 9,
        Some("Oct") => 10,
        Some("Nov") => 11,
        Some("Dec") => 12,
        _ => return Err(ImapError::Protocol),
    };
    let year = parse_decimal::<u16>(date_parts.next(), 4, 4)?;
    if date_parts.next().is_some() || !(1900..=3000).contains(&year) {
        return Err(ImapError::Protocol);
    }
    let mut time_parts = time_field.split(':');
    let hour = parse_decimal::<u8>(time_parts.next(), 2, 2)?;
    let minute = parse_decimal::<u8>(time_parts.next(), 2, 2)?;
    let second = parse_decimal::<u8>(time_parts.next(), 2, 2)?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return Err(ImapError::Protocol);
    }
    let zone = zone_field.as_bytes();
    if zone.len() != 5 || !matches!(zone[0], b'+' | b'-') {
        return Err(ImapError::Protocol);
    }
    let tz_hour = parse_decimal::<u8>(zone_field.get(1..3), 2, 2)?;
    let tz_minute = parse_decimal::<u8>(zone_field.get(3..5), 2, 2)?;
    if tz_hour > 23 || tz_minute > 59 {
        return Err(ImapError::Protocol);
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > max_day {
        return Err(ImapError::Protocol);
    }
    let date = mail_parser::DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        tz_before_gmt: zone[0] == b'-',
        tz_hour,
        tz_minute,
    };
    Ok(date.to_timestamp().saturating_mul(1_000).max(0))
}

fn parse_decimal<T>(value: Option<&str>, min: usize, max: usize) -> Result<T, ImapError>
where
    T: std::str::FromStr,
{
    let value = value.ok_or(ImapError::Protocol)?;
    if !(min..=max).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ImapError::Protocol);
    }
    value.parse().map_err(|_| ImapError::Protocol)
}

struct ProjectedImapMessage {
    thread: ProviderThreadUpsert,
    message: ProviderMessageUpsert,
    membership: ContainerMembershipChange,
    restricted_content: Option<RestrictedMessageContent>,
}

fn project_imap_message(
    account_id: &MuxAccountId,
    remote_account_id: &str,
    folder: &FolderPlan,
    selected: &SelectedPlan,
    fetched: ImapFetchedMessage,
) -> Result<ProjectedImapMessage, ImapError> {
    let sent_at_ms = internal_date_ms(&fetched.internal_date)?;
    let parsed = parse_mime(&fetched.raw).ok();
    let parsed_body = parsed.is_some();
    let (
        subject,
        sender_name,
        sender_email,
        recipients,
        cc_recipients,
        body_text,
        body_html,
        blocked_remote_resources,
        remote_images,
        internet_message_id,
        in_reply_to,
        references,
        has_attachments,
        has_invite,
        has_links,
    ) = match parsed {
        Some(SafeMessageContent {
            subject,
            from,
            to,
            cc,
            internet_message_id,
            in_reply_to,
            references,
            body_text,
            body_html,
            blocked_remote_resources,
            remote_images,
            attachments,
            ..
        }) => {
            let sender = from.first();
            let sender_name = sender.map(|value| value.name.clone()).unwrap_or_default();
            let sender_email = sender
                .map(|value| value.address.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown@example.invalid".into());
            let lower = body_text.to_ascii_lowercase();
            let has_attachments = !attachments.is_empty();
            let has_invite = attachments
                .iter()
                .any(|attachment| attachment.media_type.eq_ignore_ascii_case("text/calendar"));
            let has_links = lower.contains("http://") || lower.contains("https://");
            (
                subject,
                sender_name,
                sender_email,
                format_mailboxes(&to),
                format_mailboxes(&cc),
                body_text,
                body_html,
                blocked_remote_resources,
                remote_images,
                internet_message_id,
                in_reply_to,
                references,
                has_attachments,
                has_invite,
                has_links,
            )
        }
        None => (
            String::new(),
            String::new(),
            "unknown@example.invalid".into(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            0,
            Vec::new(),
            None,
            None,
            Vec::new(),
            false,
            false,
            false,
        ),
    };
    let subject = truncate_utf8(&normalize_single_line(&subject), 2_000);
    let sender_name = truncate_utf8(&normalize_single_line(&sender_name), 2_000);
    let sender_email = truncate_utf8(&normalize_single_line(&sender_email), 2_048);
    let recipients = truncate_utf8(&normalize_single_line(&recipients), 16_000);
    let cc_recipients = truncate_utf8(&normalize_single_line(&cc_recipients), 16_000);
    let normalized_body = body_text.replace("\r\n", "\n").replace('\r', "\n");
    let body_was_truncated = normalized_body.len() > 2 * 1024 * 1024;
    let body_text = truncate_utf8(&normalized_body, 2 * 1024 * 1024);
    let fallback_remote_id = format!(
        "imap:{}:{}:{}",
        folder.container_id, selected.uid_validity, fetched.uid
    );
    let thread_seed = references
        .first()
        .or(in_reply_to.as_ref())
        .or(internet_message_id.as_ref())
        .map(String::as_str)
        .unwrap_or(&fallback_remote_id);
    let remote_thread_id = RemoteThreadId::new(format!(
        "imap-thread-{}",
        &sha256_hex(thread_seed.as_bytes())[..40]
    ))
    .map_err(contract_error)?;
    let identity = remote_message_identity(
        account_id,
        folder,
        selected.uid_validity,
        fetched.uid,
        Some(remote_thread_id.clone()),
    )?;
    let restricted_content = parsed_body.then(|| RestrictedMessageContent {
        identity: identity.clone(),
        body_html,
        blocked_remote_resources,
        remote_images,
    });
    let mut keywords = BTreeSet::new();
    if !fetched.flags.contains("\\SEEN") {
        keywords.insert(ProviderMessageKeyword::new("unread").map_err(contract_error)?);
    }
    if fetched.flags.contains("\\FLAGGED") {
        keywords.insert(ProviderMessageKeyword::new("starred").map_err(contract_error)?);
    }
    if fetched.flags.contains("$IMPORTANT") || fetched.flags.contains("\\IMPORTANT") {
        keywords.insert(ProviderMessageKeyword::new("important").map_err(contract_error)?);
    }
    for (present, keyword) in [
        (has_attachments, "has_attachment"),
        (has_invite, "has_invite"),
        (has_links, "has_link"),
    ] {
        if present {
            keywords.insert(ProviderMessageKeyword::new(keyword).map_err(contract_error)?);
        }
    }
    let is_from_me = matches!(
        folder.role(),
        Some(ContainerRole::Sent | ContainerRole::Drafts)
    ) || sender_email.eq_ignore_ascii_case(remote_account_id);
    let mut revision_material = fetched.raw;
    for flag in &fetched.flags {
        revision_material.extend_from_slice(flag.as_bytes());
        revision_material.push(0);
    }
    if let Some(modseq) = fetched.modseq {
        revision_material.extend_from_slice(&modseq.to_be_bytes());
    }
    let revision =
        ProviderRevision::new(format!("imap-revision-{}", sha256_hex(&revision_material)))
            .map_err(contract_error)?;
    let body_state = if !parsed_body {
        ProviderBodyState::Unavailable
    } else if body_was_truncated {
        ProviderBodyState::Truncated
    } else {
        ProviderBodyState::Complete
    };
    let message = ProviderMessageUpsert {
        identity: identity.clone(),
        subject: ProviderSubject::new(subject.clone()).map_err(contract_error)?,
        sender_name: ProviderSenderName::new(sender_name.clone()).map_err(contract_error)?,
        sender_email: ProviderEmail::new(sender_email.clone()).map_err(contract_error)?,
        recipients: ProviderRecipients::new(recipients).map_err(contract_error)?,
        cc_recipients: ProviderRecipients::new(cc_recipients).map_err(contract_error)?,
        bcc_recipients: ProviderRecipients::new(String::new()).map_err(contract_error)?,
        sent_at: UnixMillis::new(sent_at_ms).map_err(contract_error)?,
        body_text: NormalizedPlainBody::new(body_text.clone()).map_err(contract_error)?,
        body_state,
        is_from_me,
        revision: Some(revision),
        keywords,
        internet_message_id,
        in_reply_to,
        references: (!references.is_empty()).then_some(references),
    };
    let participants = truncate_utf8(
        &if sender_name.is_empty() {
            sender_email.clone()
        } else {
            format!("{sender_name} <{sender_email}>")
        },
        16_000,
    );
    let thread = ProviderThreadUpsert {
        identity: RemoteThreadIdentity {
            mux_account_id: account_id.clone(),
            remote_thread_id,
        },
        subject: ProviderSubject::new(subject).map_err(contract_error)?,
        participants: ProviderParticipants::new(participants).map_err(contract_error)?,
        snippet: ProviderSnippet::new(truncate_utf8(&normalize_single_line(&body_text), 2_000))
            .map_err(contract_error)?,
        latest_at: UnixMillis::new(sent_at_ms).map_err(contract_error)?,
        message_count: ProviderThreadMessageCount::new(1).map_err(contract_error)?,
        in_inbox: folder.role() == Some(ContainerRole::Inbox),
        unread: !fetched.flags.contains("\\SEEN"),
        starred: fetched.flags.contains("\\FLAGGED"),
        has_attachments,
        has_invite,
        has_links,
        has_from_me: is_from_me,
        category: ProviderCategory::new(String::new()).map_err(contract_error)?,
        revision: Some(ProviderRevision::new("imap-derived-v1").map_err(contract_error)?),
    };
    let membership = ContainerMembershipChange::Upsert {
        membership: ContainerMembership {
            message: identity,
            container: RemoteContainerIdentity {
                mux_account_id: account_id.clone(),
                remote_container_id: RemoteContainerId::new(folder.container_id.clone())
                    .map_err(contract_error)?,
            },
        },
    };
    Ok(ProjectedImapMessage {
        thread,
        message,
        membership,
        restricted_content,
    })
}

fn next_selectable_folder(folders: &[FolderPlan], after: usize) -> Option<usize> {
    folders
        .iter()
        .enumerate()
        .skip(after.saturating_add(1))
        .find_map(|(index, folder)| folder.selectable().then_some(index))
}

#[allow(clippy::too_many_arguments)]
fn execute_scan(
    session: &mut dyn ImapSession,
    account_id: &str,
    remote_account_id: &str,
    request: ImapSyncWork,
    generation_id: String,
    account_cursor: String,
    folders: Vec<FolderPlan>,
    folder_index: usize,
    selected: SelectedPlan,
    stage: ScanStage,
    mut next_uid: u32,
    mut pending_uids: Vec<u32>,
    now_ms: i64,
) -> Result<ProviderSyncPage, ImapError> {
    validate_folder_plans(&folders)?;
    let folder = folders
        .get(folder_index)
        .cloned()
        .ok_or(ImapError::Permanent)?;
    let expected_checkpoint = folder_cursor(&folder, &selected, Some(&request.phase))?;
    if request.expected_prior_cursor.as_deref() != Some(expected_checkpoint.as_str()) {
        return Err(ImapError::Permanent);
    }
    let observed = session.examine(
        &folder.wire_name,
        session.capabilities().supports_condstore(),
    )?;
    if observed.uid_next < selected.uid_next && observed.uid_validity == selected.uid_validity {
        return Err(ImapError::Protocol);
    }
    let modseq_regressed = match (selected.highest_modseq, observed.highest_modseq) {
        (Some(expected), Some(current)) => current < expected,
        (Some(_), None) => true,
        _ => false,
    };
    if observed.uid_validity != selected.uid_validity || modseq_regressed {
        let restarted = SelectedPlan {
            uid_validity: observed.uid_validity,
            uid_next: observed.uid_next,
            highest_modseq: observed.highest_modseq,
            baseline_modseq: None,
        };
        let next_phase = ImapSyncPhase::Scan {
            generation_id: generation_id.clone(),
            account_cursor,
            folders,
            folder_index,
            selected: restarted.clone(),
            stage: ScanStage::Full,
            next_uid: 1,
            pending_uids: Vec::new(),
        };
        let cursor_value = folder_cursor(&folder, &restarted, Some(&next_phase))?;
        let next = make_work(next_phase, Some(cursor_value.clone()))?;
        return build_sync_page(
            account_id,
            request,
            SyncCursorScope::Container {
                remote_container_id: RemoteContainerId::new(folder.container_id)
                    .map_err(contract_error)?,
            },
            cursor_value,
            now_ms,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some(next),
            false,
            None,
            Some(ProviderReconciliationPage {
                generation_id,
                begin: false,
                reset_seen_containers: false,
                complete_kinds: BTreeSet::new(),
                sweep_kinds: BTreeSet::new(),
                seen_remote_messages: Vec::new(),
            }),
        );
    }

    let mux_account_id = MuxAccountId::new(account_id.to_owned()).map_err(contract_error)?;
    let mut inventory = Vec::new();
    if pending_uids.is_empty() && next_uid < selected.uid_next {
        let end = next_uid
            .saturating_add(UID_SEARCH_WINDOW.saturating_sub(1))
            .min(selected.uid_next.saturating_sub(1));
        let changed_since = if stage == ScanStage::Delta {
            Some(selected.baseline_modseq.ok_or(ImapError::Permanent)?)
        } else {
            None
        };
        let found = session.search_uid_range(next_uid, end, changed_since)?;
        next_uid = end.checked_add(1).ok_or(ImapError::Permanent)?;
        if stage == ScanStage::Inventory {
            inventory = found
                .into_iter()
                .map(|uid| {
                    remote_message_identity(
                        &mux_account_id,
                        &folder,
                        selected.uid_validity,
                        uid,
                        None,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
        } else {
            pending_uids = found;
        }
    }

    let mut thread_by_id = BTreeMap::new();
    let mut messages = Vec::new();
    let mut memberships = Vec::new();
    let mut restricted = Vec::new();
    if stage != ScanStage::Inventory && !pending_uids.is_empty() {
        let take = pending_uids.len().min(UID_FETCH_PAGE);
        let requested = pending_uids[..take].to_vec();
        let fetched = session.fetch_messages(&requested, selected.highest_modseq.is_some())?;
        pending_uids.drain(..take);
        for fetched in fetched {
            let projected = project_imap_message(
                &mux_account_id,
                remote_account_id,
                &folder,
                &selected,
                fetched,
            )?;
            let thread_id = projected
                .thread
                .identity
                .remote_thread_id
                .as_str()
                .to_owned();
            let replace =
                thread_by_id
                    .get(&thread_id)
                    .is_none_or(|prior: &ProviderThreadUpsert| {
                        prior.latest_at < projected.thread.latest_at
                    });
            if replace {
                thread_by_id.insert(thread_id, projected.thread);
            }
            messages.push(projected.message);
            memberships.push(projected.membership);
            if let Some(content) = projected.restricted_content {
                restricted.push(content);
            }
        }
    }

    // Only an inventory pass finishes a folder. Fetching stages know what
    // changed but not what is still there, and reconciliation refuses to
    // tombstone a container it has never seen in full — so a scan that skipped
    // the inventory would leave deleted mail live forever.
    let folder_finished =
        pending_uids.is_empty() && next_uid >= selected.uid_next && stage == ScanStage::Inventory;
    let (next_phase, next_expected_cursor, cursor_value) = if folder_finished {
        let cursor_value = folder_cursor(&folder, &selected, None)?;
        if let Some(next_index) = next_selectable_folder(&folders, folder_index) {
            let expected = folders[next_index].prior_cursor.clone();
            (
                ImapSyncPhase::Select {
                    generation_id: generation_id.clone(),
                    account_cursor,
                    folders,
                    folder_index: next_index,
                },
                expected,
                cursor_value,
            )
        } else {
            (
                ImapSyncPhase::Sweep {
                    generation_id: generation_id.clone(),
                    sweep_index: 0,
                },
                Some(account_cursor),
                cursor_value,
            )
        }
    } else {
        // A fetching stage that has run out of UIDs hands over to the
        // inventory, which walks the folder once more to report what remains.
        let (next_stage, next_stage_uid) = if stage != ScanStage::Inventory
            && pending_uids.is_empty()
            && next_uid >= selected.uid_next
        {
            (ScanStage::Inventory, 1)
        } else {
            (stage, next_uid)
        };
        let next_phase = ImapSyncPhase::Scan {
            generation_id: generation_id.clone(),
            account_cursor,
            folders,
            folder_index,
            selected: selected.clone(),
            stage: next_stage,
            next_uid: next_stage_uid,
            pending_uids,
        };
        let cursor_value = folder_cursor(&folder, &selected, Some(&next_phase))?;
        (next_phase, Some(cursor_value.clone()), cursor_value)
    };
    let next = make_work(next_phase, next_expected_cursor)?;
    build_sync_page(
        account_id,
        request,
        SyncCursorScope::Container {
            remote_container_id: RemoteContainerId::new(folder.container_id)
                .map_err(contract_error)?,
        },
        cursor_value,
        now_ms,
        thread_by_id.into_values().collect(),
        messages,
        Vec::new(),
        memberships,
        restricted,
        Some(next),
        false,
        None,
        Some(ProviderReconciliationPage {
            generation_id,
            begin: false,
            reset_seen_containers: false,
            complete_kinds: BTreeSet::new(),
            sweep_kinds: BTreeSet::new(),
            seen_remote_messages: inventory,
        }),
    )
}

fn execute_sweep(
    account_id: &str,
    request: ImapSyncWork,
    generation_id: String,
    sweep_index: u32,
    now_ms: i64,
) -> Result<ProviderSyncPage, ImapError> {
    let expected = account_cursor(&generation_id, sweep_index)?;
    if request.expected_prior_cursor.as_deref() != Some(expected.as_str()) {
        return Err(ImapError::Permanent);
    }
    let next_index = sweep_index.checked_add(1).ok_or(ImapError::Permanent)?;
    let cursor_value = account_cursor(&generation_id, next_index)?;
    let next = make_work(
        ImapSyncPhase::Sweep {
            generation_id: generation_id.clone(),
            sweep_index: next_index,
        },
        Some(cursor_value.clone()),
    )?;
    let complete_kinds = [
        ReconciliationObjectKind::Containers,
        ReconciliationObjectKind::Threads,
        ReconciliationObjectKind::Messages,
    ]
    .into_iter()
    .collect();
    build_sync_page(
        account_id,
        request,
        SyncCursorScope::Account,
        cursor_value,
        now_ms,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Some(next),
        false,
        None,
        Some(ProviderReconciliationPage {
            generation_id,
            begin: false,
            reset_seen_containers: false,
            complete_kinds,
            sweep_kinds: [
                ReconciliationObjectKind::Containers,
                ReconciliationObjectKind::Threads,
                ReconciliationObjectKind::Messages,
            ]
            .into_iter()
            .collect(),
            seen_remote_messages: Vec::new(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_conformance::apply_worker_projection;
    use crate::store::MuxStore;
    use crate::worker::{DurableWorker, WorkerConfig};
    use std::io::Cursor;
    use tempfile::tempdir;

    fn capabilities(values: &[&str]) -> ImapCapabilities {
        ImapCapabilities::new(values.iter().map(|value| (*value).to_owned()))
            .expect("valid capabilities")
    }

    fn inbox() -> ImapFolder {
        ImapFolder {
            wire_name: "INBOX".into(),
            display_name: "INBOX".into(),
            delimiter: Some('/'),
            flags: ["\\INBOX".to_owned()].into_iter().collect(),
        }
    }

    fn raw_message(uid: u32) -> Vec<u8> {
        format!(
            "From: Sender <sender@example.test>\r\n\
             To: Reader <reader@example.test>\r\n\
             Subject: Message {uid}\r\n\
             Message-ID: <message-{uid}@example.test>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Body {uid}\r\n"
        )
        .into_bytes()
    }

    fn fetched(uid: u32, modseq: Option<u64>) -> ImapFetchedMessage {
        ImapFetchedMessage {
            uid,
            flags: if uid == 1 {
                ["\\SEEN".to_owned()].into_iter().collect()
            } else {
                BTreeSet::new()
            },
            internal_date: "17-Jul-1996 02:44:25 -0700".into(),
            modseq,
            raw: raw_message(uid),
        }
    }

    struct ScriptedSession {
        capabilities: ImapCapabilities,
        folders: Vec<ImapFolder>,
        selected: Vec<ImapSelectedFolder>,
        searches: Vec<(u32, u32, Option<u64>, Vec<u32>)>,
        fetches: BTreeMap<u32, ImapFetchedMessage>,
        examined: Vec<(String, bool)>,
    }

    impl ScriptedSession {
        fn new(selected: Vec<ImapSelectedFolder>) -> Self {
            Self {
                capabilities: capabilities(&["IMAP4rev1", "CONDSTORE"]),
                folders: vec![inbox()],
                selected,
                searches: Vec::new(),
                fetches: BTreeMap::new(),
                examined: Vec::new(),
            }
        }
    }

    impl ImapSession for ScriptedSession {
        fn capabilities(&self) -> &ImapCapabilities {
            &self.capabilities
        }

        fn list_folders(&mut self) -> Result<Vec<ImapFolder>, ImapError> {
            Ok(self.folders.clone())
        }

        fn examine(
            &mut self,
            folder: &str,
            request_condstore: bool,
        ) -> Result<ImapSelectedFolder, ImapError> {
            self.examined.push((folder.to_owned(), request_condstore));
            if self.selected.is_empty() {
                return Err(ImapError::Protocol);
            }
            Ok(self.selected.remove(0))
        }

        fn search_uid_range(
            &mut self,
            start: u32,
            end: u32,
            changed_since: Option<u64>,
        ) -> Result<Vec<u32>, ImapError> {
            let Some((expected_start, expected_end, expected_modseq, values)) =
                self.searches.first().cloned()
            else {
                return Err(ImapError::Protocol);
            };
            self.searches.remove(0);
            if (start, end, changed_since) != (expected_start, expected_end, expected_modseq) {
                return Err(ImapError::Protocol);
            }
            Ok(values)
        }

        fn fetch_messages(
            &mut self,
            uids: &[u32],
            _request_modseq: bool,
        ) -> Result<Vec<ImapFetchedMessage>, ImapError> {
            uids.iter()
                .map(|uid| self.fetches.get(uid).cloned().ok_or(ImapError::Protocol))
                .collect()
        }
    }

    fn continuation(page: &ProviderSyncPage) -> ImapSyncWork {
        serde_json::from_str(
            &page
                .continuation
                .as_ref()
                .expect("continuation")
                .payload_json,
        )
        .expect("valid IMAP continuation")
    }

    fn initial_request(generation: &str, prior_folders: Vec<StoredFolderCursor>) -> ImapSyncWork {
        make_work(
            ImapSyncPhase::Discover {
                generation_id: generation.into(),
                prior_folders,
            },
            None,
        )
        .expect("initial request")
    }

    #[test]
    fn imap_full_sync_walks_discover_select_scan_and_sweep_with_bounded_checkpoints() {
        let selected = ImapSelectedFolder {
            uid_validity: 7,
            uid_next: 3,
            highest_modseq: Some(20),
        };
        let mut session = ScriptedSession::new(vec![selected, selected, selected]);
        // One search for the fetching pass, one for the inventory that follows it.
        session.searches.push((1, 2, None, vec![1, 2]));
        session.searches.push((1, 2, None, vec![1, 2]));
        session.fetches.insert(1, fetched(1, Some(19)));
        session.fetches.insert(2, fetched(2, Some(20)));

        let discover = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            initial_request("generation-full", Vec::new()),
            1_000,
        )
        .expect("discover page");
        assert!(matches!(
            discover.batch.cursor.scope,
            SyncCursorScope::Account
        ));
        assert_eq!(discover.batch.container_upserts.len(), 1);
        assert!(discover
            .reconciliation
            .as_ref()
            .is_some_and(|value| value.begin));

        let select = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            continuation(&discover),
            1_001,
        )
        .expect("select page");
        assert!(matches!(
            continuation(&select).phase,
            ImapSyncPhase::Scan {
                stage: ScanStage::Full,
                ..
            }
        ));
        let SyncCursorScope::Container {
            remote_container_id,
        } = &select.batch.cursor.scope
        else {
            panic!("select must checkpoint its container")
        };
        let checkpoint = DurableFolderCursor::decode(
            select.batch.cursor.value.as_str(),
            remote_container_id.as_str(),
        )
        .expect("checkpoint cursor");
        assert!(!checkpoint.complete);

        let scan = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            continuation(&select),
            1_002,
        )
        .expect("scan page");
        assert_eq!(scan.batch.message_upserts.len(), 2);
        assert_eq!(scan.batch.membership_changes.len(), 2);
        assert_eq!(scan.restricted_message_content.len(), 2);
        assert!(scan.batch.message_upserts[1]
            .keywords
            .iter()
            .any(|keyword| keyword.as_str() == "unread"));
        // Fetching knows what changed but not what is still there, so the folder
        // is not finished until an inventory pass has reported its UIDs.
        assert!(matches!(
            continuation(&scan).phase,
            ImapSyncPhase::Scan {
                stage: ScanStage::Inventory,
                ..
            }
        ));

        let inventory = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            continuation(&scan),
            1_003,
        )
        .expect("inventory page");
        assert!(inventory.batch.message_upserts.is_empty());
        assert_eq!(
            inventory
                .reconciliation
                .as_ref()
                .expect("inventory reconciliation")
                .seen_remote_messages
                .len(),
            2,
            "the inventory reports every UID still present"
        );
        assert!(matches!(
            continuation(&inventory).phase,
            ImapSyncPhase::Sweep { sweep_index: 0, .. }
        ));
        let SyncCursorScope::Container {
            remote_container_id,
        } = &inventory.batch.cursor.scope
        else {
            panic!("inventory must checkpoint its container")
        };
        let durable = DurableFolderCursor::decode(
            inventory.batch.cursor.value.as_str(),
            remote_container_id.as_str(),
        )
        .expect("complete cursor");
        assert!(durable.complete);

        let sweep = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            continuation(&inventory),
            1_004,
        )
        .expect("sweep page");
        assert!(matches!(sweep.batch.cursor.scope, SyncCursorScope::Account));
        let reconciliation = sweep.reconciliation.expect("sweep reconciliation");
        assert_eq!(reconciliation.complete_kinds.len(), 3);
        assert_eq!(reconciliation.sweep_kinds.len(), 3);
        assert!(session.searches.is_empty());
    }

    #[test]
    fn imap_condstore_delta_fetches_changes_then_inventory_without_refetching() {
        let folder = FolderPlan {
            wire_name: "INBOX".into(),
            display_name: "INBOX".into(),
            delimiter: Some('/'),
            flags: vec!["\\INBOX".into()],
            container_id: format!("imap-folder-{}", &sha256_hex(b"INBOX")[..40]),
            prior_cursor: None,
        };
        let prior = folder_cursor(
            &folder,
            &SelectedPlan {
                uid_validity: 7,
                uid_next: 3,
                highest_modseq: Some(10),
                baseline_modseq: None,
            },
            None,
        )
        .expect("prior folder cursor");
        let current = ImapSelectedFolder {
            uid_validity: 7,
            uid_next: 4,
            highest_modseq: Some(20),
        };
        let mut session = ScriptedSession::new(vec![current, current, current]);
        session
            .searches
            .extend([(1, 3, Some(10), vec![3]), (1, 3, None, vec![1, 2, 3])]);
        session.fetches.insert(3, fetched(3, Some(20)));
        let discover = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            initial_request(
                "generation-delta",
                vec![StoredFolderCursor {
                    container_id: folder.container_id.clone(),
                    cursor: prior,
                }],
            ),
            2_000,
        )
        .expect("discover");
        let select = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            continuation(&discover),
            2_001,
        )
        .expect("select");
        assert!(matches!(
            continuation(&select).phase,
            ImapSyncPhase::Scan {
                stage: ScanStage::Delta,
                ..
            }
        ));
        let changed = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            continuation(&select),
            2_002,
        )
        .expect("delta page");
        assert_eq!(changed.batch.message_upserts.len(), 1);
        assert!(matches!(
            continuation(&changed).phase,
            ImapSyncPhase::Scan {
                stage: ScanStage::Inventory,
                ..
            }
        ));
        let inventory = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            continuation(&changed),
            2_003,
        )
        .expect("inventory page");
        assert!(inventory.batch.message_upserts.is_empty());
        assert_eq!(
            inventory
                .reconciliation
                .as_ref()
                .expect("inventory evidence")
                .seen_remote_messages
                .len(),
            3
        );
        assert!(session.searches.is_empty());
    }

    #[test]
    fn imap_scan_restarts_from_uid_one_when_uidvalidity_changes_mid_generation() {
        let folder = FolderPlan {
            wire_name: "INBOX".into(),
            display_name: "INBOX".into(),
            delimiter: Some('/'),
            flags: vec!["\\INBOX".into()],
            container_id: "imap-folder-restart".into(),
            prior_cursor: None,
        };
        let selected = SelectedPlan {
            uid_validity: 7,
            uid_next: 9,
            highest_modseq: Some(20),
            baseline_modseq: None,
        };
        let phase = ImapSyncPhase::Scan {
            generation_id: "generation-restart".into(),
            account_cursor: account_cursor("generation-restart", 0).expect("account cursor"),
            folders: vec![folder.clone()],
            folder_index: 0,
            selected: selected.clone(),
            stage: ScanStage::Full,
            next_uid: 5,
            pending_uids: Vec::new(),
        };
        let expected = folder_cursor(&folder, &selected, Some(&phase)).expect("checkpoint");
        let request = make_work(phase, Some(expected)).expect("restart work");
        let mut session = ScriptedSession::new(vec![ImapSelectedFolder {
            uid_validity: 8,
            uid_next: 3,
            highest_modseq: Some(2),
        }]);
        let page = execute_sync_page(
            &mut session,
            "imap-account",
            "reader@example.test",
            request,
            3_000,
        )
        .expect("restart page");
        match continuation(&page).phase {
            ImapSyncPhase::Scan {
                selected,
                stage,
                next_uid,
                pending_uids,
                ..
            } => {
                assert_eq!(selected.uid_validity, 8);
                assert_eq!(stage, ScanStage::Full);
                assert_eq!(next_uid, 1);
                assert!(pending_uids.is_empty());
            }
            other => panic!("expected restarted scan, got {other:?}"),
        }
    }

    #[test]
    fn malformed_mime_degrades_to_unavailable_without_skipping_remote_identity() {
        let account = MuxAccountId::new("imap-account").expect("account");
        let folder = FolderPlan {
            wire_name: "INBOX".into(),
            display_name: "INBOX".into(),
            delimiter: Some('/'),
            flags: vec!["\\INBOX".into()],
            container_id: "imap-folder-hostile".into(),
            prior_cursor: None,
        };
        let projected = project_imap_message(
            &account,
            "reader@example.test",
            &folder,
            &SelectedPlan {
                uid_validity: 2,
                uid_next: 3,
                highest_modseq: None,
                baseline_modseq: None,
            },
            ImapFetchedMessage {
                uid: 1,
                flags: BTreeSet::new(),
                internal_date: "17-Jul-1996 02:44:25 -0700".into(),
                modseq: None,
                raw: vec![0xff, 0xfe, 0xfd],
            },
        )
        .expect("bounded fallback projection");
        assert_eq!(projected.message.body_state, ProviderBodyState::Unavailable);
        assert!(projected.restricted_content.is_none());
        assert_eq!(
            projected.message.identity.remote_message_id.as_str(),
            "imap:imap-folder-hostile:2:1"
        );
    }

    struct Transcript {
        read: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Transcript {
        fn new(read: impl Into<Vec<u8>>) -> Self {
            Self {
                read: Cursor::new(read.into()),
                written: Vec::new(),
            }
        }
    }

    impl Read for Transcript {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.read.read(buffer)
        }
    }

    impl Write for Transcript {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn imap_wire_parses_modified_utf7_select_search_and_exact_fetch_literals() {
        let list_io = Transcript::new(
            b"* OK ready\r\n* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n* LIST (\\Sent) \"/\" \"Sent &AMk-l\"\r\nM000001 OK list\r\n"
                .to_vec(),
        );
        let mut list = ImapWireSession::new(Box::new(list_io)).expect("greeting");
        list.capabilities = capabilities(&["IMAP4rev1"]);
        let folders = list.list_folders().expect("LIST parse");
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[1].display_name, "Sent Él");

        let examine_io = Transcript::new(
            b"* OK ready\r\n* OK [UIDVALIDITY 9] stable\r\n* OK [UIDNEXT 4] next\r\n* OK [HIGHESTMODSEQ 22] modseq\r\nM000001 OK selected\r\n"
                .to_vec(),
        );
        let mut examine = ImapWireSession::new(Box::new(examine_io)).expect("greeting");
        examine.capabilities = capabilities(&["IMAP4rev1", "CONDSTORE"]);
        assert_eq!(
            examine.examine("INBOX", true).expect("EXAMINE parse"),
            ImapSelectedFolder {
                uid_validity: 9,
                uid_next: 4,
                highest_modseq: Some(22),
            }
        );

        let search_io = Transcript::new(
            b"* OK ready\r\n* SEARCH 1 3 (MODSEQ 22)\r\nM000001 OK search\r\n".to_vec(),
        );
        let mut search = ImapWireSession::new(Box::new(search_io)).expect("greeting");
        search.capabilities = capabilities(&["IMAP4rev1", "CONDSTORE"]);
        assert_eq!(
            search
                .search_uid_range(1, 3, Some(10))
                .expect("SEARCH parse"),
            vec![1, 3]
        );

        let raw = raw_message(1);
        let mut fetch_transcript = b"* OK ready\r\n* 1 FETCH (UID 1 FLAGS (\\Seen) INTERNALDATE \"17-Jul-1996 02:44:25 -0700\" MODSEQ (22) BODY[] {"
            .to_vec();
        fetch_transcript.extend_from_slice(raw.len().to_string().as_bytes());
        fetch_transcript.extend_from_slice(b"}\r\n");
        fetch_transcript.extend_from_slice(&raw);
        fetch_transcript.extend_from_slice(b")\r\nM000001 OK fetch\r\n");
        let mut fetch =
            ImapWireSession::new(Box::new(Transcript::new(fetch_transcript))).expect("greeting");
        fetch.capabilities = capabilities(&["IMAP4rev1", "CONDSTORE"]);
        let messages = fetch.fetch_messages(&[1], true).expect("FETCH parse");
        assert_eq!(messages, vec![fetched(1, Some(22))]);

        let omitted_io = Transcript::new(b"* OK ready\r\nM000001 OK fetch omitted\r\n".to_vec());
        let mut omitted = ImapWireSession::new(Box::new(omitted_io)).expect("greeting");
        omitted.capabilities = capabilities(&["IMAP4rev1"]);
        assert_eq!(
            omitted.fetch_messages(&[1], false),
            Err(ImapError::Protocol),
            "missing requested FETCH rows must not be reconciled as absent"
        );
    }

    #[test]
    fn durable_work_validators_reject_duplicate_folders_controls_and_oversized_payloads() {
        let cursor = DurableFolderCursor {
            version: WORK_FORMAT_VERSION,
            provider: "imap".into(),
            container_id: "one".into(),
            uid_validity: 1,
            uid_next: 1,
            highest_modseq: None,
            complete: true,
            checkpoint: None,
        }
        .encode()
        .expect("cursor");
        assert!(validate_stored_folders(&[
            StoredFolderCursor {
                container_id: "one".into(),
                cursor: cursor.clone(),
            },
            StoredFolderCursor {
                container_id: "one".into(),
                cursor,
            },
        ])
        .is_err());
        assert!(validate_generation("bad\nvalue").is_err());

        let mut oversized = initial_request("bounded", Vec::new());
        if let ImapSyncPhase::Discover { prior_folders, .. } = &mut oversized.phase {
            prior_folders.push(StoredFolderCursor {
                container_id: "x".repeat(MAX_WORK_BYTES),
                cursor: "cursor".into(),
            });
        }
        assert!(oversized.validate().is_err());
    }

    struct FixtureAccess;

    impl ImapAccessSource for FixtureAccess {
        fn access_for_account(&self, account_id: &str) -> Result<ImapAccessGrant, ImapAccessError> {
            if account_id != "imap-account" {
                return Err(ImapAccessError::ReauthorizationRequired);
            }
            Ok(ImapAccessGrant {
                host: "imap.example.test".into(),
                port: 993,
                username: "reader@example.test".into(),
                password: Zeroizing::new("fixture-secret".into()),
                remote_account_id: "reader@example.test".into(),
            })
        }
    }

    struct MailboxSession {
        capabilities: ImapCapabilities,
        uids: Vec<u32>,
    }

    impl ImapSession for MailboxSession {
        fn capabilities(&self) -> &ImapCapabilities {
            &self.capabilities
        }

        fn list_folders(&mut self) -> Result<Vec<ImapFolder>, ImapError> {
            Ok(vec![inbox()])
        }

        fn examine(
            &mut self,
            folder: &str,
            request_condstore: bool,
        ) -> Result<ImapSelectedFolder, ImapError> {
            if folder != "INBOX" || request_condstore {
                return Err(ImapError::Protocol);
            }
            Ok(ImapSelectedFolder {
                uid_validity: 9,
                uid_next: self
                    .uids
                    .last()
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1)
                    .max(1),
                highest_modseq: None,
            })
        }

        fn search_uid_range(
            &mut self,
            start: u32,
            end: u32,
            changed_since: Option<u64>,
        ) -> Result<Vec<u32>, ImapError> {
            if changed_since.is_some() {
                return Err(ImapError::Protocol);
            }
            Ok(self
                .uids
                .iter()
                .copied()
                .filter(|uid| (start..=end).contains(uid))
                .collect())
        }

        fn fetch_messages(
            &mut self,
            uids: &[u32],
            request_modseq: bool,
        ) -> Result<Vec<ImapFetchedMessage>, ImapError> {
            if request_modseq || uids.iter().any(|uid| !self.uids.contains(uid)) {
                return Err(ImapError::Protocol);
            }
            Ok(uids.iter().map(|uid| fetched(*uid, None)).collect())
        }
    }

    struct MailboxFactory {
        uids: Vec<u32>,
    }

    /// A factory that fails the way a real server does when the details are
    /// wrong, so verification can be proved without one.
    struct RefusingFactory {
        error: ImapError,
    }

    impl ImapSessionFactory for RefusingFactory {
        fn open(&self, _grant: &ImapAccessGrant) -> Result<Box<dyn ImapSession>, ImapError> {
            Err(self.error)
        }
    }

    fn verification_grant() -> ImapAccessGrant {
        ImapAccessGrant {
            host: "mail.example.test".into(),
            port: 993,
            username: "reader@example.test".into(),
            password: Zeroizing::new("swordfish".into()),
            remote_account_id: "reader@example.test".into(),
        }
    }

    #[test]
    fn verifying_a_mailbox_reports_what_the_server_showed() {
        let folders =
            verify_authority_with(&MailboxFactory { uids: vec![1, 2] }, &verification_grant())
                .expect("verified");
        // MailboxFactory lists one folder, and a listable mailbox is the least
        // a sync needs.
        assert_eq!(folders, 1);
    }

    #[test]
    fn verification_passes_the_refusal_through_rather_than_flattening_it() {
        // The interface says different things for a wrong password and an
        // unreachable host, so these must not collapse into one error.
        for error in [
            ImapError::Authentication,
            ImapError::Transient,
            ImapError::Permanent,
            ImapError::Protocol,
        ] {
            assert_eq!(
                verify_authority_with(&RefusingFactory { error }, &verification_grant()),
                Err(error)
            );
        }
    }

    impl ImapSessionFactory for MailboxFactory {
        fn open(&self, _grant: &ImapAccessGrant) -> Result<Box<dyn ImapSession>, ImapError> {
            Ok(Box::new(MailboxSession {
                capabilities: capabilities(&["IMAP4rev1"]),
                uids: self.uids.clone(),
            }))
        }
    }

    /// Drives the worker until nothing is claimable. `start_ms` has to be at or
    /// after the work's availability, or every cycle claims nothing and the
    /// fixture looks idle while its sync has not run at all.
    fn run_sync_to_idle(path: &Path, adapter: &(impl WorkerAdapter + Sync), start_ms: i64) {
        let worker = DurableWorker::new(path, WorkerConfig::default()).expect("worker");
        for cycle_index in 0..12 {
            let now = start_ms + cycle_index;
            let result = worker
                .run_cycle(
                    "imap-fixture-worker",
                    adapter,
                    &apply_worker_projection,
                    &|| now,
                )
                .expect("worker cycle");
            assert!(
                result.errors.is_empty(),
                "worker errors: {:?}",
                result.errors
            );
            if result.claimed == 0 {
                return;
            }
            assert_eq!(result.succeeded, 1);
        }
        panic!("IMAP fixture did not become idle within its bounded phase count");
    }

    #[test]
    fn imap_provider_conformance_commits_pages_and_reconciles_removed_uids_atomically() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("imap-provider-conformance.db");
        drop(MuxStore::open(&path, false).expect("schema"));
        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('imap-account', 'IMAP', 'reader@example.test', '#000000', 'imap')",
                [],
            )
            .expect("account");
        connection
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(
                   'imap-account', 'imap', 'reader@example.test', 'ready',
                   'opaque-keychain-reference', 'never_synced', 0, 0
                 )",
                [],
            )
            .expect("provider account");
        drop(connection);

        assert_eq!(
            schedule_initial_syncs(&path, 1_000).expect("initial schedule"),
            1
        );
        let initial =
            ImapAdapter::new(FixtureAccess, MailboxFactory { uids: vec![1, 2] }, || 2_000);
        run_sync_to_idle(&path, &initial, 10_000);
        let verify = Connection::open(&path).expect("first verification");
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE remote_deleted = 0",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("live messages"),
            2
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT sync_state FROM provider_accounts WHERE account_id = 'imap-account'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("sync state"),
            "idle"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_sync_cursors
                     WHERE account_id = 'imap-account'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("cursor count"),
            2,
            "account and folder cursors must both be durable"
        );
        drop(verify);

        assert_eq!(
            sync_account_now(&path, "imap-account", 20_000).expect("rescan"),
            1
        );
        let rescan = ImapAdapter::new(FixtureAccess, MailboxFactory { uids: vec![1] }, || 21_000);
        run_sync_to_idle(&path, &rescan, 21_000);
        let verify = Connection::open(&path).expect("second verification");
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE remote_deleted = 0",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("remaining live messages"),
            1
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_runs",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("reconciliation runs"),
            0
        );
    }
}
