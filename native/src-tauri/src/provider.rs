//! Provider-neutral identities and durable synchronization vocabulary.
//!
//! This module deliberately contains no provider payloads, credentials, raw MIME,
//! or rendered HTML. Adapters translate their protocol-specific responses into
//! these bounded types before the storage layer sees them.

use std::{collections::BTreeSet, fmt, marker::PhantomData};

use serde::{
    de::{Error as _, IgnoredAny, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use thiserror::Error;

const MAX_LOCAL_ACCOUNT_ID_BYTES: usize = 256;
const MAX_REMOTE_ID_BYTES: usize = 2 * 1024;
const MAX_CURSOR_BYTES: usize = 16 * 1024;
const MAX_CONTAINER_NAME_BYTES: usize = 512;
const MAX_BATCH_ID_BYTES: usize = 256;
const MAX_SUBJECT_BYTES: usize = 2_000;
const MAX_PARTICIPANTS_BYTES: usize = 16_000;
const MAX_SNIPPET_BYTES: usize = 2_000;
const MAX_CATEGORY_BYTES: usize = 200;
const MAX_REVISION_BYTES: usize = 2_000;
const MAX_SENDER_NAME_BYTES: usize = 2_000;
const MAX_EMAIL_BYTES: usize = 2_048;
const MAX_RECIPIENTS_BYTES: usize = 16_000;
const MAX_PLAIN_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_KEYWORD_BYTES: usize = 256;
const MAX_MESSAGE_KEYWORDS: usize = 256;
const MAX_THREAD_MESSAGE_COUNT: u32 = 1_000_000;
const MAX_BATCH_COLLECTION_ITEMS: usize = 1_000;
const MAX_BATCH_TOTAL_ITEMS: usize = 4_000;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderContractError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} exceeds the {max_bytes}-byte limit")]
    TooLong {
        field: &'static str,
        max_bytes: usize,
    },
    #[error("{field} contains control characters")]
    ControlCharacter { field: &'static str },
    #[error("{relation} must belong to one Mux account")]
    AccountMismatch { relation: &'static str },
    #[error("Unix timestamp must not be negative")]
    NegativeTimestamp,
    #[error("{field} is not normalized")]
    NotNormalized { field: &'static str },
    #[error("{field} must be between {min} and {max}")]
    NumericOutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
    },
    #[error("{collection} exceeds the {max_items}-item limit")]
    CollectionTooLarge {
        collection: &'static str,
        max_items: usize,
    },
    #[error("provider batch exceeds the {max_items}-item total limit")]
    BatchTooLarge { max_items: usize },
    #[error("{collection} contains a duplicate remote identity")]
    DuplicateIdentity { collection: &'static str },
    #[error("provider batch contains contradictory changes for one {entity}")]
    ContradictoryChange { entity: &'static str },
}

fn validate_bounded_string(
    value: &str,
    field: &'static str,
    max_bytes: usize,
) -> Result<(), ProviderContractError> {
    if value.is_empty() {
        return Err(ProviderContractError::Empty { field });
    }
    if value.len() > max_bytes {
        return Err(ProviderContractError::TooLong { field, max_bytes });
    }
    if value.chars().any(char::is_control) {
        return Err(ProviderContractError::ControlCharacter { field });
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    field: &'static str,
    max_bytes: usize,
    multiline: bool,
) -> Result<(), ProviderContractError> {
    if value.len() > max_bytes {
        return Err(ProviderContractError::TooLong { field, max_bytes });
    }
    if multiline {
        if value.contains('\r') {
            return Err(ProviderContractError::NotNormalized { field });
        }
        if value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        {
            return Err(ProviderContractError::ControlCharacter { field });
        }
    } else if value.chars().any(char::is_control) {
        return Err(ProviderContractError::ControlCharacter { field });
    }
    Ok(())
}

fn deserialize_batch_collection<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedCollectionVisitor<T>(PhantomData<T>);

    impl<'de, T> Visitor<'de> for BoundedCollectionVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                formatter,
                "a provider batch collection with at most {MAX_BATCH_COLLECTION_ITEMS} items"
            )
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            if sequence
                .size_hint()
                .is_some_and(|size| size > MAX_BATCH_COLLECTION_ITEMS)
            {
                return Err(A::Error::custom(format_args!(
                    "provider batch collection exceeds the {MAX_BATCH_COLLECTION_ITEMS}-item limit"
                )));
            }

            let mut values = Vec::with_capacity(
                sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_BATCH_COLLECTION_ITEMS),
            );
            while values.len() < MAX_BATCH_COLLECTION_ITEMS {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
            }
            if sequence.next_element::<IgnoredAny>()?.is_some() {
                return Err(A::Error::custom(format_args!(
                    "provider batch collection exceeds the {MAX_BATCH_COLLECTION_ITEMS}-item limit"
                )));
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedCollectionVisitor(PhantomData))
}

macro_rules! bounded_string_type {
    ($name:ident, $field:literal, $max:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderContractError> {
                let value = value.into();
                validate_bounded_string(&value, $field, $max)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProviderContractError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(D::Error::custom)
            }
        }
    };
}

macro_rules! bounded_text_type {
    ($name:ident, $field:literal, $max:expr, $multiline:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderContractError> {
                let value = value.into();
                validate_bounded_text(&value, $field, $max, $multiline)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProviderContractError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(D::Error::custom)
            }
        }
    };
}

