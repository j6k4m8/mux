//! Bounded SMTP submission for IMAP-backed accounts.
//!
//! SMTP owns only the non-idempotent submission. The immutable outgoing MIME
//! snapshot is built by `outgoing`, credentials stay behind `SmtpAccessSource`,
//! and the durable send fence is crossed immediately before `DATA` can make the
//! request delivery-capable. No SMTP response is projected as provider mail;
//! accepted sends use the existing local sent projection and are adopted when
//! the IMAP Sent folder later observes the correlation header.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use rustls_platform_verifier::ConfigVerifierExt as _;
use zeroize::Zeroizing;

use crate::outgoing::PreparedOutgoingMessage;
use crate::worker::{
    ClaimedWork, WorkKind, WorkerAdapter, WorkerError, WorkerExecutionContext, WorkerOutcome,
    WorkerProjection,
};

const SEND_SCOPE_PREFIX: &str = "outgoing:imap:v1:";
const MAX_HOST_ADDRESSES: usize = 16;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_LINES: usize = 100;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_CAPABILITIES: usize = 128;
const MAX_CAPABILITY_BYTES: usize = 256;
const MAX_ENVELOPE_BYTES: usize = 2_048;
const MAX_SMTP_DATA_BYTES: usize = crate::mime_ingest::MAX_RAW_MESSAGE_BYTES * 2 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmtpTlsMode {
    Implicit,
    StartTls,
}

pub(crate) struct SmtpAccessGrant {
    pub host: String,
    pub port: u16,
    pub tls_mode: SmtpTlsMode,
    pub username: String,
    pub password: Zeroizing<String>,
    pub remote_account_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmtpAccessError {
    CredentialUnavailable,
    ReauthorizationRequired,
    Retryable,
    Permanent,
}

pub(crate) trait SmtpAccessSource {
    fn smtp_access_for_account(&self, account_id: &str)
        -> Result<SmtpAccessGrant, SmtpAccessError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SmtpError {
    Authentication,
    RetryableBeforeSubmission,
    PermanentBeforeSubmission,
    ExplicitTransientRejection,
    ExplicitPermanentRejection,
    AmbiguousSubmission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmtpVerificationError {
    Authentication,
    Transient,
    Unsupported,
}

trait SmtpSubmission {
    fn submit(
        &self,
        grant: &SmtpAccessGrant,
        message: &PreparedOutgoingMessage,
        before_data: &mut dyn FnMut() -> Result<(), SmtpError>,
    ) -> Result<(), SmtpError>;
}

pub(crate) struct NativeSmtpSubmission {
    tls: Arc<ClientConfig>,
}

impl NativeSmtpSubmission {
    pub(crate) fn new() -> Result<Self, WorkerError> {
        let tls = ClientConfig::with_platform_verifier().map_err(|_| {
            WorkerError::Conflict("Could not initialize SMTP TLS verification".into())
        })?;
        Ok(Self { tls: Arc::new(tls) })
    }

    fn connect(&self, grant: &SmtpAccessGrant) -> Result<TcpStream, SmtpError> {
        let addresses = (grant.host.as_str(), grant.port)
            .to_socket_addrs()
            .map_err(|_| SmtpError::RetryableBeforeSubmission)?
            .take(MAX_HOST_ADDRESSES + 1)
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.len() > MAX_HOST_ADDRESSES {
            return Err(SmtpError::PermanentBeforeSubmission);
        }
        let mut connected = None;
        for address in addresses {
            if let Ok(stream) = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                connected = Some(stream);
                break;
            }
        }
        let stream = connected.ok_or(SmtpError::RetryableBeforeSubmission)?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
            .map_err(|_| SmtpError::RetryableBeforeSubmission)?;
        Ok(stream)
    }

    fn tls_stream(
        &self,
        stream: TcpStream,
        host: &str,
    ) -> Result<StreamOwned<ClientConnection, TcpStream>, SmtpError> {
        let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| SmtpError::PermanentBeforeSubmission)?;
        let connection = ClientConnection::new(Arc::clone(&self.tls), server_name)
            .map_err(|_| SmtpError::PermanentBeforeSubmission)?;
        Ok(StreamOwned::new(connection, stream))
    }

