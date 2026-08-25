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
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use rustls_platform_verifier::ConfigVerifierExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::content::{parse_mime, ParsedMailbox, SafeMessageContent};
use crate::provider::{
    ContainerDisplayName, ContainerKind, ContainerMembership, ContainerMembershipChange,
    ContainerRole, MuxAccountId, NormalizedPlainBody, OpaqueSyncCursor, ProviderBatch,
    ProviderBatchId, ProviderBodyState, ProviderCapabilities, ProviderCapability,
    ProviderCategory, ProviderContainer, ProviderEmail, ProviderMessageKeyword,
    ProviderMessageUpsert, ProviderParticipants, ProviderRecipients, ProviderRevision,
    ProviderSenderName, ProviderSnippet, ProviderSubject, ProviderSyncCursor,
    ProviderThreadMessageCount, ProviderThreadUpsert, RemoteContainerId,
    RemoteContainerIdentity, RemoteMessageId, RemoteMessageIdentity, RemoteThreadId,
    RemoteThreadIdentity, SyncCursorScope, UnixMillis,
};
use crate::worker::{
    enqueue_in_transaction, ClaimedWork, NewWorkItem, ProviderReconciliationPage,
    ProviderSyncContinuation, ProviderSyncPage, WorkKind, WorkerAdapter, WorkerError,
    WorkerExecutionContext, WorkerOutcome, WorkerProjection,
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
    Locked,
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
    ModseqRegressed,
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
                || value.bytes().any(|byte| byte.is_ascii_control() || byte == b' ')
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
    fn selectable(&self) -> bool {
        !self.flags.contains("\\NOSELECT")
    }

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

pub(crate) trait ImapSessionFactory {
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
            if line.starts_with(tag.as_bytes())
                && line.get(tag.len()) == Some(&b' ')
            {
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
        let uid_validity = uid_validity.filter(|value| *value > 0).ok_or(ImapError::Protocol)?;
        let uid_next = uid_next.filter(|value| *value > 0).ok_or(ImapError::Protocol)?;
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
            || uids.iter().any(|uid| *uid == 0)
            || uids.windows(2).any(|window| window[0] >= window[1])
        {
            return Err(ImapError::Permanent);
        }
        let uid_set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let fields = if request_modseq && self.capabilities.supports_condstore() {
            "UID FLAGS INTERNALDATE MODSEQ BODY.PEEK[]"
        } else {
            "UID FLAGS INTERNALDATE BODY.PEEK[]"
        };
        let response = self.command(&format!("UID FETCH {uid_set} ({fields})"), MAX_LITERAL_BYTES)?;
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
        if messages.len() > uids.len()
            || messages.windows(2).any(|window| window[0].uid == window[1].uid)
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
        while standard.len() % 4 != 0 {
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
    let date_start = upper.find(internal_marker).ok_or(ImapError::Protocol)? + internal_marker.len();
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
                    || pending_uids.len() > MAX_UIDS_PER_SEARCH
                    || pending_uids.iter().any(|uid| *uid == 0)
                    || pending_uids.windows(2).any(|window| window[0] >= window[1])
                    || (*stage == ScanStage::Delta && selected.baseline_modseq.is_none())
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

pub(crate) fn is_imap_sync_work(work: &ClaimedWork) -> bool {
    work.kind == WorkKind::Sync && work.scope == IMAP_ACCOUNT_SCOPE
}

pub(crate) fn schedule_initial_syncs(
    database_path: &Path,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    schedule_syncs(database_path, now_ms, true)
}

pub(crate) fn schedule_resumable_syncs(
    database_path: &Path,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    schedule_syncs(database_path, now_ms, false)
}

fn schedule_syncs(
    database_path: &Path,
    now_ms: i64,
    initial_only: bool,
) -> Result<usize, WorkerError> {
    if now_ms < 0 {
        return Err(WorkerError::Validation(
            "IMAP sync timestamp is invalid".into(),
        ));
    }
    let mut connection = Connection::open(database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let state = if initial_only {
        "account.sync_state = 'never_synced'"
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
            .query_map([IMAP_ACCOUNT_SCOPE], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    for (index, (account_id, account_cursor)) in accounts.iter().enumerate() {
        let prior_folders = read_stored_folder_cursors(&transaction, account_id)?;
        let generation_id = format!("imap-scan-{now_ms}-{index}");
        let request = make_work(
            ImapSyncPhase::Discover {
                generation_id,
                prior_folders,
            },
            account_cursor.clone(),
        )
        .map_err(|_| WorkerError::Validation("IMAP work could not be constructed".into()))?;
        enqueue_in_transaction(&transaction, work_item(account_id, request, now_ms)?, now_ms)?;
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

pub(crate) fn initial_sync_work(
    account_id: &str,
    request_id: &str,
    now_ms: i64,
) -> Result<NewWorkItem, WorkerError> {
    if request_id.is_empty()
        || request_id.len() > 128
        || request_id.chars().any(char::is_control)
        || now_ms < 0
    {
        return Err(WorkerError::Validation(
            "IMAP initial sync input is invalid".into(),
        ));
    }
    let request = make_work(
        ImapSyncPhase::Discover {
            generation_id: format!("imap-scan-{request_id}"),
            prior_folders: Vec::new(),
        },
        None,
    )
    .map_err(|_| WorkerError::Validation("IMAP initial sync work is invalid".into()))?;
    work_item(account_id, request, now_ms)
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
            Ok(request)
                if request.validate().is_ok() && request.batch_id == work.ordering_key =>
            {
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
        } => execute_sweep(
            account_id,
            request,
            generation_id,
            sweep_index,
            now_ms,
        ),
    }
}