bounded_string_type!(MuxAccountId, "Mux account ID", MAX_LOCAL_ACCOUNT_ID_BYTES);
bounded_string_type!(RemoteAccountId, "remote account ID", MAX_REMOTE_ID_BYTES);
bounded_string_type!(RemoteThreadId, "remote thread ID", MAX_REMOTE_ID_BYTES);
bounded_string_type!(RemoteMessageId, "remote message ID", MAX_REMOTE_ID_BYTES);
bounded_string_type!(
    RemoteContainerId,
    "remote container ID",
    MAX_REMOTE_ID_BYTES
);
bounded_string_type!(OpaqueSyncCursor, "sync cursor", MAX_CURSOR_BYTES);
bounded_string_type!(ProviderBatchId, "provider batch ID", MAX_BATCH_ID_BYTES);
bounded_string_type!(ProviderRevision, "provider revision", MAX_REVISION_BYTES);
bounded_string_type!(
    ContainerDisplayName,
    "container display name",
    MAX_CONTAINER_NAME_BYTES
);
bounded_text_type!(
    ProviderSubject,
    "provider subject",
    MAX_SUBJECT_BYTES,
    false
);
bounded_text_type!(
    ProviderParticipants,
    "provider participants",
    MAX_PARTICIPANTS_BYTES,
    false
);
bounded_text_type!(
    ProviderSnippet,
    "provider snippet",
    MAX_SNIPPET_BYTES,
    false
);
bounded_text_type!(
    ProviderCategory,
    "provider category",
    MAX_CATEGORY_BYTES,
    false
);
bounded_text_type!(
    ProviderSenderName,
    "provider sender name",
    MAX_SENDER_NAME_BYTES,
    false
);
bounded_text_type!(ProviderEmail, "provider email", MAX_EMAIL_BYTES, false);
bounded_text_type!(
    ProviderRecipients,
    "provider recipients",
    MAX_RECIPIENTS_BYTES,
    false
);
bounded_text_type!(
    NormalizedPlainBody,
    "normalized plain body",
    MAX_PLAIN_BODY_BYTES,
    true
);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderMessageKeyword(String);