    fn verify(&self, grant: &SmtpAccessGrant) -> Result<(), SmtpError> {
        let stream = self.connect(grant)?;
        match grant.tls_mode {
            SmtpTlsMode::Implicit => {
                let tls = self.tls_stream(stream, &grant.host)?;
                let mut wire = SmtpWire::new(tls)?;
                verify_authenticated_session(&mut wire, grant)
            }
            SmtpTlsMode::StartTls => {
                let wire = SmtpWire::new(stream)?;
                let stream = begin_starttls(wire)?;
                let tls = self.tls_stream(stream, &grant.host)?;
                let mut wire = SmtpWire::after_starttls(tls);
                verify_authenticated_session(&mut wire, grant)
            }
        }
    }
}

/// Proves TLS negotiation and SMTP authentication without issuing an envelope
/// or crossing the non-idempotent submission fence.
pub(crate) fn verify_authority(grant: &SmtpAccessGrant) -> Result<(), SmtpVerificationError> {
    NativeSmtpSubmission::new()
        .map_err(|_| SmtpVerificationError::Unsupported)?
        .verify(grant)
        .map_err(|error| match error {
            SmtpError::Authentication => SmtpVerificationError::Authentication,
            SmtpError::RetryableBeforeSubmission
            | SmtpError::ExplicitTransientRejection
            | SmtpError::AmbiguousSubmission => SmtpVerificationError::Transient,
            SmtpError::PermanentBeforeSubmission | SmtpError::ExplicitPermanentRejection => {
                SmtpVerificationError::Unsupported
            }
        })
}

impl SmtpSubmission for NativeSmtpSubmission {
    fn submit(
        &self,
        grant: &SmtpAccessGrant,
        message: &PreparedOutgoingMessage,
        before_data: &mut dyn FnMut() -> Result<(), SmtpError>,
    ) -> Result<(), SmtpError> {
        let stream = self.connect(grant)?;
        match grant.tls_mode {
            SmtpTlsMode::Implicit => {
                let tls = self.tls_stream(stream, &grant.host)?;
                let mut wire = SmtpWire::new(tls)?;
                let capabilities = wire.ehlo()?;
                submit_authenticated(&mut wire, grant, message, &capabilities, before_data)
            }
            SmtpTlsMode::StartTls => {
                let wire = SmtpWire::new(stream)?;
                let stream = begin_starttls(wire)?;
                let tls = self.tls_stream(stream, &grant.host)?;
                let mut wire = SmtpWire::after_starttls(tls);
                let capabilities = wire.ehlo()?;
                submit_authenticated(&mut wire, grant, message, &capabilities, before_data)
            }
        }
    }
}

pub(crate) fn smtp_send_scope(operation_id: &str) -> String {
    format!("{SEND_SCOPE_PREFIX}{operation_id}")
}

pub(crate) fn is_smtp_send_work(work: &ClaimedWork) -> bool {
    work.kind == WorkKind::Send
        && work
            .operation_id
            .as_deref()
            .is_some_and(|operation_id| work.scope == smtp_send_scope(operation_id))
}

pub(crate) struct SmtpAdapter<A, S, C> {
    access: A,
    submission: S,
    clock: C,
    database_path: PathBuf,
}

impl<A, S, C> SmtpAdapter<A, S, C> {
    pub(crate) fn new(access: A, submission: S, clock: C, database_path: &Path) -> Self {
        Self {
            access,
            submission,
            clock,
            database_path: database_path.to_owned(),
        }
    }
}

impl<A, S, C> WorkerAdapter for SmtpAdapter<A, S, C>
where
    A: SmtpAccessSource + Sync,
    S: SmtpSubmission + Sync,
    C: Fn() -> i64 + Sync,
{
    fn execute(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome {
        let prepared = match crate::outgoing::prepare_from_durable_payload(&work.payload_json, &[])
        {
            Ok(prepared)
                if is_smtp_send_work(work)
                    && work.operation_id.is_some()
                    && prepared.provider_kind.as_deref() == Some("imap")
                    && prepared.client_correlation_id.is_some()
                    && work.ordering_key == prepared.message_id
                    && work.scope
                        == smtp_send_scope(work.operation_id.as_deref().expect("validated")) =>
            {
                prepared
            }
            _ => {
                return WorkerOutcome::PermanentFailure {
                    code: "smtp_invalid_send_snapshot".into(),
                }
            }
        };
        if context.is_cancelled() {
            return WorkerOutcome::RejectedBeforeSubmission {
                code: "smtp_send_cancelled_before_submission".into(),
            };
        }
        let grant = match self.access.smtp_access_for_account(&work.account_id) {
            Ok(grant) => grant,
            Err(error) => return access_error_outcome(error),
        };
        if !prepared
            .envelope_from
            .eq_ignore_ascii_case(&grant.remote_account_id)
        {
            return WorkerOutcome::TransportAuthenticationExpired {
                code: "smtp_account_mismatch".into(),
                capability: "outgoing_mail".into(),
            };
        }
        if context.is_cancelled() {
            return WorkerOutcome::RejectedBeforeSubmission {
                code: "smtp_send_cancelled_before_submission".into(),
            };
        }
        let mut before_data = || {
            crate::worker::mark_send_submission_started(&self.database_path, work, (self.clock)())
                .map_err(|_| SmtpError::RetryableBeforeSubmission)
        };
        match self.submission.submit(&grant, &prepared, &mut before_data) {
            Ok(()) => WorkerOutcome::Succeeded {
                projection: WorkerProjection::LocalOperation,
            },
            Err(SmtpError::Authentication) => WorkerOutcome::TransportAuthenticationExpired {
                code: "smtp_authentication_expired".into(),
                capability: "outgoing_mail".into(),
            },
            Err(SmtpError::RetryableBeforeSubmission)
            | Err(SmtpError::ExplicitTransientRejection) => {
                WorkerOutcome::RejectedBeforeSubmission {
                    code: "smtp_submission_rejected_retryable".into(),
                }
            }
            Err(SmtpError::PermanentBeforeSubmission)
            | Err(SmtpError::ExplicitPermanentRejection) => WorkerOutcome::PermanentFailure {
                code: "smtp_submission_rejected".into(),
            },
            Err(SmtpError::AmbiguousSubmission) => WorkerOutcome::OutcomeUnknown {
                code: "smtp_submission_outcome_unknown".into(),
            },
        }
    }
}

fn access_error_outcome(error: SmtpAccessError) -> WorkerOutcome {
    match error {
        SmtpAccessError::CredentialUnavailable => WorkerOutcome::CredentialUnavailable,
        SmtpAccessError::ReauthorizationRequired => WorkerOutcome::AuthenticationExpired {
            code: "smtp_reauthorization_required".into(),
        },
        SmtpAccessError::Retryable => WorkerOutcome::RejectedBeforeSubmission {
            code: "smtp_access_retryable".into(),
        },
        SmtpAccessError::Permanent => WorkerOutcome::PermanentFailure {
            code: "smtp_access_invalid".into(),
        },
    }
}

#[derive(Debug)]
enum WireError {
    Transport,
    Protocol,
}

impl From<WireError> for SmtpError {
    fn from(error: WireError) -> Self {
        match error {
            WireError::Transport => SmtpError::RetryableBeforeSubmission,
            WireError::Protocol => SmtpError::PermanentBeforeSubmission,
        }
    }
}

struct SmtpWire<T> {
    io: BufReader<T>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SmtpResponse {
    code: u16,
    text: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EhloCapabilities {
    values: BTreeSet<String>,
    auth: BTreeSet<String>,
    size: Option<usize>,
}

impl EhloCapabilities {
    fn contains(&self, value: &str) -> bool {
        self.values.contains(&value.to_ascii_uppercase())
    }
}

impl<T: Read + Write> SmtpWire<T> {
    fn new(io: T) -> Result<Self, SmtpError> {
        let mut wire = Self {
            io: BufReader::new(io),
        };
        let greeting = wire.read_response().map_err(SmtpError::from)?;
        match greeting.code {
            220 => Ok(wire),
            400..=499 => Err(SmtpError::RetryableBeforeSubmission),
            _ => Err(SmtpError::PermanentBeforeSubmission),
        }
    }

    fn after_starttls(io: T) -> Self {
        Self {
            io: BufReader::new(io),
        }
    }

    fn into_inner(self) -> T {
        self.io.into_inner()
    }

    fn ehlo(&mut self) -> Result<EhloCapabilities, SmtpError> {
        let response = self.command("EHLO [127.0.0.1]").map_err(SmtpError::from)?;
        if response.code != 250 {
            return Err(classify_pre_submission_response(response.code));
        }
        parse_ehlo_capabilities(&response)
    }

    fn command(&mut self, command: &str) -> Result<SmtpResponse, WireError> {
        self.write_line(command)?;
        self.read_response()
    }

    fn write_line(&mut self, line: &str) -> Result<(), WireError> {
        if line.is_empty()
            || line.len() > MAX_LINE_BYTES
            || line.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
        {
            return Err(WireError::Protocol);
        }
        self.io
            .get_mut()
            .write_all(line.as_bytes())
            .and_then(|()| self.io.get_mut().write_all(b"\r\n"))
            .and_then(|()| self.io.get_mut().flush())
            .map_err(|_| WireError::Transport)
    }

    fn write_data(&mut self, data: &[u8]) -> Result<(), WireError> {
        if data.is_empty() || data.len() > MAX_SMTP_DATA_BYTES || !data.ends_with(b".\r\n") {
            return Err(WireError::Protocol);
        }
        self.io
            .get_mut()
            .write_all(data)
            .and_then(|()| self.io.get_mut().flush())
            .map_err(|_| WireError::Transport)
    }

    fn read_response(&mut self) -> Result<SmtpResponse, WireError> {
        let mut total = 0usize;
        let mut text = Vec::new();
        let mut code = None;
        loop {
            if text.len() >= MAX_RESPONSE_LINES {
                return Err(WireError::Protocol);
            }
            let line = read_bounded_line(&mut self.io)?;
            total = total
                .checked_add(line.len())
                .filter(|value| *value <= MAX_RESPONSE_BYTES)
                .ok_or(WireError::Protocol)?;
            if line.len() < 4
                || !line[..3].iter().all(u8::is_ascii_digit)
                || !matches!(line[3], b' ' | b'-')
                || !line.is_ascii()
            {
                return Err(WireError::Protocol);
            }
            let line_code = std::str::from_utf8(&line[..3])
                .ok()
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|value| (200..=599).contains(value))
                .ok_or(WireError::Protocol)?;
            if code.get_or_insert(line_code) != &line_code {
                return Err(WireError::Protocol);
            }
            let final_line = line[3] == b' ';
            let value = std::str::from_utf8(&line[4..])
                .map_err(|_| WireError::Protocol)?
                .to_owned();
            text.push(value);
            if final_line {
                return Ok(SmtpResponse {
                    code: line_code,
                    text,
                });
            }
        }
    }
}

fn read_bounded_line<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, WireError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|_| WireError::Transport)?;
        if available.is_empty() {
            return Err(WireError::Transport);
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_LINE_BYTES + 2 {
            return Err(WireError::Protocol);
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.ends_with(b"\n") {
            if !line.ends_with(b"\r\n") {
                return Err(WireError::Protocol);
            }
            line.truncate(line.len() - 2);
            return Ok(line);
        }
    }
}

fn parse_ehlo_capabilities(response: &SmtpResponse) -> Result<EhloCapabilities, SmtpError> {
    if response.code != 250 || response.text.is_empty() {
        return Err(SmtpError::PermanentBeforeSubmission);
    }
    let mut values = BTreeSet::new();
    let mut auth = BTreeSet::new();
    let mut size = None;
    for line in response.text.iter().skip(1) {
        if line.is_empty() || line.len() > MAX_CAPABILITY_BYTES || !line.is_ascii() {
            return Err(SmtpError::PermanentBeforeSubmission);
        }
        let mut parts = line.split_ascii_whitespace();
        let name = parts
            .next()
            .ok_or(SmtpError::PermanentBeforeSubmission)?
            .to_ascii_uppercase();
        if name.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(SmtpError::PermanentBeforeSubmission);
        }
        if let Some(mechanism) = name.strip_prefix("AUTH=") {
            auth.insert(mechanism.to_owned());
            values.insert("AUTH".into());
        } else {
            values.insert(name.clone());
        }
        if name == "AUTH" {
            for mechanism in parts {
                auth.insert(mechanism.to_ascii_uppercase());
            }
        } else if name == "SIZE" {
            if let Some(value) = parts.next() {
                let parsed = value
                    .parse::<u64>()
                    .ok()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(SmtpError::PermanentBeforeSubmission)?;
                size = Some(parsed);
            }
        }
        if values.len() > MAX_CAPABILITIES || auth.len() > MAX_CAPABILITIES {
            return Err(SmtpError::PermanentBeforeSubmission);
        }
    }
    Ok(EhloCapabilities { values, auth, size })
}

fn begin_starttls<T: Read + Write>(mut wire: SmtpWire<T>) -> Result<T, SmtpError> {
    let capabilities = wire.ehlo()?;
    if !capabilities.contains("STARTTLS") {
        return Err(SmtpError::PermanentBeforeSubmission);
    }
    let response = wire.command("STARTTLS").map_err(SmtpError::from)?;
    match response.code {
        220 => Ok(wire.into_inner()),
        code => Err(classify_pre_submission_response(code)),
    }
}

fn submit_authenticated<T: Read + Write>(
    wire: &mut SmtpWire<T>,
    grant: &SmtpAccessGrant,
    message: &PreparedOutgoingMessage,
    capabilities: &EhloCapabilities,
    before_data: &mut dyn FnMut() -> Result<(), SmtpError>,
) -> Result<(), SmtpError> {
    authenticate(wire, grant, capabilities)?;
    validate_envelope_address(&message.envelope_from)?;
    if message.envelope_recipients.is_empty() {
        return Err(SmtpError::PermanentBeforeSubmission);
    }
    for recipient in &message.envelope_recipients {
        validate_envelope_address(recipient)?;
    }
    if message.requires_smtp_utf8 && !capabilities.contains("SMTPUTF8") {
        return Err(SmtpError::PermanentBeforeSubmission);
    }
    let canonical = canonicalize_crlf(&message.raw_message)?;
    if capabilities
        .size
        .is_some_and(|maximum| canonical.len() > maximum)
    {
        return Err(SmtpError::ExplicitPermanentRejection);
    }
    let mut mail = format!("MAIL FROM:<{}>", message.envelope_from);
    if capabilities.contains("SIZE") {
        mail.push_str(&format!(" SIZE={}", canonical.len()));
    }
    if message.requires_smtp_utf8 {
        mail.push_str(" SMTPUTF8");
    }
    expect_envelope_response(wire.command(&mail).map_err(SmtpError::from)?)?;
    for recipient in &message.envelope_recipients {
        let response = wire
            .command(&format!("RCPT TO:<{recipient}>"))
            .map_err(SmtpError::from)?;
        if !matches!(response.code, 250..=252) {
            let _ = wire.command("RSET");
            return Err(classify_pre_submission_response(response.code));
        }
    }

    before_data()?;
    let data_response = wire
        .command("DATA")
        .map_err(|_| SmtpError::AmbiguousSubmission)?;
    if data_response.code != 354 {
        return Err(classify_explicit_rejection(data_response.code));
    }
    let body = dot_stuff(&canonical)?;
    wire.write_data(&body)
        .map_err(|_| SmtpError::AmbiguousSubmission)?;
    let accepted = wire
        .read_response()
        .map_err(|_| SmtpError::AmbiguousSubmission)?;
    match accepted.code {
        250 => {
            let _ = wire.command("QUIT");
            Ok(())
        }
        code => Err(classify_explicit_rejection(code)),
    }
}

fn verify_authenticated_session<T: Read + Write>(
    wire: &mut SmtpWire<T>,
    grant: &SmtpAccessGrant,
) -> Result<(), SmtpError> {
    let capabilities = wire.ehlo()?;
    authenticate(wire, grant, &capabilities)?;
    let _ = wire.command("QUIT");
    Ok(())
}

fn authenticate<T: Read + Write>(
    wire: &mut SmtpWire<T>,
    grant: &SmtpAccessGrant,
    capabilities: &EhloCapabilities,
) -> Result<(), SmtpError> {
    if capabilities.auth.contains("PLAIN") {
        let payload = Zeroizing::new(format!("\0{}\0{}", grant.username, grant.password.as_str()));
        let encoded = Zeroizing::new(STANDARD.encode(payload.as_bytes()));
        let command = Zeroizing::new(format!("AUTH PLAIN {}", encoded.as_str()));
        let response = wire.command(&command).map_err(SmtpError::from)?;
        return classify_auth_response(response.code);
    }
    if capabilities.auth.contains("LOGIN") {
        let response = wire.command("AUTH LOGIN").map_err(SmtpError::from)?;
        if response.code != 334 {
            return classify_auth_response(response.code);
        }
        let username = Zeroizing::new(STANDARD.encode(grant.username.as_bytes()));
        let response = wire.command(&username).map_err(SmtpError::from)?;
        if response.code != 334 {
            return classify_auth_response(response.code);
        }
        let password = Zeroizing::new(STANDARD.encode(grant.password.as_bytes()));
        let response = wire.command(&password).map_err(SmtpError::from)?;
        return classify_auth_response(response.code);
    }
    Err(SmtpError::PermanentBeforeSubmission)
}

fn classify_auth_response(code: u16) -> Result<(), SmtpError> {
    match code {
        235 => Ok(()),
        432 | 454 => Err(SmtpError::RetryableBeforeSubmission),
        534 | 535 | 538 => Err(SmtpError::Authentication),
        400..=499 => Err(SmtpError::RetryableBeforeSubmission),
        _ => Err(SmtpError::PermanentBeforeSubmission),
    }
}

fn expect_envelope_response(response: SmtpResponse) -> Result<(), SmtpError> {
    if response.code == 250 {
        Ok(())
    } else {
        Err(classify_pre_submission_response(response.code))
    }
}

fn classify_pre_submission_response(code: u16) -> SmtpError {
    match code {
        400..=499 => SmtpError::RetryableBeforeSubmission,
        _ => SmtpError::ExplicitPermanentRejection,
    }
}

fn classify_explicit_rejection(code: u16) -> SmtpError {
    match code {
        400..=499 => SmtpError::ExplicitTransientRejection,
        500..=599 => SmtpError::ExplicitPermanentRejection,
        _ => SmtpError::AmbiguousSubmission,
    }
}

fn validate_envelope_address(value: &str) -> Result<(), SmtpError> {
    if value.is_empty()
        || value.len() > MAX_ENVELOPE_BYTES
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ' || matches!(byte, b'<' | b'>'))
    {
        return Err(SmtpError::PermanentBeforeSubmission);
    }
    Ok(())
}

fn canonicalize_crlf(raw: &[u8]) -> Result<Vec<u8>, SmtpError> {
    if raw.is_empty() || raw.len() > crate::mime_ingest::MAX_RAW_MESSAGE_BYTES {
        return Err(SmtpError::PermanentBeforeSubmission);
    }
    let mut output = Vec::with_capacity(raw.len().saturating_add(2));
    let mut index = 0usize;
    while index < raw.len() {
        match raw[index] {
            b'\r' if raw.get(index + 1) == Some(&b'\n') => {
                output.extend_from_slice(b"\r\n");
                index += 2;
            }
            b'\r' | b'\n' => {
                output.extend_from_slice(b"\r\n");
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
        if output.len() > MAX_SMTP_DATA_BYTES {
            return Err(SmtpError::PermanentBeforeSubmission);
        }
    }
    if !output.ends_with(b"\r\n") {
        output.extend_from_slice(b"\r\n");
    }
    Ok(output)
}

fn dot_stuff(canonical: &[u8]) -> Result<Vec<u8>, SmtpError> {
    if canonical.is_empty() || !canonical.ends_with(b"\r\n") {
        return Err(SmtpError::PermanentBeforeSubmission);
    }
    let mut output = Vec::with_capacity(canonical.len().saturating_add(3));
    let mut line_start = true;
    for byte in canonical {
        if line_start && *byte == b'.' {
            output.push(b'.');
        }
        output.push(*byte);
        line_start = output.ends_with(b"\r\n");
        if output.len() > MAX_SMTP_DATA_BYTES.saturating_sub(3) {
            return Err(SmtpError::PermanentBeforeSubmission);
        }
    }
    output.extend_from_slice(b".\r\n");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_conformance::apply_worker_projection;
    use crate::store::MuxStore;
    use crate::worker::{DurableWorker, WorkerConfig};
    use std::io::Cursor;
    use tempfile::tempdir;

    struct Transcript {
        read: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Transcript {
        fn new(value: impl Into<Vec<u8>>) -> Self {
            Self {
                read: Cursor::new(value.into()),
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

    fn grant(mode: SmtpTlsMode) -> SmtpAccessGrant {
        SmtpAccessGrant {
            host: "smtp.example.test".into(),
            port: if mode == SmtpTlsMode::Implicit {
                465
            } else {
                587
            },
            tls_mode: mode,
            username: "reader@example.test".into(),
            password: Zeroizing::new("secret".into()),
            remote_account_id: "reader@example.test".into(),
        }
    }

    fn message() -> PreparedOutgoingMessage {
        PreparedOutgoingMessage {
            envelope_from: "reader@example.test".into(),
            envelope_recipients: vec!["one@example.test".into(), "two@example.test".into()],
            raw_message: b"From: reader@example.test\nTo: one@example.test\n\n.first\nlast"
                .to_vec(),
            raw_sha256_hex: "00".repeat(32),
            message_id: "<message@mux.invalid>".into(),
            client_correlation_id: Some("mux-correlation".into()),
            provider_kind: Some("imap".into()),
            remote_thread_id: None,
            queued_at_ms: 1,
            requires_smtp_utf8: false,
        }
    }

    #[test]
    fn smtp_implicit_transcript_authenticates_envelopes_dot_stuffs_and_fences_before_data() {
        let transcript = Transcript::new(
            b"220 ready\r\n250-smtp.example.test\r\n250-SIZE 100000\r\n250 AUTH PLAIN LOGIN\r\n235 authenticated\r\n250 sender ok\r\n251 first forwarded\r\n252 second accepted\r\n354 send data\r\n250 accepted\r\n221 bye\r\n"
                .to_vec(),
        );
        let mut wire = SmtpWire::new(transcript).expect("greeting");
        let capabilities = wire.ehlo().expect("EHLO");
        let mut fenced = false;
        submit_authenticated(
            &mut wire,
            &grant(SmtpTlsMode::Implicit),
            &message(),
            &capabilities,
            &mut || {
                fenced = true;
                Ok(())
            },
        )
        .expect("accepted submission");
        assert!(fenced);
        let written = String::from_utf8(wire.into_inner().written).expect("ASCII transcript");
        assert!(written.starts_with("EHLO [127.0.0.1]\r\nAUTH PLAIN "));
        assert!(written.contains("MAIL FROM:<reader@example.test> SIZE="));
        assert!(written.contains("RCPT TO:<one@example.test>\r\n"));
        assert!(written.contains("RCPT TO:<two@example.test>\r\nDATA\r\n"));
        assert!(written.contains("\r\n..first\r\nlast\r\n.\r\nQUIT\r\n"));
    }

    #[test]
    fn smtp_partial_recipient_failure_resets_and_never_crosses_data_fence() {
        let transcript = Transcript::new(
            b"220 ready\r\n250-smtp.example.test\r\n250 AUTH LOGIN\r\n334 username\r\n334 password\r\n235 authenticated\r\n250 sender ok\r\n250 first ok\r\n550 second rejected\r\n250 reset\r\n"
                .to_vec(),
        );
        let mut wire = SmtpWire::new(transcript).expect("greeting");
        let capabilities = wire.ehlo().expect("EHLO");
        let mut fenced = false;
        assert_eq!(
            submit_authenticated(
                &mut wire,
                &grant(SmtpTlsMode::Implicit),
                &message(),
                &capabilities,
                &mut || {
                    fenced = true;
                    Ok(())
                },
            ),
            Err(SmtpError::ExplicitPermanentRejection)
        );
        assert!(!fenced);
        let written = String::from_utf8(wire.into_inner().written).expect("ASCII transcript");
        assert!(written.ends_with("RCPT TO:<two@example.test>\r\nRSET\r\n"));
        assert!(!written.contains("DATA\r\n"));
    }

    #[test]
    fn smtp_disconnect_after_data_is_ambiguous_and_never_retryable() {
        let transcript = Transcript::new(
            b"220 ready\r\n250-smtp.example.test\r\n250 AUTH PLAIN\r\n235 authenticated\r\n250 sender ok\r\n250 first ok\r\n250 second ok\r\n354 send data\r\n"
                .to_vec(),
        );
        let mut wire = SmtpWire::new(transcript).expect("greeting");
        let capabilities = wire.ehlo().expect("EHLO");
        let mut fenced = false;
        assert_eq!(
            submit_authenticated(
                &mut wire,
                &grant(SmtpTlsMode::Implicit),
                &message(),
                &capabilities,
                &mut || {
                    fenced = true;
                    Ok(())
                },
            ),
            Err(SmtpError::AmbiguousSubmission)
        );
        assert!(fenced);
    }

    #[test]
    fn smtp_explicit_transient_rejection_after_data_is_safe_to_retry() {
        let transcript = Transcript::new(
            b"220 ready\r\n250-smtp.example.test\r\n250 AUTH PLAIN\r\n235 authenticated\r\n250 sender ok\r\n250 first ok\r\n250 second ok\r\n354 send data\r\n451 temporarily unavailable\r\n"
                .to_vec(),
        );
        let mut wire = SmtpWire::new(transcript).expect("greeting");
        let capabilities = wire.ehlo().expect("EHLO");
        let mut fenced = false;
        assert_eq!(
            submit_authenticated(
                &mut wire,
                &grant(SmtpTlsMode::Implicit),
                &message(),
                &capabilities,
                &mut || {
                    fenced = true;
                    Ok(())
                },
            ),
            Err(SmtpError::ExplicitTransientRejection)
        );
        assert!(fenced);
    }

    #[test]
    fn smtp_response_parser_rejects_lf_only_mixed_codes_and_oversized_lines() {
        let mut lf_only = SmtpWire::after_starttls(Transcript::new(b"220 ready\n".to_vec()));
        assert!(matches!(lf_only.read_response(), Err(WireError::Protocol)));

        let mut mixed = SmtpWire::after_starttls(Transcript::new(
            b"250-first line\r\n550 second line\r\n".to_vec(),
        ));
        assert!(matches!(mixed.read_response(), Err(WireError::Protocol)));

        let oversized = format!("220 {}\r\n", "x".repeat(MAX_LINE_BYTES + 1));
        let mut oversized = SmtpWire::after_starttls(Transcript::new(oversized.into_bytes()));
        assert!(matches!(
            oversized.read_response(),
            Err(WireError::Protocol)
        ));
    }

    #[test]
    fn smtp_starttls_requires_capability_and_reissues_ehlo_after_upgrade() {
        let transcript = Transcript::new(
            b"220 ready\r\n250-smtp.example.test\r\n250 STARTTLS\r\n220 begin tls\r\n".to_vec(),
        );
        let wire = SmtpWire::new(transcript).expect("greeting");
        let upgraded = begin_starttls(wire).expect("STARTTLS accepted");
        let written = String::from_utf8(upgraded.written).expect("ASCII transcript");
        assert_eq!(written, "EHLO [127.0.0.1]\r\nSTARTTLS\r\n");

        let missing =
            Transcript::new(b"220 ready\r\n250-smtp.example.test\r\n250 SIZE 100\r\n".to_vec());
        let wire = SmtpWire::new(missing).expect("greeting");
        assert!(matches!(
            begin_starttls(wire),
            Err(SmtpError::PermanentBeforeSubmission)
        ));
    }

    #[test]
    fn smtp_authority_verification_authenticates_without_starting_an_envelope() {
        let transcript = Transcript::new(
            b"250-smtp.example.test\r\n250 AUTH PLAIN\r\n235 authenticated\r\n221 bye\r\n".to_vec(),
        );
        let mut wire = SmtpWire::after_starttls(transcript);
        verify_authenticated_session(&mut wire, &grant(SmtpTlsMode::StartTls))
            .expect("SMTP authority verified");
        let written = String::from_utf8(wire.into_inner().written).expect("ASCII transcript");
        assert!(written.starts_with("EHLO [127.0.0.1]\r\nAUTH PLAIN "));
        assert!(written.ends_with("\r\nQUIT\r\n"));
        assert!(!written.contains("MAIL FROM"));
        assert!(!written.contains("RCPT TO"));
        assert!(!written.contains("DATA\r\n"));
    }

    #[test]
    fn smtp_size_and_smtputf8_requirements_fail_before_mail_or_data() {
        let response = SmtpResponse {
            code: 250,
            text: vec![
                "smtp.example.test".into(),
                "SIZE 2".into(),
                "AUTH PLAIN".into(),
            ],
        };
        let capabilities = parse_ehlo_capabilities(&response).expect("capabilities");
        let transcript = Transcript::new(b"235 authenticated\r\n".to_vec());
        let mut wire = SmtpWire::after_starttls(transcript);
        let mut fenced = false;
        assert_eq!(
            submit_authenticated(
                &mut wire,
                &grant(SmtpTlsMode::Implicit),
                &message(),
                &capabilities,
                &mut || {
                    fenced = true;
                    Ok(())
                },
            ),
            Err(SmtpError::ExplicitPermanentRejection)
        );
        assert!(!fenced);
        let written = String::from_utf8(wire.into_inner().written).expect("ASCII transcript");
        assert!(written.starts_with("AUTH PLAIN "));
        assert!(!written.contains("MAIL FROM"));
    }

    struct FixtureAccess;

    impl SmtpAccessSource for FixtureAccess {
        fn smtp_access_for_account(
            &self,
            account_id: &str,
        ) -> Result<SmtpAccessGrant, SmtpAccessError> {
            if account_id != "imap-account" {
                return Err(SmtpAccessError::ReauthorizationRequired);
            }
            Ok(grant(SmtpTlsMode::Implicit))
        }
    }

    struct AcceptedSubmission;

    impl SmtpSubmission for AcceptedSubmission {
        fn submit(
            &self,
            _grant: &SmtpAccessGrant,
            _message: &PreparedOutgoingMessage,
            before_data: &mut dyn FnMut() -> Result<(), SmtpError>,
        ) -> Result<(), SmtpError> {
            before_data()?;
            Ok(())
        }
    }

    struct AmbiguousSubmission;

    impl SmtpSubmission for AmbiguousSubmission {
        fn submit(
            &self,
            _grant: &SmtpAccessGrant,
            _message: &PreparedOutgoingMessage,
            before_data: &mut dyn FnMut() -> Result<(), SmtpError>,
        ) -> Result<(), SmtpError> {
            before_data()?;
            Err(SmtpError::AmbiguousSubmission)
        }
    }

    struct RefusedAuthentication;

    impl SmtpSubmission for RefusedAuthentication {
        fn submit(
            &self,
            _grant: &SmtpAccessGrant,
            _message: &PreparedOutgoingMessage,
            _before_data: &mut dyn FnMut() -> Result<(), SmtpError>,
        ) -> Result<(), SmtpError> {
            Err(SmtpError::Authentication)
        }
    }

    fn smtp_store_fixture(with_capability: bool) -> (tempfile::TempDir, PathBuf, String) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("smtp-worker.db");
        drop(MuxStore::open(&path, false).expect("schema"));
        let connection = rusqlite::Connection::open(&path).expect("fixture connection");
        connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('imap-account', 'IMAP', 'reader@example.test', '#000000', 'imap');
                 INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(
                   'imap-account', 'imap', 'reader@example.test', 'ready',
                   'opaque-keychain-reference', 'idle', 0, 0
                 );
                 INSERT INTO drafts(
                   id, account_id, recipients, cc_recipients, bcc_recipients,
                   subject, body, body_html, updated_at, revision
                 ) VALUES(
                   'smtp-draft', 'imap-account', 'one@example.test', '', '',
                   'SMTP fixture', 'body', '', 1, 1
                 );",
            )
            .expect("account fixture");
        if with_capability {
            connection
                .execute(
                    "INSERT INTO provider_capabilities(account_id, capability, enabled)
                     VALUES('imap-account', 'outgoing_mail', 1)",
                    [],
                )
                .expect("SMTP capability");
        }
        (directory, path, "smtp-draft".into())
    }

    #[test]
    fn smtp_queue_requires_explicit_capability_before_journaling_any_send() {
        let (_directory, path, draft_id) = smtp_store_fixture(false);
        let mut store = MuxStore::open(&path, false).expect("store");
        assert!(matches!(
            store.queue_send(&draft_id, 1_000),
            Err(crate::store::StoreError::Conflict(message))
                if message.contains("no SMTP transport")
        ));
        assert_eq!(
            rusqlite::Connection::open(&path)
                .expect("verification connection")
                .query_row("SELECT COUNT(*) FROM operations", [], |row| row
                    .get::<_, i64>(0))
                .expect("operation count"),
            0
        );
    }

    #[test]
    fn smtp_worker_fences_then_confirms_the_exact_local_send_projection() {
        let (_directory, path, draft_id) = smtp_store_fixture(true);
        let mut store = MuxStore::open(&path, false).expect("store");
        store.queue_send(&draft_id, 1_000).expect("queue SMTP send");
        drop(store);
        let connection = rusqlite::Connection::open(&path).expect("fixture connection");
        let (available_at, operation_id, work_scope): (i64, String, String) = connection
            .query_row(
                "SELECT available_at, operation_id, scope FROM provider_work_items
                 WHERE kind = 'send'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("send work");
        assert_eq!(work_scope, smtp_send_scope(&operation_id));
        drop(connection);

        let adapter = SmtpAdapter::new(FixtureAccess, AcceptedSubmission, || available_at, &path);
        let worker = DurableWorker::new(&path, WorkerConfig::default()).expect("worker");
        let result = worker
            .run_cycle(
                "smtp-fixture-worker",
                &adapter,
                &apply_worker_projection,
                &|| available_at,
            )
            .expect("worker cycle");
        assert_eq!(result.succeeded, 1);
        assert!(
            result.errors.is_empty(),
            "worker errors: {:?}",
            result.errors
        );
        let verify = rusqlite::Connection::open(&path).expect("verification");
        assert_eq!(
            verify
                .query_row(
                    "SELECT state FROM operations WHERE id = ?1",
                    [&operation_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("operation state"),
            "confirmed"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE is_from_me = 1 AND remote_deleted = 0",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("local sent message"),
            1
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM drafts WHERE id = ?1",
                    [&draft_id],
                    |row| row.get::<_, i64>(0),
                )
                .expect("draft count"),
            0
        );
    }

    #[test]
    fn smtp_ambiguous_submission_is_terminal_and_never_projects_or_retries() {
        let (_directory, path, draft_id) = smtp_store_fixture(true);
        let mut store = MuxStore::open(&path, false).expect("store");
        store.queue_send(&draft_id, 1_000).expect("queue SMTP send");
        drop(store);
        let fixture = rusqlite::Connection::open(&path).expect("fixture connection");
        let (available_at, operation_id): (i64, String) = fixture
            .query_row(
                "SELECT available_at, operation_id FROM provider_work_items WHERE kind = 'send'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("send work");
        drop(fixture);

        let adapter = SmtpAdapter::new(FixtureAccess, AmbiguousSubmission, || available_at, &path);
        let worker = DurableWorker::new(&path, WorkerConfig::default()).expect("worker");
        let result = worker
            .run_cycle(
                "smtp-ambiguous-worker",
                &adapter,
                &apply_worker_projection,
                &|| available_at,
            )
            .expect("worker cycle");
        assert_eq!(result.outcome_unknown, 1);
        assert_eq!(result.retry_scheduled, 0);
        assert_eq!(result.succeeded, 0);
        assert!(result.errors.is_empty());

        let verify = rusqlite::Connection::open(&path).expect("verification connection");
        let states: (String, String, i64, i64) = verify
            .query_row(
                "SELECT operation.state, work.state,
                        (SELECT COUNT(*) FROM messages WHERE is_from_me = 1),
                        (SELECT COUNT(*) FROM drafts WHERE id = ?2)
                 FROM operations operation
                 JOIN provider_work_items work ON work.operation_id = operation.id
                 WHERE operation.id = ?1",
                rusqlite::params![operation_id, draft_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("uncertain send state");
        assert_eq!(
            states,
            ("outcome_unknown".into(), "outcome_unknown".into(), 0, 1)
        );
    }

    #[test]
    fn smtp_authentication_failure_disables_only_outgoing_mail() {
        let (_directory, path, draft_id) = smtp_store_fixture(true);
        let mut store = MuxStore::open(&path, false).expect("store");
        store.queue_send(&draft_id, 1_000).expect("queue SMTP send");
        drop(store);
        let fixture = rusqlite::Connection::open(&path).expect("fixture connection");
        let available_at: i64 = fixture
            .query_row(
                "SELECT available_at FROM provider_work_items WHERE kind = 'send'",
                [],
                |row| row.get(0),
            )
            .expect("send work");
        drop(fixture);

        let adapter =
            SmtpAdapter::new(FixtureAccess, RefusedAuthentication, || available_at, &path);
        let worker = DurableWorker::new(&path, WorkerConfig::default()).expect("worker");
        let result = worker
            .run_cycle(
                "smtp-auth-worker",
                &adapter,
                &apply_worker_projection,
                &|| available_at,
            )
            .expect("worker cycle");
        assert_eq!(result.failed, 1);
        assert_eq!(result.authentication_blocked, 0);

        let verify = rusqlite::Connection::open(&path).expect("verification connection");
        let state: (String, String, i64) = verify
            .query_row(
                "SELECT provider.auth_state, provider.sync_state, capability.enabled
                 FROM provider_accounts provider
                 JOIN provider_capabilities capability
                   ON capability.account_id = provider.account_id
                  AND capability.capability = 'outgoing_mail'
                 WHERE provider.account_id = 'imap-account'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("transport state");
        assert_eq!(state, ("ready".into(), "idle".into(), 0));
    }
}