impl ProviderMessageKeyword {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderContractError> {
        let value = value.into();
        validate_bounded_string(&value, "provider message keyword", MAX_KEYWORD_BYTES)?;
        if !value.bytes().all(|byte| byte.is_ascii_graphic())
            || value.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Err(ProviderContractError::NotNormalized {
                field: "provider message keyword",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for ProviderMessageKeyword {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderThreadMessageCount(u32);

impl ProviderThreadMessageCount {
    pub fn new(value: u32) -> Result<Self, ProviderContractError> {
        if value > MAX_THREAD_MESSAGE_COUNT {
            return Err(ProviderContractError::NumericOutOfRange {
                field: "provider thread message count",
                min: 0,
                max: u64::from(MAX_THREAD_MESSAGE_COUNT),
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ProviderThreadMessageCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Gmail,
    Imap,
    Jmap,
    Pop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderCapability {
    DeltaSync,
    RemoteThreads,
    MultiContainerMembership,
    RemoteDrafts,
    Mutations,
    OutgoingMail,
    AttachmentFetch,
    ServerSearch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCapabilities {
    pub supported: BTreeSet<ProviderCapability>,
}

impl ProviderCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = ProviderCapability>) -> Self {
        Self {
            supported: capabilities.into_iter().collect(),
        }
    }

    pub fn contains(&self, capability: ProviderCapability) -> bool {
        self.supported.contains(&capability)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderAuthStatus {
    SignedOut,
    CredentialLocked,
    Ready,
    ReauthorizationRequired,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderSyncStatus {
    NeverSynced,
    Idle,
    Scheduled,
    Syncing,
    Backoff,
    AuthenticationBlocked,
    Offline,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderAccountIdentity {
    pub mux_account_id: MuxAccountId,
    pub provider_kind: ProviderKind,
    pub remote_account_id: RemoteAccountId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderAccountState {
    pub identity: ProviderAccountIdentity,
    pub capabilities: ProviderCapabilities,
    pub auth_status: ProviderAuthStatus,
    pub sync_status: ProviderSyncStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContainerKind {
    Label,
    Folder,
    Mailbox,
    RetrievalState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContainerRole {
    Inbox,
    Archive,
    AllMail,
    Drafts,
    Sent,
    Trash,
    Spam,
    Starred,
    Important,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteContainerIdentity {
    pub mux_account_id: MuxAccountId,
    pub remote_container_id: RemoteContainerId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderContainer {
    pub identity: RemoteContainerIdentity,
    pub display_name: ContainerDisplayName,
    pub kind: ContainerKind,
    pub role: Option<ContainerRole>,
    pub parent_remote_container_id: Option<RemoteContainerId>,
    pub selectable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteThreadIdentity {
    pub mux_account_id: MuxAccountId,
    pub remote_thread_id: RemoteThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteMessageIdentity {
    pub mux_account_id: MuxAccountId,
    pub remote_message_id: RemoteMessageId,
    pub remote_thread_id: Option<RemoteThreadId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerMembership {
    pub message: RemoteMessageIdentity,
    pub container: RemoteContainerIdentity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContainerMembershipWire {
    message: RemoteMessageIdentity,
    container: RemoteContainerIdentity,
}

impl ContainerMembership {
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.message.mux_account_id != self.container.mux_account_id {
            return Err(ProviderContractError::AccountMismatch {
                relation: "container membership",
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ContainerMembership {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContainerMembershipWire::deserialize(deserializer)?;
        let membership = Self {
            message: wire.message,
            container: wire.container,
        };
        membership.validate().map_err(D::Error::custom)?;
        Ok(membership)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SyncCursorScope {
    Account,
    Container {
        remote_container_id: RemoteContainerId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderSyncCursor {
    pub mux_account_id: MuxAccountId,
    pub scope: SyncCursorScope,
    pub value: OpaqueSyncCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct UnixMillis(i64);

impl UnixMillis {
    pub fn new(value: i64) -> Result<Self, ProviderContractError> {
        if value < 0 {
            return Err(ProviderContractError::NegativeTimestamp);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for UnixMillis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = i64::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TombstoneTarget {
    Thread {
        identity: RemoteThreadIdentity,
    },
    Message {
        identity: RemoteMessageIdentity,
    },
    Container {
        identity: RemoteContainerIdentity,
    },
    Membership {
        message: RemoteMessageIdentity,
        container: RemoteContainerIdentity,
    },
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum TombstoneTargetWire {
    Thread {
        identity: RemoteThreadIdentity,
    },
    Message {
        identity: RemoteMessageIdentity,
    },
    Container {
        identity: RemoteContainerIdentity,
    },
    Membership {
        message: RemoteMessageIdentity,
        container: RemoteContainerIdentity,
    },
}

impl<'de> Deserialize<'de> for TombstoneTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TombstoneTargetWire::deserialize(deserializer)?;
        match wire {
            TombstoneTargetWire::Thread { identity } => Ok(Self::Thread { identity }),
            TombstoneTargetWire::Message { identity } => Ok(Self::Message { identity }),
            TombstoneTargetWire::Container { identity } => Ok(Self::Container { identity }),
            TombstoneTargetWire::Membership { message, container } => {
                ContainerMembership {
                    message: message.clone(),
                    container: container.clone(),
                }
                .validate()
                .map_err(D::Error::custom)?;
                Ok(Self::Membership { message, container })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderTombstone {
    pub target: TombstoneTarget,
    pub observed_at: UnixMillis,
    pub cursor: Option<OpaqueSyncCursor>,
}

impl ProviderTombstone {
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if let TombstoneTarget::Membership { message, container } = &self.target {
            ContainerMembership {
                message: message.clone(),
                container: container.clone(),
            }
            .validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderBodyState {
    Complete,
    Truncated,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderThreadUpsert {
    pub identity: RemoteThreadIdentity,
    pub subject: ProviderSubject,
    pub participants: ProviderParticipants,
    pub snippet: ProviderSnippet,
    pub latest_at: UnixMillis,
    pub message_count: ProviderThreadMessageCount,
    pub in_inbox: bool,
    pub unread: bool,
    pub starred: bool,
    pub has_attachments: bool,
    pub has_invite: bool,
    pub has_links: bool,
    pub has_from_me: bool,
    pub category: ProviderCategory,
    pub revision: Option<ProviderRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderMessageUpsert {
    pub identity: RemoteMessageIdentity,
    pub subject: ProviderSubject,
    pub sender_name: ProviderSenderName,
    pub sender_email: ProviderEmail,
    pub recipients: ProviderRecipients,
    pub cc_recipients: ProviderRecipients,
    pub bcc_recipients: ProviderRecipients,
    pub sent_at: UnixMillis,
    pub body_text: NormalizedPlainBody,
    pub body_state: ProviderBodyState,
    pub is_from_me: bool,
    pub revision: Option<ProviderRevision>,
    pub keywords: BTreeSet<ProviderMessageKeyword>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internet_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<Vec<String>>,
}

impl ProviderMessageUpsert {
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.keywords.len() > MAX_MESSAGE_KEYWORDS {
            return Err(ProviderContractError::CollectionTooLarge {
                collection: "message keywords",
                max_items: MAX_MESSAGE_KEYWORDS,
            });
        }
        if self.body_state == ProviderBodyState::Unavailable && !self.body_text.as_str().is_empty()
        {
            return Err(ProviderContractError::NotNormalized {
                field: "unavailable provider message body",
            });
        }
        for (value, field) in [
            (self.internet_message_id.as_deref(), "Internet Message-ID"),
            (self.in_reply_to.as_deref(), "In-Reply-To"),
        ] {
            if value
                .is_some_and(|value| crate::internet_message::validate_message_id(value).is_err())
            {
                return Err(ProviderContractError::NotNormalized { field });
            }
        }
        if self.references.as_ref().is_some_and(|references| {
            crate::internet_message::validate_references(references).is_err()
        }) {
            return Err(ProviderContractError::NotNormalized {
                field: "References",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ContainerMembershipChange {
    Upsert { membership: ContainerMembership },
    Remove { membership: ContainerMembership },
}

impl ContainerMembershipChange {
    pub fn membership(&self) -> &ContainerMembership {
        match self {
            Self::Upsert { membership } | Self::Remove { membership } => membership,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderBatch {
    pub mux_account_id: MuxAccountId,
    pub batch_id: ProviderBatchId,
    /// The cursor that must still be durable when this batch is applied.
    /// `None` means the scope must not have a cursor yet.
    pub expected_prior_cursor: Option<OpaqueSyncCursor>,
    pub cursor: ProviderSyncCursor,
    pub observed_at: UnixMillis,
    pub thread_upserts: Vec<ProviderThreadUpsert>,
    pub message_upserts: Vec<ProviderMessageUpsert>,
    pub container_upserts: Vec<ProviderContainer>,
    pub membership_changes: Vec<ContainerMembershipChange>,
    pub tombstones: Vec<ProviderTombstone>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderBatchWire {
    mux_account_id: MuxAccountId,
    batch_id: ProviderBatchId,
    expected_prior_cursor: Option<OpaqueSyncCursor>,
    cursor: ProviderSyncCursor,
    observed_at: UnixMillis,
    #[serde(deserialize_with = "deserialize_batch_collection")]
    thread_upserts: Vec<ProviderThreadUpsert>,
    #[serde(deserialize_with = "deserialize_batch_collection")]
    message_upserts: Vec<ProviderMessageUpsert>,
    #[serde(deserialize_with = "deserialize_batch_collection")]
    container_upserts: Vec<ProviderContainer>,
    #[serde(deserialize_with = "deserialize_batch_collection")]
    membership_changes: Vec<ContainerMembershipChange>,
    #[serde(deserialize_with = "deserialize_batch_collection")]
    tombstones: Vec<ProviderTombstone>,
}

impl ProviderBatch {
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.cursor.mux_account_id != self.mux_account_id {
            return Err(ProviderContractError::AccountMismatch {
                relation: "provider batch cursor",
            });
        }

        let collection_lengths = [
            ("thread upserts", self.thread_upserts.len()),
            ("message upserts", self.message_upserts.len()),
            ("container upserts", self.container_upserts.len()),
            ("membership changes", self.membership_changes.len()),
            ("tombstones", self.tombstones.len()),
        ];
        for (collection, length) in collection_lengths {
            if length > MAX_BATCH_COLLECTION_ITEMS {
                return Err(ProviderContractError::CollectionTooLarge {
                    collection,
                    max_items: MAX_BATCH_COLLECTION_ITEMS,
                });
            }
        }
        let total_items: usize = collection_lengths.iter().map(|(_, length)| length).sum();
        if total_items > MAX_BATCH_TOTAL_ITEMS {
            return Err(ProviderContractError::BatchTooLarge {
                max_items: MAX_BATCH_TOTAL_ITEMS,
            });
        }

        let mut thread_ids = BTreeSet::new();
        for thread in &self.thread_upserts {
            self.require_batch_account(&thread.identity.mux_account_id, "thread upsert")?;
            if !thread_ids.insert(thread.identity.remote_thread_id.as_str()) {
                return Err(ProviderContractError::DuplicateIdentity {
                    collection: "thread upserts",
                });
            }
        }

        let mut message_ids = BTreeSet::new();
        for message in &self.message_upserts {
            self.require_batch_account(&message.identity.mux_account_id, "message upsert")?;
            message.validate()?;
            if !message_ids.insert(message.identity.remote_message_id.as_str()) {
                return Err(ProviderContractError::DuplicateIdentity {
                    collection: "message upserts",
                });
            }
        }

        let mut container_ids = BTreeSet::new();
        for container in &self.container_upserts {
            self.require_batch_account(&container.identity.mux_account_id, "container upsert")?;
            if !container_ids.insert(container.identity.remote_container_id.as_str()) {
                return Err(ProviderContractError::DuplicateIdentity {
                    collection: "container upserts",
                });
            }
        }

        let mut membership_ids = BTreeSet::new();
        for change in &self.membership_changes {
            let membership = change.membership();
            membership.validate()?;
            self.require_batch_account(
                &membership.message.mux_account_id,
                "membership change message",
            )?;
            self.require_batch_account(
                &membership.container.mux_account_id,
                "membership change container",
            )?;
            if !membership_ids.insert((
                membership.message.remote_message_id.as_str(),
                membership.container.remote_container_id.as_str(),
            )) {
                return Err(ProviderContractError::DuplicateIdentity {
                    collection: "membership changes",
                });
            }
        }

        let mut tombstone_ids = BTreeSet::new();
        for tombstone in &self.tombstones {
            tombstone.validate()?;
            let identity = match &tombstone.target {
                TombstoneTarget::Thread { identity } => {
                    self.require_batch_account(&identity.mux_account_id, "thread tombstone")?;
                    if thread_ids.contains(identity.remote_thread_id.as_str()) {
                        return Err(ProviderContractError::ContradictoryChange {
                            entity: "thread",
                        });
                    }
                    ("thread", identity.remote_thread_id.as_str(), "")
                }
                TombstoneTarget::Message { identity } => {
                    self.require_batch_account(&identity.mux_account_id, "message tombstone")?;
                    if message_ids.contains(identity.remote_message_id.as_str()) {
                        return Err(ProviderContractError::ContradictoryChange {
                            entity: "message",
                        });
                    }
                    ("message", identity.remote_message_id.as_str(), "")
                }
                TombstoneTarget::Container { identity } => {
                    self.require_batch_account(&identity.mux_account_id, "container tombstone")?;
                    if container_ids.contains(identity.remote_container_id.as_str()) {
                        return Err(ProviderContractError::ContradictoryChange {
                            entity: "container",
                        });
                    }
                    ("container", identity.remote_container_id.as_str(), "")
                }
                TombstoneTarget::Membership { message, container } => {
                    self.require_batch_account(
                        &message.mux_account_id,
                        "membership tombstone message",
                    )?;
                    self.require_batch_account(
                        &container.mux_account_id,
                        "membership tombstone container",
                    )?;
                    if membership_ids.contains(&(
                        message.remote_message_id.as_str(),
                        container.remote_container_id.as_str(),
                    )) {
                        return Err(ProviderContractError::ContradictoryChange {
                            entity: "membership",
                        });
                    }
                    (
                        "membership",
                        message.remote_message_id.as_str(),
                        container.remote_container_id.as_str(),
                    )
                }
            };
            if !tombstone_ids.insert(identity) {
                return Err(ProviderContractError::DuplicateIdentity {
                    collection: "tombstones",
                });
            }
        }

        Ok(())
    }

    fn require_batch_account(
        &self,
        account_id: &MuxAccountId,
        relation: &'static str,
    ) -> Result<(), ProviderContractError> {
        if account_id != &self.mux_account_id {
            return Err(ProviderContractError::AccountMismatch { relation });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ProviderBatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProviderBatchWire::deserialize(deserializer)?;
        let batch = Self {
            mux_account_id: wire.mux_account_id,
            batch_id: wire.batch_id,
            expected_prior_cursor: wire.expected_prior_cursor,
            cursor: wire.cursor,
            observed_at: wire.observed_at,
            thread_upserts: wire.thread_upserts,
            message_upserts: wire.message_upserts,
            container_upserts: wire.container_upserts,
            membership_changes: wire.membership_changes,
            tombstones: wire.tombstones,
        };
        batch.validate().map_err(D::Error::custom)?;
        Ok(batch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderBatchApplyCounts {
    pub thread_upserts: u32,
    pub message_upserts: u32,
    pub container_upserts: u32,
    pub membership_changes: u32,
    pub tombstones: u32,
}

impl From<&ProviderBatch> for ProviderBatchApplyCounts {
    fn from(batch: &ProviderBatch) -> Self {
        Self {
            thread_upserts: batch.thread_upserts.len() as u32,
            message_upserts: batch.message_upserts.len() as u32,
            container_upserts: batch.container_upserts.len() as u32,
            membership_changes: batch.membership_changes.len() as u32,
            tombstones: batch.tombstones.len() as u32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderBatchApplyResult {
    pub applied: bool,
    pub batch_id: ProviderBatchId,
    pub cursor: ProviderSyncCursor,
    pub counts: ProviderBatchApplyCounts,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_id() -> MuxAccountId {
        MuxAccountId::new("account-1").expect("valid account ID")
    }

    fn remote_account_id() -> RemoteAccountId {
        RemoteAccountId::new("remote-account-1").expect("valid remote account ID")
    }

    fn message_identity() -> RemoteMessageIdentity {
        RemoteMessageIdentity {
            mux_account_id: account_id(),
            remote_message_id: RemoteMessageId::new("message-42").expect("valid remote message ID"),
            remote_thread_id: Some(
                RemoteThreadId::new("thread-7").expect("valid remote thread ID"),
            ),
        }
    }

    fn thread_identity() -> RemoteThreadIdentity {
        RemoteThreadIdentity {
            mux_account_id: account_id(),
            remote_thread_id: RemoteThreadId::new("thread-7").expect("valid remote thread ID"),
        }
    }

    fn container_identity() -> RemoteContainerIdentity {
        RemoteContainerIdentity {
            mux_account_id: account_id(),
            remote_container_id: RemoteContainerId::new("inbox")
                .expect("valid remote container ID"),
        }
    }

    fn sample_thread_upsert() -> ProviderThreadUpsert {
        ProviderThreadUpsert {
            identity: thread_identity(),
            subject: ProviderSubject::new("Normalized provider thread").expect("valid subject"),
            participants: ProviderParticipants::new("Ada <ada@example.test>")
                .expect("valid participants"),
            snippet: ProviderSnippet::new("A bounded preview").expect("valid snippet"),
            latest_at: UnixMillis::new(1_778_000_000_000).expect("valid timestamp"),
            message_count: ProviderThreadMessageCount::new(1).expect("valid message count"),
            in_inbox: true,
            unread: true,
            starred: false,
            has_attachments: false,
            has_invite: false,
            has_links: false,
            has_from_me: false,
            category: ProviderCategory::new("primary").expect("valid category"),
            revision: Some(ProviderRevision::new("revision-1").expect("valid revision")),
        }
    }

    fn sample_message_upsert() -> ProviderMessageUpsert {
        ProviderMessageUpsert {
            identity: message_identity(),
            subject: ProviderSubject::new("Normalized provider thread").expect("valid subject"),
            sender_name: ProviderSenderName::new("Ada").expect("valid sender name"),
            sender_email: ProviderEmail::new("ada@example.test").expect("valid sender email"),
            recipients: ProviderRecipients::new("mux@example.test").expect("valid recipients"),
            cc_recipients: ProviderRecipients::new("").expect("valid empty recipients"),
            bcc_recipients: ProviderRecipients::new("").expect("valid empty recipients"),
            sent_at: UnixMillis::new(1_778_000_000_000).expect("valid timestamp"),
            body_text: NormalizedPlainBody::new("Plain text only.\nSecond line.")
                .expect("valid normalized body"),
            body_state: ProviderBodyState::Complete,
            is_from_me: false,
            revision: Some(ProviderRevision::new("revision-1").expect("valid revision")),
            keywords: [ProviderMessageKeyword::new("$seen").expect("valid keyword")]
                .into_iter()
                .collect(),
            internet_message_id: Some("<provider-message-1@example.test>".into()),
            in_reply_to: None,
            references: None,
        }
    }

    fn sample_batch() -> ProviderBatch {
        let membership = ContainerMembership {
            message: message_identity(),
            container: container_identity(),
        };
        ProviderBatch {
            mux_account_id: account_id(),
            batch_id: ProviderBatchId::new("batch-1").expect("valid batch ID"),
            expected_prior_cursor: None,
            cursor: ProviderSyncCursor {
                mux_account_id: account_id(),
                scope: SyncCursorScope::Account,
                value: OpaqueSyncCursor::new("cursor-1").expect("valid cursor"),
            },
            observed_at: UnixMillis::new(1_778_000_000_100).expect("valid timestamp"),
            thread_upserts: vec![sample_thread_upsert()],
            message_upserts: vec![sample_message_upsert()],
            container_upserts: vec![ProviderContainer {
                identity: container_identity(),
                display_name: ContainerDisplayName::new("Inbox").expect("valid display name"),
                kind: ContainerKind::Mailbox,
                role: Some(ContainerRole::Inbox),
                parent_remote_container_id: None,
                selectable: true,
            }],
            membership_changes: vec![ContainerMembershipChange::Upsert { membership }],
            tombstones: Vec::new(),
        }
    }

    #[test]
    fn account_state_round_trips_with_protocol_neutral_shape() {
        let state = ProviderAccountState {
            identity: ProviderAccountIdentity {
                mux_account_id: account_id(),
                provider_kind: ProviderKind::Jmap,
                remote_account_id: remote_account_id(),
            },
            capabilities: ProviderCapabilities::new([
                ProviderCapability::DeltaSync,
                ProviderCapability::Mutations,
                ProviderCapability::DeltaSync,
            ]),
            auth_status: ProviderAuthStatus::Ready,
            sync_status: ProviderSyncStatus::Idle,
        };

        let json = serde_json::to_value(&state).expect("serialize provider state");
        assert_eq!(json["identity"]["providerKind"], "jmap");
        assert_eq!(
            json["capabilities"]["supported"]
                .as_array()
                .expect("capability array")
                .len(),
            2
        );
        assert_eq!(
            serde_json::from_value::<ProviderAccountState>(json)
                .expect("deserialize provider state"),
            state
        );
    }

    #[test]
    fn strict_shapes_reject_secret_shaped_fields() {
        let json = serde_json::json!({
            "identity": {
                "muxAccountId": "account-1",
                "providerKind": "gmail",
                "remoteAccountId": "remote-account-1"
            },
            "capabilities": { "supported": ["deltaSync"] },
            "authStatus": "ready",
            "syncStatus": "idle",
            "accessToken": "must-never-be-durable-here"
        });

        let error = serde_json::from_value::<ProviderAccountState>(json)
            .expect_err("unknown credential field must be rejected");
        assert!(error.to_string().contains("unknown field `accessToken`"));
    }

    #[test]
    fn opaque_values_are_bounded_and_reject_control_characters() {
        assert_eq!(
            RemoteMessageId::new("").expect_err("empty IDs are invalid"),
            ProviderContractError::Empty {
                field: "remote message ID"
            }
        );
        assert!(matches!(
            RemoteMessageId::new("x".repeat(MAX_REMOTE_ID_BYTES + 1)),
            Err(ProviderContractError::TooLong { .. })
        ));
        assert_eq!(
            OpaqueSyncCursor::new("state\nnext").expect_err("control characters are invalid"),
            ProviderContractError::ControlCharacter {
                field: "sync cursor"
            }
        );
        assert!(serde_json::from_str::<RemoteMessageId>("\"bad\\u0000id\"").is_err());
        assert!(ProviderRevision::new("r".repeat(MAX_REVISION_BYTES)).is_ok());
        assert!(matches!(
            ProviderRevision::new("r".repeat(MAX_REVISION_BYTES + 1)),
            Err(ProviderContractError::TooLong { .. })
        ));
    }

    #[test]
    fn memberships_and_container_scoped_cursors_round_trip() {
        let container = RemoteContainerIdentity {
            mux_account_id: account_id(),
            remote_container_id: RemoteContainerId::new("inbox")
                .expect("valid remote container ID"),
        };
        let membership = ContainerMembership {
            message: message_identity(),
            container: container.clone(),
        };
        membership.validate().expect("same-account membership");
        let cursor = ProviderSyncCursor {
            mux_account_id: account_id(),
            scope: SyncCursorScope::Container {
                remote_container_id: container.remote_container_id.clone(),
            },
            value: OpaqueSyncCursor::new("opaque-state-99").expect("valid cursor"),
        };

        let membership_json = serde_json::to_string(&membership).expect("serialize membership");
        assert_eq!(
            serde_json::from_str::<ContainerMembership>(&membership_json)
                .expect("deserialize membership"),
            membership
        );

        let cursor_json = serde_json::to_value(&cursor).expect("serialize cursor");
        assert_eq!(cursor_json["scope"]["kind"], "container");
        assert_eq!(cursor_json["scope"]["remoteContainerId"], "inbox");
        assert_eq!(
            serde_json::from_value::<ProviderSyncCursor>(cursor_json).expect("deserialize cursor"),
            cursor
        );
    }

    #[test]
    fn tombstones_identify_deleted_remote_entities_without_payloads() {
        let tombstone = ProviderTombstone {
            target: TombstoneTarget::Message {
                identity: message_identity(),
            },
            observed_at: UnixMillis::new(1_778_000_000_000).expect("valid timestamp"),
            cursor: Some(OpaqueSyncCursor::new("after-delete").expect("valid cursor")),
        };

        let json = serde_json::to_value(&tombstone).expect("serialize tombstone");
        assert_eq!(json["target"]["kind"], "message");
        assert!(json["target"].get("bodyHtml").is_none());
        assert_eq!(
            serde_json::from_value::<ProviderTombstone>(json).expect("deserialize tombstone"),
            tombstone
        );
        assert!(serde_json::from_str::<UnixMillis>("-1").is_err());
    }

    #[test]
    fn cross_account_memberships_fail_contract_validation() {
        let membership = ContainerMembership {
            message: message_identity(),
            container: RemoteContainerIdentity {
                mux_account_id: MuxAccountId::new("account-2").expect("valid account ID"),
                remote_container_id: RemoteContainerId::new("inbox")
                    .expect("valid remote container ID"),
            },
        };

        assert_eq!(
            membership.validate(),
            Err(ProviderContractError::AccountMismatch {
                relation: "container membership"
            })
        );

        let membership_json = serde_json::to_value(&membership).expect("serialize membership");
        let membership_error = serde_json::from_value::<ContainerMembership>(membership_json)
            .expect_err("cross-account membership must fail deserialization");
        assert!(membership_error
            .to_string()
            .contains("container membership must belong to one Mux account"));
    }

    #[test]
    fn cross_account_membership_tombstones_fail_deserialization() {
        let target_json = serde_json::json!({
            "kind": "membership",
            "message": {
                "muxAccountId": "account-1",
                "remoteMessageId": "message-42",
                "remoteThreadId": "thread-7"
            },
            "container": {
                "muxAccountId": "account-2",
                "remoteContainerId": "inbox"
            }
        });

        let target_error = serde_json::from_value::<TombstoneTarget>(target_json.clone())
            .expect_err("cross-account target must fail deserialization");
        assert!(target_error
            .to_string()
            .contains("container membership must belong to one Mux account"));

        let tombstone_json = serde_json::json!({
            "target": target_json,
            "observedAt": 1_778_000_000_000_i64,
            "cursor": "after-delete"
        });
        let tombstone_error = serde_json::from_value::<ProviderTombstone>(tombstone_json)
            .expect_err("cross-account tombstone must fail deserialization");
        assert!(tombstone_error
            .to_string()
            .contains("container membership must belong to one Mux account"));
    }

    #[test]
    fn provider_batch_round_trips_and_reports_deterministic_counts() {
        let batch = sample_batch();
        batch.validate().expect("valid normalized provider batch");

        let json = serde_json::to_value(&batch).expect("serialize provider batch");
        assert_eq!(json["batchId"], "batch-1");
        assert_eq!(json["threadUpserts"][0]["messageCount"], 1);
        assert_eq!(json["messageUpserts"][0]["bodyState"], "complete");
        assert!(json["messageUpserts"][0].get("bodyHtml").is_none());
        assert_eq!(
            serde_json::from_value::<ProviderBatch>(json).expect("deserialize provider batch"),
            batch
        );

        let counts = ProviderBatchApplyCounts::from(&batch);
        assert_eq!(counts.thread_upserts, 1);
        assert_eq!(counts.message_upserts, 1);
        assert_eq!(counts.container_upserts, 1);
        assert_eq!(counts.membership_changes, 1);
        assert_eq!(counts.tombstones, 0);
    }

    #[test]
    fn provider_batch_deserialization_rejects_ambient_authority_and_raw_content_fields() {
        let original = serde_json::to_value(sample_batch()).expect("serialize provider batch");

        for forbidden_field in ["accessToken", "password", "rawMime"] {
            let mut json = original.clone();
            json.as_object_mut()
                .expect("batch object")
                .insert(forbidden_field.to_string(), serde_json::json!("forbidden"));
            assert!(
                serde_json::from_value::<ProviderBatch>(json).is_err(),
                "{forbidden_field} must be rejected"
            );
        }

        let mut json = original;
        json["messageUpserts"][0]
            .as_object_mut()
            .expect("message object")
            .insert("bodyHtml".to_string(), serde_json::json!("<b>unsafe</b>"));
        let error = serde_json::from_value::<ProviderBatch>(json)
            .expect_err("raw HTML field must be rejected");
        assert!(error.to_string().contains("unknown field `bodyHtml`"));
    }

    #[test]
    fn provider_batch_deserialization_enforces_account_scope() {
        let original = sample_batch();

        let mut cursor_mismatch = original.clone();
        cursor_mismatch.cursor.mux_account_id =
            MuxAccountId::new("account-2").expect("valid account ID");
        let error = serde_json::from_value::<ProviderBatch>(
            serde_json::to_value(cursor_mismatch).expect("serialize invalid batch"),
        )
        .expect_err("cursor account mismatch must fail deserialization");
        assert!(error
            .to_string()
            .contains("provider batch cursor must belong to one Mux account"));

        let mut message_mismatch = original.clone();
        message_mismatch.message_upserts[0].identity.mux_account_id =
            MuxAccountId::new("account-2").expect("valid account ID");
        let error = serde_json::from_value::<ProviderBatch>(
            serde_json::to_value(message_mismatch).expect("serialize invalid batch"),
        )
        .expect_err("message account mismatch must fail deserialization");
        assert!(error
            .to_string()
            .contains("message upsert must belong to one Mux account"));

        let mut tombstone_mismatch = original;
        tombstone_mismatch.tombstones.push(ProviderTombstone {
            target: TombstoneTarget::Thread {
                identity: RemoteThreadIdentity {
                    mux_account_id: MuxAccountId::new("account-2").expect("valid account ID"),
                    remote_thread_id: RemoteThreadId::new("deleted-thread")
                        .expect("valid remote thread ID"),
                },
            },
            observed_at: UnixMillis::new(1_778_000_000_200).expect("valid timestamp"),
            cursor: None,
        });
        let error = serde_json::from_value::<ProviderBatch>(
            serde_json::to_value(tombstone_mismatch).expect("serialize invalid batch"),
        )
        .expect_err("tombstone account mismatch must fail deserialization");
        assert!(error
            .to_string()
            .contains("thread tombstone must belong to one Mux account"));
    }

    #[test]
    fn provider_batch_rejects_duplicate_identities_and_collection_overflow() {
        let mut duplicate_threads = sample_batch();
        duplicate_threads
            .thread_upserts
            .push(duplicate_threads.thread_upserts[0].clone());
        assert_eq!(
            duplicate_threads.validate(),
            Err(ProviderContractError::DuplicateIdentity {
                collection: "thread upserts"
            })
        );

        let mut duplicate_memberships = sample_batch();
        duplicate_memberships
            .membership_changes
            .push(duplicate_memberships.membership_changes[0].clone());
        assert_eq!(
            duplicate_memberships.validate(),
            Err(ProviderContractError::DuplicateIdentity {
                collection: "membership changes"
            })
        );

        let mut oversized_collection = sample_batch();
        oversized_collection.thread_upserts =
            vec![sample_thread_upsert(); MAX_BATCH_COLLECTION_ITEMS + 1];
        assert_eq!(
            oversized_collection.validate(),
            Err(ProviderContractError::CollectionTooLarge {
                collection: "thread upserts",
                max_items: MAX_BATCH_COLLECTION_ITEMS
            })
        );
        let oversized_json =
            serde_json::to_value(&oversized_collection).expect("serialize oversized collection");
        assert!(serde_json::from_value::<ProviderBatch>(oversized_json).is_err());

        let mut oversized_batch = sample_batch();
        oversized_batch.thread_upserts = vec![sample_thread_upsert(); 1_000];
        oversized_batch.message_upserts = vec![sample_message_upsert(); 1_000];
        oversized_batch.container_upserts =
            vec![oversized_batch.container_upserts[0].clone(); 1_000];
        oversized_batch.membership_changes =
            vec![oversized_batch.membership_changes[0].clone(); 1_000];
        oversized_batch.tombstones = vec![
            ProviderTombstone {
                target: TombstoneTarget::Thread {
                    identity: thread_identity(),
                },
                observed_at: UnixMillis::new(1_778_000_000_200).expect("valid timestamp"),
                cursor: None,
            };
            1
        ];
        assert_eq!(
            oversized_batch.validate(),
            Err(ProviderContractError::BatchTooLarge {
                max_items: MAX_BATCH_TOTAL_ITEMS
            })
        );
    }

    #[test]
    fn provider_batch_rejects_upsert_tombstone_contradictions() {
        let mut thread = sample_batch();
        thread.tombstones.push(ProviderTombstone {
            target: TombstoneTarget::Thread {
                identity: thread_identity(),
            },
            observed_at: UnixMillis::new(1_778_000_000_200).expect("valid timestamp"),
            cursor: None,
        });
        assert_eq!(
            thread.validate(),
            Err(ProviderContractError::ContradictoryChange { entity: "thread" })
        );

        let mut message = sample_batch();
        message.tombstones.push(ProviderTombstone {
            target: TombstoneTarget::Message {
                identity: message_identity(),
            },
            observed_at: UnixMillis::new(1_778_000_000_200).expect("valid timestamp"),
            cursor: None,
        });
        assert_eq!(
            message.validate(),
            Err(ProviderContractError::ContradictoryChange { entity: "message" })
        );

        let mut container = sample_batch();
        container.tombstones.push(ProviderTombstone {
            target: TombstoneTarget::Container {
                identity: container_identity(),
            },
            observed_at: UnixMillis::new(1_778_000_000_200).expect("valid timestamp"),
            cursor: None,
        });
        assert_eq!(
            container.validate(),
            Err(ProviderContractError::ContradictoryChange {
                entity: "container"
            })
        );

        let mut membership = sample_batch();
        membership.tombstones.push(ProviderTombstone {
            target: TombstoneTarget::Membership {
                message: message_identity(),
                container: container_identity(),
            },
            observed_at: UnixMillis::new(1_778_000_000_200).expect("valid timestamp"),
            cursor: None,
        });
        assert_eq!(
            membership.validate(),
            Err(ProviderContractError::ContradictoryChange {
                entity: "membership"
            })
        );

        let serialized = serde_json::to_value(membership).expect("serialize invalid batch");
        assert!(serde_json::from_value::<ProviderBatch>(serialized).is_err());
    }

    #[test]
    fn provider_batch_text_and_message_state_are_normalized_and_bounded() {
        assert!(ProviderSubject::new("é".repeat(1_000)).is_ok());
        assert!(matches!(
            ProviderSubject::new("é".repeat(1_001)),
            Err(ProviderContractError::TooLong { .. })
        ));
        assert!(NormalizedPlainBody::new("line one\n\tline two").is_ok());
        assert_eq!(
            NormalizedPlainBody::new("line one\r\nline two").expect_err("CRLF is not normalized"),
            ProviderContractError::NotNormalized {
                field: "normalized plain body"
            }
        );
        assert!(NormalizedPlainBody::new("x".repeat(MAX_PLAIN_BODY_BYTES)).is_ok());
        assert!(matches!(
            NormalizedPlainBody::new("x".repeat(MAX_PLAIN_BODY_BYTES + 1)),
            Err(ProviderContractError::TooLong { .. })
        ));
        assert!(ProviderMessageKeyword::new("$seen").is_ok());
        assert!(matches!(
            ProviderMessageKeyword::new("$Seen"),
            Err(ProviderContractError::NotNormalized { .. })
        ));
        assert!(ProviderThreadMessageCount::new(0).is_ok());
        assert!(ProviderThreadMessageCount::new(MAX_THREAD_MESSAGE_COUNT + 1).is_err());

        let mut unsafe_threading = sample_batch();
        unsafe_threading.message_upserts[0].internet_message_id =
            Some("<message@example.test>\r\nBcc: injected@example.test".into());
        assert_eq!(
            unsafe_threading.validate(),
            Err(ProviderContractError::NotNormalized {
                field: "Internet Message-ID"
            })
        );
        let mut oversized_references = sample_batch();
        oversized_references.message_upserts[0].references =
            Some(vec![
                "<reference@example.test>".into();
                crate::internet_message::MAX_REFERENCES + 1
            ]);
        assert_eq!(
            oversized_references.validate(),
            Err(ProviderContractError::NotNormalized {
                field: "References"
            })
        );

        let mut unavailable_body = sample_batch();
        unavailable_body.message_upserts[0].body_state = ProviderBodyState::Unavailable;
        assert_eq!(
            unavailable_body.validate(),
            Err(ProviderContractError::NotNormalized {
                field: "unavailable provider message body"
            })
        );
        assert!(serde_json::from_value::<ProviderBatch>(
            serde_json::to_value(unavailable_body).expect("serialize invalid body state")
        )
        .is_err());
    }
}
