//! Passphrase-encrypted, local credential storage.
//!
//! The vault deliberately has no Tauri command surface. Provider adapters use
//! this crate-private API, while UI integration is limited to lifecycle/status
//! operations in `lib.rs`. The database and the vault are separate files: a
//! vault is always named `mux.vault` beside the database that selected it.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use fs2::FileExt;
use rand_core::{OsRng, RngCore};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

const MAGIC: &[u8; 8] = b"MUXVAULT";
const FORMAT_VERSION: u16 = 1;
const KDF_ARGON2ID: u8 = 1;
const ARGON2_VERSION: u8 = 0x13;
const ARGON2_MEMORY_KIB: u32 = 65_536;
const ARGON2_ITERATIONS: u32 = 3;
const ARGON2_PARALLELISM: u32 = 4;
const DERIVED_KEY_LEN: u16 = 32;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 8 + 2 + 1 + 1 + 4 + 4 + 4 + 2 + SALT_LEN + NONCE_LEN + 4;

const MAX_VAULT_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PLAINTEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_PASSPHRASE_BYTES: usize = 1024;
const MAX_RECORDS: usize = 256;
const MAX_RECORD_KEY_BYTES: usize = 256;
const MAX_RECORD_VALUE_BYTES: usize = 64 * 1024;

#[cfg(test)]
static KDF_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static WRITE_FAILPOINT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VaultStatus {
    Absent,
    Locked,
    Unlocked,
    Unavailable,
}

#[derive(Debug, Error)]
pub(crate) enum VaultError {
    #[error("credential vault already exists")]
    AlreadyExists,
    #[error("credential vault does not exist")]
    Absent,
    #[error("credential vault is locked")]
    Locked,
    #[allow(dead_code)] // Used by the Rust-only credential API once a real adapter is connected.
    #[error("credential vault unlock is stale; unlock it again")]
    StaleUnlock,
    #[error("credential vault passphrase is invalid")]
    InvalidPassphrase,
    #[error("credential identifier is invalid")]
    InvalidIdentifier,
    #[error("credential vault limit exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error("credential vault format is unsupported")]
    UnsupportedFormat,
    #[error("credential vault is malformed")]
    Malformed,
    #[error("credential vault authentication failed")]
    AuthenticationFailed,
    #[error("credential vault permissions are unsafe")]
    UnsafePermissions,
    #[error("credential vault path is not a regular file")]
    InvalidFileType,
    #[error("credential vault cryptography failed")]
    Cryptography,
    #[error("credential vault I/O failed")]
    Io(#[source] io::Error),
    /// The atomic rename succeeded, but syncing the directory failed. The new
    /// data may be committed. Callers must not blindly retry non-idempotent
    /// surrounding work; they should reopen and inspect the vault.
    #[error("credential vault {operation} may have committed")]
    CommitUncertain {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

impl From<io::Error> for VaultError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

struct UnlockMaterial {
    #[allow(dead_code)] // Retained for Rust-only get/put/remove; never exposed over Tauri IPC.
    key: Zeroizing<[u8; 32]>,
    salt: [u8; SALT_LEN],
}

#[derive(Default)]
struct VaultRecords(BTreeMap<String, Vec<u8>>);

impl Drop for VaultRecords {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

/// A process-local handle. Constructing a handle never unlocks a vault, even
/// when another handle in the same process is unlocked.
pub(crate) struct CredentialVault {
    vault_path: PathBuf,
    lock_path: PathBuf,
    unlocked: Option<UnlockMaterial>,
}

impl CredentialVault {
    /// Select `mux.vault` and `mux.vault.lock` in the database's directory.
    pub(crate) fn for_database(database_path: &Path) -> Self {
        let parent = database_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Self::at_path(parent.join("mux.vault"))
    }

    pub(crate) fn at_path(vault_path: PathBuf) -> Self {
        let lock_path = vault_path.with_file_name("mux.vault.lock");
        Self {
            vault_path,
            lock_path,
            unlocked: None,
        }
    }

    /// Returns only lifecycle state. It intentionally does not validate or
    /// describe file contents, accounts, identifiers, or failure details.
    pub(crate) fn status(&self) -> VaultStatus {
        match fs::symlink_metadata(&self.vault_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return VaultStatus::Absent,
            Err(_) => return VaultStatus::Unavailable,
            Ok(_) => {}
        }
        let Ok(lock_file) = self.acquire_lock() else {
            return VaultStatus::Unavailable;
        };
        let result = self
            .read_envelope()
            .and_then(|bytes| parse_envelope(&bytes).map(|parsed| parsed.salt));
        drop(lock_file);
        match result {
            Ok(salt)
                if self
                    .unlocked
                    .as_ref()
                    .is_some_and(|material| material.salt == salt) =>
            {
                VaultStatus::Unlocked
            }
            Ok(_) => VaultStatus::Locked,
            Err(_) => VaultStatus::Unavailable,
        }
    }

    /// Creates an empty vault. Any existing filesystem entry is preserved and
    /// causes `AlreadyExists`; corrupt vaults are never silently replaced.
    pub(crate) fn create(&mut self, passphrase: Zeroizing<Vec<u8>>) -> Result<(), VaultError> {
        validate_passphrase(&passphrase)?;
        let lock_file = self.acquire_lock()?;
        match fs::symlink_metadata(&self.vault_path) {
            Ok(_) => return Err(VaultError::AlreadyExists),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let salt = random_array::<SALT_LEN>();
        let key = derive_key(&passphrase, &salt)?;
        let records = VaultRecords::default();
        let envelope = encrypt_envelope(&records, &key, salt)?;
        let result = self.atomic_write(&envelope, false, "creation");
        if write_reached_rename(&result) {
            self.unlocked = Some(UnlockMaterial { key, salt });
        }
        drop(lock_file);
        result
    }

    /// Unlocks the latest on-disk vault. Wrong passwords and authenticated
    /// tampering return the same error and leave the handle locked.
    pub(crate) fn unlock(&mut self, passphrase: Zeroizing<Vec<u8>>) -> Result<(), VaultError> {
        self.unlocked = None;
        validate_passphrase(&passphrase)?;
        let lock_file = self.acquire_lock()?;
        let envelope = self.read_envelope()?;
        let parsed = parse_envelope(&envelope)?;
        let key = derive_key(&passphrase, &parsed.salt)?;
        let plaintext = decrypt_envelope(&parsed, &key)?;
        let _records = decode_records(&plaintext)?;
        self.unlocked = Some(UnlockMaterial {
            key,
            salt: parsed.salt,
        });
        drop(lock_file);
        Ok(())
    }

    pub(crate) fn lock(&mut self) {
        self.unlocked = None;
    }

    /// Returns a zeroizing copy. There is intentionally no generic IPC wrapper
    /// around this API.
    #[allow(dead_code)] // Provider adapters consume this crate-private API in a later task.
    pub(crate) fn get(&self, identifier: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        validate_identifier(identifier)?;
        let material = self.unlocked.as_ref().ok_or(VaultError::Locked)?;
        let lock_file = self.acquire_lock()?;
        let records = self.read_latest_records(material)?;
        let value = records
            .0
            .get(identifier)
            .map(|value| Zeroizing::new(value.clone()));
        drop(lock_file);
        Ok(value)
    }

    #[allow(dead_code)] // Provider adapters consume this crate-private API in a later task.
    pub(crate) fn put(
        &self,
        identifier: &str,
        value: Zeroizing<Vec<u8>>,
    ) -> Result<(), VaultError> {
        validate_identifier(identifier)?;
        if value.len() > MAX_RECORD_VALUE_BYTES {
            return Err(VaultError::LimitExceeded("credential value"));
        }
        let material = self.unlocked.as_ref().ok_or(VaultError::Locked)?;
        let lock_file = self.acquire_lock()?;
        let mut records = self.read_latest_records(material)?;
        if !records.0.contains_key(identifier) && records.0.len() == MAX_RECORDS {
            return Err(VaultError::LimitExceeded("credential records"));
        }
        records.0.insert(identifier.to_owned(), value.to_vec());
        let envelope = encrypt_envelope(&records, &material.key, material.salt)?;
        let result = self.atomic_write(&envelope, true, "update");
        drop(lock_file);
        result
    }

    /// Returns whether a record existed. Removing a missing record is a safe,
    /// idempotent no-op and does not rewrite the file.
    #[allow(dead_code)] // Provider adapters consume this crate-private API in a later task.
    pub(crate) fn remove(&self, identifier: &str) -> Result<bool, VaultError> {
        validate_identifier(identifier)?;
        let material = self.unlocked.as_ref().ok_or(VaultError::Locked)?;
        let lock_file = self.acquire_lock()?;
        let mut records = self.read_latest_records(material)?;
        let removed = records.0.remove(identifier).is_some();
        let result = if removed {
            let envelope = encrypt_envelope(&records, &material.key, material.salt)?;
            self.atomic_write(&envelope, true, "update")
        } else {
            Ok(())
        };
        drop(lock_file);
        result.map(|()| removed)
    }

    /// Re-encrypts the latest records using a fresh salt, nonce, and derived
    /// key. Before the rename, failures preserve the old passphrase. A failure
    /// after rename is reported as `CommitUncertain` and this handle adopts the
    /// new key because the new file is already visible.
    pub(crate) fn change_passphrase(
        &mut self,
        current_passphrase: Zeroizing<Vec<u8>>,
        new_passphrase: Zeroizing<Vec<u8>>,
    ) -> Result<(), VaultError> {
        if self.unlocked.is_none() {
            return Err(VaultError::Locked);
        }
        validate_passphrase(&current_passphrase)?;
        validate_passphrase(&new_passphrase)?;
        let lock_file = self.acquire_lock()?;
        // Verification and rewrite deliberately share this one lock so another
        // process cannot change the password between those two steps.
        let old_envelope = self.read_envelope()?;
        let old_parsed = parse_envelope(&old_envelope)?;
        let old_key = derive_key(&current_passphrase, &old_parsed.salt)?;
        let old_plaintext = decrypt_envelope(&old_parsed, &old_key)?;
        let records = decode_records(&old_plaintext)?;
        let salt = random_array::<SALT_LEN>();
        let key = derive_key(&new_passphrase, &salt)?;
        let envelope = encrypt_envelope(&records, &key, salt)?;
        let result = self.atomic_write(&envelope, true, "passphrase change");
        if write_reached_rename(&result) {
            self.unlocked = Some(UnlockMaterial { key, salt });
        }
        drop(lock_file);
        result
    }

    /// Permanently removes the complete credential vault. This is the only
    /// recovery path for a corrupt or unknown vault; `create` never invokes it
    /// and always refuses to overwrite any existing entry.
    pub(crate) fn reset(&mut self) -> Result<(), VaultError> {
        let lock_file = self.acquire_lock()?;
        let metadata = match fs::symlink_metadata(&self.vault_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.unlocked = None;
                drop(lock_file);
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() {
            return Err(VaultError::InvalidFileType);
        }
        fs::remove_file(&self.vault_path)?;
        self.unlocked = None;
        let parent = self
            .vault_path
            .parent()
            .ok_or_else(|| VaultError::Io(io::Error::other("vault has no parent directory")))?;
        let directory = File::open(parent).map_err(|source| VaultError::CommitUncertain {
            operation: "reset",
            source,
        })?;
        let result = directory
            .sync_all()
            .map_err(|source| VaultError::CommitUncertain {
                operation: "reset",
                source,
            });
        drop(lock_file);
        result
    }

    #[allow(dead_code)] // Shared only by the crate-private credential access methods above.
    fn read_latest_records(&self, material: &UnlockMaterial) -> Result<VaultRecords, VaultError> {
        let envelope = self.read_envelope()?;
        let parsed = parse_envelope(&envelope)?;
        if parsed.salt != material.salt {
            return Err(VaultError::StaleUnlock);
        }
        let plaintext = decrypt_envelope(&parsed, &material.key)?;
        decode_records(&plaintext)
    }

    fn acquire_lock(&self) -> Result<File, VaultError> {
        let parent = self
            .lock_path
            .parent()
            .ok_or_else(|| VaultError::Io(io::Error::other("vault has no parent directory")))?;
        let parent_metadata = fs::metadata(parent)?;
        if !parent_metadata.is_dir() {
            return Err(VaultError::InvalidFileType);
        }

        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_create_options(&mut options);
        let file = options.open(&self.lock_path)?;
        enforce_private_mode(&file)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn read_envelope(&self) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let metadata = fs::symlink_metadata(&self.vault_path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                VaultError::Absent
            } else {
                error.into()
            }
        })?;
        if !metadata.file_type().is_file() {
            return Err(VaultError::InvalidFileType);
        }
        if !has_private_mode(&metadata) {
            return Err(VaultError::UnsafePermissions);
        }
        if metadata.len() > MAX_VAULT_FILE_BYTES as u64 {
            return Err(VaultError::LimitExceeded("vault file"));
        }

        let mut options = OpenOptions::new();
        options.read(true);
        set_no_follow(&mut options);
        let mut file = options.open(&self.vault_path)?;
        let opened_metadata = file.metadata()?;
        if !opened_metadata.file_type().is_file() {
            return Err(VaultError::InvalidFileType);
        }
        if !has_private_mode(&opened_metadata) {
            return Err(VaultError::UnsafePermissions);
        }
        if opened_metadata.len() > MAX_VAULT_FILE_BYTES as u64 {
            return Err(VaultError::LimitExceeded("vault file"));
        }
        let capacity = usize::try_from(opened_metadata.len())
            .unwrap_or(MAX_VAULT_FILE_BYTES)
            .min(MAX_VAULT_FILE_BYTES);
        let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
        std::io::Read::by_ref(&mut file)
            .take((MAX_VAULT_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_VAULT_FILE_BYTES {
            return Err(VaultError::LimitExceeded("vault file"));
        }
        Ok(bytes)
    }

    fn atomic_write(
        &self,
        bytes: &[u8],
        replace: bool,
        operation: &'static str,
    ) -> Result<(), VaultError> {
        if bytes.len() > MAX_VAULT_FILE_BYTES {
            return Err(VaultError::LimitExceeded("vault file"));
        }
        let parent = self
            .vault_path
            .parent()
            .ok_or_else(|| VaultError::Io(io::Error::other("vault has no parent directory")))?;
        let mut random = [0u8; 16];
        let mut temporary = None;
        for _ in 0..16 {
            OsRng.fill_bytes(&mut random);
            let name = format!(".mux.vault.{}.tmp", encode_hex(&random));
            let path = parent.join(name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            set_private_create_options(&mut options);
            match options.open(&path) {
                Ok(file) => {
                    temporary = Some((path, file));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        random.zeroize();
        let (temporary_path, mut temporary_file) = temporary.ok_or_else(|| {
            VaultError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate vault temporary file",
            ))
        })?;

        let before_rename = (|| -> Result<(), VaultError> {
            enforce_private_mode(&temporary_file)?;
            temporary_file.write_all(bytes)?;
            test_fail_before_temp_sync()?;
            temporary_file.sync_all()?;
            drop(temporary_file);
            test_fail_before_rename()?;
            if !replace {
                match fs::symlink_metadata(&self.vault_path) {
                    Ok(_) => return Err(VaultError::AlreadyExists),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            fs::rename(&temporary_path, &self.vault_path)?;
            Ok(())
        })();

        if let Err(error) = before_rename {
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }

        if let Err(source) = test_fail_after_rename() {
            return Err(VaultError::CommitUncertain { operation, source });
        }
        let directory = File::open(parent)
            .map_err(|source| VaultError::CommitUncertain { operation, source })?;
        if let Err(source) = test_fail_directory_sync() {
            return Err(VaultError::CommitUncertain { operation, source });
        }
        if let Err(source) = directory.sync_all() {
            return Err(VaultError::CommitUncertain { operation, source });
        }
        Ok(())
    }
}

impl Drop for CredentialVault {
    fn drop(&mut self) {
        self.lock();
    }
}

impl std::fmt::Debug for CredentialVault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialVault")
            .field("vault_path", &self.vault_path)
            .field("lock_path", &self.lock_path)
            .field("unlocked", &self.unlocked.is_some())
            .finish()
    }
}

struct ParsedEnvelope<'a> {
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    aad: &'a [u8],
    ciphertext: &'a [u8],
}

fn parse_envelope(bytes: &[u8]) -> Result<ParsedEnvelope<'_>, VaultError> {
    if bytes.len() > MAX_VAULT_FILE_BYTES {
        return Err(VaultError::LimitExceeded("vault file"));
    }
    if bytes.len() < HEADER_LEN + TAG_LEN {
        return Err(VaultError::Malformed);
    }
    if &bytes[0..8] != MAGIC {
        return Err(VaultError::UnsupportedFormat);
    }
    if read_u16(bytes, 8)? != FORMAT_VERSION
        || bytes[10] != KDF_ARGON2ID
        || bytes[11] != ARGON2_VERSION
        || read_u32(bytes, 12)? != ARGON2_MEMORY_KIB
        || read_u32(bytes, 16)? != ARGON2_ITERATIONS
        || read_u32(bytes, 20)? != ARGON2_PARALLELISM
        || read_u16(bytes, 24)? != DERIVED_KEY_LEN
    {
        return Err(VaultError::UnsupportedFormat);
    }

    let salt: [u8; SALT_LEN] = bytes[26..42]
        .try_into()
        .map_err(|_| VaultError::Malformed)?;
    let nonce: [u8; NONCE_LEN] = bytes[42..66]
        .try_into()
        .map_err(|_| VaultError::Malformed)?;
    let ciphertext_len = usize::try_from(read_u32(bytes, 66)?)
        .map_err(|_| VaultError::LimitExceeded("ciphertext"))?;
    if !(TAG_LEN..=MAX_PLAINTEXT_BYTES + TAG_LEN).contains(&ciphertext_len) {
        return Err(VaultError::LimitExceeded("ciphertext"));
    }
    let expected_len = HEADER_LEN
        .checked_add(ciphertext_len)
        .ok_or(VaultError::LimitExceeded("vault file"))?;
    if bytes.len() != expected_len {
        return Err(VaultError::Malformed);
    }
    Ok(ParsedEnvelope {
        salt,
        nonce,
        aad: &bytes[..HEADER_LEN],
        ciphertext: &bytes[HEADER_LEN..],
    })
}

fn encrypt_envelope(
    records: &VaultRecords,
    key: &[u8; 32],
    salt: [u8; SALT_LEN],
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let nonce = random_array::<NONCE_LEN>();
    let mut ciphertext = encode_records(records)?;
    let ciphertext_len = ciphertext
        .len()
        .checked_add(TAG_LEN)
        .ok_or(VaultError::LimitExceeded("ciphertext"))?;
    if ciphertext_len > MAX_PLAINTEXT_BYTES + TAG_LEN {
        return Err(VaultError::LimitExceeded("plaintext"));
    }
    let ciphertext_len_u32 =
        u32::try_from(ciphertext_len).map_err(|_| VaultError::LimitExceeded("ciphertext"))?;
    let header = encode_header(salt, nonce, ciphertext_len_u32);
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| VaultError::Cryptography)?;
    cipher
        .encrypt_in_place(XNonce::from_slice(&nonce), &header, &mut *ciphertext)
        .map_err(|_| VaultError::Cryptography)?;

    let mut envelope = Zeroizing::new(Vec::with_capacity(HEADER_LEN + ciphertext.len()));
    envelope.extend_from_slice(&header);
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

fn decrypt_envelope(
    parsed: &ParsedEnvelope<'_>,
    key: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| VaultError::Cryptography)?;
    let mut plaintext = Zeroizing::new(parsed.ciphertext.to_vec());
    cipher
        .decrypt_in_place(
            XNonce::from_slice(&parsed.nonce),
            parsed.aad,
            &mut *plaintext,
        )
        .map_err(|_| VaultError::AuthenticationFailed)?;
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(VaultError::LimitExceeded("plaintext"));
    }
    Ok(plaintext)
}

fn encode_header(
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
    ciphertext_len: u32,
) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[0..8].copy_from_slice(MAGIC);
    header[8..10].copy_from_slice(&FORMAT_VERSION.to_be_bytes());
    header[10] = KDF_ARGON2ID;
    header[11] = ARGON2_VERSION;
    header[12..16].copy_from_slice(&ARGON2_MEMORY_KIB.to_be_bytes());
    header[16..20].copy_from_slice(&ARGON2_ITERATIONS.to_be_bytes());
    header[20..24].copy_from_slice(&ARGON2_PARALLELISM.to_be_bytes());
    header[24..26].copy_from_slice(&DERIVED_KEY_LEN.to_be_bytes());
    header[26..42].copy_from_slice(&salt);
    header[42..66].copy_from_slice(&nonce);
    header[66..70].copy_from_slice(&ciphertext_len.to_be_bytes());
    header
}

fn derive_key(passphrase: &[u8], salt: &[u8; SALT_LEN]) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    validate_passphrase(passphrase)?;
    #[cfg(test)]
    KDF_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(DERIVED_KEY_LEN as usize),
    )
    .map_err(|_| VaultError::Cryptography)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(passphrase, salt, &mut *key)
        .map_err(|_| VaultError::Cryptography)?;
    Ok(key)
}

fn encode_records(records: &VaultRecords) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if records.0.len() > MAX_RECORDS {
        return Err(VaultError::LimitExceeded("credential records"));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    bytes.extend_from_slice(
        &u32::try_from(records.0.len())
            .map_err(|_| VaultError::LimitExceeded("credential records"))?
            .to_be_bytes(),
    );
    for (identifier, value) in &records.0 {
        validate_identifier(identifier)?;
        if value.len() > MAX_RECORD_VALUE_BYTES {
            return Err(VaultError::LimitExceeded("credential value"));
        }
        bytes.extend_from_slice(
            &u16::try_from(identifier.len())
                .map_err(|_| VaultError::LimitExceeded("credential identifier"))?
                .to_be_bytes(),
        );
        bytes.extend_from_slice(
            &u32::try_from(value.len())
                .map_err(|_| VaultError::LimitExceeded("credential value"))?
                .to_be_bytes(),
        );
        bytes.extend_from_slice(identifier.as_bytes());
        bytes.extend_from_slice(value);
        if bytes.len() > MAX_PLAINTEXT_BYTES {
            return Err(VaultError::LimitExceeded("plaintext"));
        }
    }
    Ok(bytes)
}

fn decode_records(bytes: &[u8]) -> Result<VaultRecords, VaultError> {
    if bytes.len() > MAX_PLAINTEXT_BYTES || bytes.len() < 4 {
        return Err(VaultError::Malformed);
    }
    let count = usize::try_from(read_u32(bytes, 0)?)
        .map_err(|_| VaultError::LimitExceeded("credential records"))?;
    if count > MAX_RECORDS {
        return Err(VaultError::LimitExceeded("credential records"));
    }
    let mut cursor = 4usize;
    let mut records = VaultRecords::default();
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let identifier_len = usize::from(read_u16(bytes, cursor)?);
        cursor = cursor.checked_add(2).ok_or(VaultError::Malformed)?;
        let value_len = usize::try_from(read_u32(bytes, cursor)?)
            .map_err(|_| VaultError::LimitExceeded("credential value"))?;
        cursor = cursor.checked_add(4).ok_or(VaultError::Malformed)?;
        if !(1..=MAX_RECORD_KEY_BYTES).contains(&identifier_len)
            || value_len > MAX_RECORD_VALUE_BYTES
        {
            return Err(VaultError::Malformed);
        }
        let identifier_end = cursor
            .checked_add(identifier_len)
            .filter(|end| *end <= bytes.len())
            .ok_or(VaultError::Malformed)?;
        let identifier = std::str::from_utf8(&bytes[cursor..identifier_end])
            .map_err(|_| VaultError::Malformed)?;
        validate_identifier(identifier).map_err(|_| VaultError::Malformed)?;
        cursor = identifier_end;
        let value_end = cursor
            .checked_add(value_len)
            .filter(|end| *end <= bytes.len())
            .ok_or(VaultError::Malformed)?;
        if previous.as_deref().is_some_and(|item| item >= identifier) {
            return Err(VaultError::Malformed);
        }
        records
            .0
            .insert(identifier.to_owned(), bytes[cursor..value_end].to_vec());
        previous = Some(identifier.to_owned());
        cursor = value_end;
    }
    if cursor != bytes.len() || records.0.len() != count {
        return Err(VaultError::Malformed);
    }
    Ok(records)
}

fn validate_passphrase(passphrase: &[u8]) -> Result<(), VaultError> {
    if passphrase.is_empty() || passphrase.len() > MAX_PASSPHRASE_BYTES {
        return Err(VaultError::InvalidPassphrase);
    }
    Ok(())
}

fn validate_identifier(identifier: &str) -> Result<(), VaultError> {
    let bytes = identifier.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_RECORD_KEY_BYTES || !bytes[0].is_ascii_lowercase() {
        return Err(VaultError::InvalidIdentifier);
    }
    if bytes
        .iter()
        .skip(1)
        .any(|byte| !matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b':' | b'/' | b'-'))
    {
        return Err(VaultError::InvalidIdentifier);
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, VaultError> {
    let end = offset.checked_add(2).ok_or(VaultError::Malformed)?;
    let encoded: [u8; 2] = bytes
        .get(offset..end)
        .ok_or(VaultError::Malformed)?
        .try_into()
        .map_err(|_| VaultError::Malformed)?;
    Ok(u16::from_be_bytes(encoded))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, VaultError> {
    let end = offset.checked_add(4).ok_or(VaultError::Malformed)?;
    let encoded: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(VaultError::Malformed)?
        .try_into()
        .map_err(|_| VaultError::Malformed)?;
    Ok(u32::from_be_bytes(encoded))
}

fn random_array<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn write_reached_rename(result: &Result<(), VaultError>) -> bool {
    result.is_ok() || matches!(result, Err(VaultError::CommitUncertain { .. }))
}

#[cfg(unix)]
fn set_private_create_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn set_private_create_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn set_no_follow(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn enforce_private_mode(file: &File) -> Result<(), VaultError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn enforce_private_mode(_file: &File) -> Result<(), VaultError> {
    Ok(())
}

#[cfg(unix)]
fn has_private_mode(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777 == 0o600
}

#[cfg(not(unix))]
fn has_private_mode(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(test)]
fn test_fail_before_temp_sync() -> Result<(), VaultError> {
    if WRITE_FAILPOINT.load(std::sync::atomic::Ordering::SeqCst) == 1 {
        return Err(VaultError::Io(io::Error::other(
            "injected failure before temporary file sync",
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn test_fail_before_temp_sync() -> Result<(), VaultError> {
    Ok(())
}

#[cfg(test)]
fn test_fail_before_rename() -> Result<(), VaultError> {
    if WRITE_FAILPOINT.load(std::sync::atomic::Ordering::SeqCst) == 2 {
        return Err(VaultError::Io(io::Error::other(
            "injected failure before rename",
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn test_fail_before_rename() -> Result<(), VaultError> {
    Ok(())
}

#[cfg(test)]
fn test_fail_after_rename() -> Result<(), io::Error> {
    if WRITE_FAILPOINT.load(std::sync::atomic::Ordering::SeqCst) == 3 {
        return Err(io::Error::other("injected failure after rename"));
    }
    Ok(())
}

#[cfg(not(test))]
fn test_fail_after_rename() -> Result<(), io::Error> {
    Ok(())
}

#[cfg(test)]
fn test_fail_directory_sync() -> Result<(), io::Error> {
    if WRITE_FAILPOINT.load(std::sync::atomic::Ordering::SeqCst) == 4 {
        return Err(io::Error::other("injected directory sync failure"));
    }
    Ok(())
}

#[cfg(not(test))]
fn test_fail_directory_sync() -> Result<(), io::Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().expect("test serialization lock")
    }

    fn secret(value: &str) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(value.as_bytes().to_vec())
    }

    fn vault_in(temp: &tempfile::TempDir) -> CredentialVault {
        CredentialVault::for_database(&temp.path().join("mux.db"))
    }

    #[test]
    fn lifecycle_round_trip_starts_locked_after_restart() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        assert_eq!(vault.status(), VaultStatus::Absent);
        vault
            .create(secret("correct horse battery staple"))
            .unwrap();
        assert_eq!(vault.status(), VaultStatus::Unlocked);
        vault
            .put("gmail/account-1/refresh-token", secret("secret-value"))
            .unwrap();
        vault.lock();
        assert_eq!(vault.status(), VaultStatus::Locked);
        assert!(matches!(
            vault.get("gmail/account-1/refresh-token"),
            Err(VaultError::Locked)
        ));
        drop(vault);

        let mut restarted = vault_in(&temp);
        assert_eq!(restarted.status(), VaultStatus::Locked);
        restarted
            .unlock(secret("correct horse battery staple"))
            .unwrap();
        let stored = restarted
            .get("gmail/account-1/refresh-token")
            .unwrap()
            .unwrap();
        assert_eq!(stored.as_slice(), b"secret-value");
    }

    #[test]
    fn wrong_passphrase_leaves_vault_locked_and_does_not_recreate() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("right passphrase")).unwrap();
        let before = fs::read(&vault.vault_path).unwrap();
        vault.lock();
        assert!(matches!(
            vault.unlock(secret("wrong passphrase")),
            Err(VaultError::AuthenticationFailed)
        ));
        assert_eq!(vault.status(), VaultStatus::Locked);
        assert_eq!(fs::read(&vault.vault_path).unwrap(), before);
    }

    #[test]
    fn put_get_remove_and_change_passphrase_work() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("old passphrase")).unwrap();
        vault
            .put("smtp/account-1/password", secret("alpha"))
            .unwrap();
        vault
            .put("smtp/account-1/password", secret("beta"))
            .unwrap();
        let stored = vault.get("smtp/account-1/password").unwrap().unwrap();
        assert_eq!(stored.as_slice(), b"beta");
        assert!(!vault.remove("smtp/missing/password").unwrap());
        vault
            .change_passphrase(secret("old passphrase"), secret("new passphrase"))
            .unwrap();
        vault.lock();
        assert!(matches!(
            vault.unlock(secret("old passphrase")),
            Err(VaultError::AuthenticationFailed)
        ));
        vault.unlock(secret("new passphrase")).unwrap();
        assert!(vault.remove("smtp/account-1/password").unwrap());
        assert_eq!(vault.get("smtp/account-1/password").unwrap(), None);
    }

    #[test]
    fn passphrase_change_verifies_current_password_without_a_race_window() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("current passphrase")).unwrap();
        vault.put("test/value", secret("credential")).unwrap();
        let before = fs::read(&vault.vault_path).unwrap();
        assert!(matches!(
            vault.change_passphrase(secret("wrong current"), secret("new passphrase")),
            Err(VaultError::AuthenticationFailed)
        ));
        assert_eq!(fs::read(&vault.vault_path).unwrap(), before);
        let stored = vault.get("test/value").unwrap().unwrap();
        assert_eq!(stored.as_slice(), b"credential");
    }

    #[test]
    fn every_write_uses_a_fresh_nonce_and_passphrase_change_uses_fresh_salt() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("old passphrase")).unwrap();
        let first = fs::read(&vault.vault_path).unwrap();
        vault.put("test/value", secret("same-value")).unwrap();
        let second = fs::read(&vault.vault_path).unwrap();
        assert_ne!(&first[42..66], &second[42..66]);
        assert_eq!(&first[26..42], &second[26..42]);
        vault
            .change_passphrase(secret("old passphrase"), secret("new passphrase"))
            .unwrap();
        let third = fs::read(&vault.vault_path).unwrap();
        assert_ne!(&second[26..42], &third[26..42]);
        assert_ne!(&second[42..66], &third[42..66]);
    }

    #[test]
    fn encrypted_file_never_contains_plaintext_secret_or_passphrase() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault
            .create(secret("distinct-passphrase-sentinel"))
            .unwrap();
        vault
            .put(
                "imap/account-1/password",
                secret("distinct-secret-value-sentinel"),
            )
            .unwrap();
        let bytes = fs::read(&vault.vault_path).unwrap();
        assert!(!bytes
            .windows(b"distinct-passphrase-sentinel".len())
            .any(|window| window == b"distinct-passphrase-sentinel"));
        assert!(!bytes
            .windows(b"distinct-secret-value-sentinel".len())
            .any(|window| window == b"distinct-secret-value-sentinel"));
    }

    #[test]
    fn malformed_unsupported_trailing_and_oversized_inputs_fail_before_kdf() {
        let _guard = serial();
        let baseline = KDF_CALLS.load(std::sync::atomic::Ordering::SeqCst);

        assert!(matches!(
            parse_envelope(b"short"),
            Err(VaultError::Malformed)
        ));
        let mut unsupported = vec![0u8; HEADER_LEN + TAG_LEN];
        unsupported[..8].copy_from_slice(MAGIC);
        unsupported[8..10].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(
            parse_envelope(&unsupported),
            Err(VaultError::UnsupportedFormat)
        ));

        let header = encode_header([1; SALT_LEN], [2; NONCE_LEN], TAG_LEN as u32);
        let mut trailing = Vec::from(header);
        trailing.extend_from_slice(&[0u8; TAG_LEN + 1]);
        assert!(matches!(
            parse_envelope(&trailing),
            Err(VaultError::Malformed)
        ));
        let oversized = vec![0u8; MAX_VAULT_FILE_BYTES + 1];
        assert!(matches!(
            parse_envelope(&oversized),
            Err(VaultError::LimitExceeded("vault file"))
        ));
        assert_eq!(
            KDF_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            baseline
        );
    }

    #[test]
    fn oversized_file_is_rejected_before_read_allocation_or_kdf() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("passphrase")).unwrap();
        vault.lock();
        OpenOptions::new()
            .write(true)
            .open(&vault.vault_path)
            .unwrap()
            .set_len((MAX_VAULT_FILE_BYTES + 1) as u64)
            .unwrap();
        let baseline = KDF_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            vault.unlock(secret("passphrase")),
            Err(VaultError::LimitExceeded("vault file"))
        ));
        assert_eq!(
            KDF_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            baseline
        );
    }

    #[test]
    fn v1_header_encodes_only_the_canonical_fixed_parameters() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("passphrase")).unwrap();
        let bytes = fs::read(&vault.vault_path).unwrap();
        assert_eq!(&bytes[0..8], MAGIC);
        assert_eq!(read_u16(&bytes, 8).unwrap(), FORMAT_VERSION);
        assert_eq!(bytes[10], KDF_ARGON2ID);
        assert_eq!(bytes[11], ARGON2_VERSION);
        assert_eq!(read_u32(&bytes, 12).unwrap(), ARGON2_MEMORY_KIB);
        assert_eq!(read_u32(&bytes, 16).unwrap(), ARGON2_ITERATIONS);
        assert_eq!(read_u32(&bytes, 20).unwrap(), ARGON2_PARALLELISM);
        assert_eq!(read_u16(&bytes, 24).unwrap(), DERIVED_KEY_LEN);
        let ciphertext_len = read_u32(&bytes, 66).unwrap() as usize;
        assert_eq!(bytes.len(), HEADER_LEN + ciphertext_len);
        assert!(parse_envelope(&bytes).is_ok());
    }

    #[test]
    fn status_maps_structural_corruption_to_unavailable_without_running_kdf() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("passphrase")).unwrap();
        vault.lock();
        let mut bytes = fs::read(&vault.vault_path).unwrap();
        bytes[8..10].copy_from_slice(&99u16.to_be_bytes());
        fs::write(&vault.vault_path, bytes).unwrap();
        let baseline = KDF_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(vault.status(), VaultStatus::Unavailable);
        assert_eq!(
            KDF_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            baseline
        );
    }

    #[test]
    fn corrupt_vault_is_never_recreated_without_explicit_reset() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        fs::write(&vault.vault_path, b"corrupt-existing-vault").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&vault.vault_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(vault.status(), VaultStatus::Unavailable);
        assert!(matches!(
            vault.create(secret("passphrase")),
            Err(VaultError::AlreadyExists)
        ));
        assert_eq!(
            fs::read(&vault.vault_path).unwrap(),
            b"corrupt-existing-vault"
        );
        vault.reset().unwrap();
        assert_eq!(vault.status(), VaultStatus::Absent);
        vault.reset().unwrap();
        assert_eq!(vault.status(), VaultStatus::Absent);
        vault.create(secret("passphrase")).unwrap();
        assert_eq!(vault.status(), VaultStatus::Unlocked);
    }

    #[test]
    fn authenticated_header_and_ciphertext_tampering_is_rejected() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("passphrase")).unwrap();
        vault.lock();
        let original = fs::read(&vault.vault_path).unwrap();

        let mut header_tamper = original.clone();
        header_tamper[42] ^= 1;
        fs::write(&vault.vault_path, header_tamper).unwrap();
        assert!(matches!(
            vault.unlock(secret("passphrase")),
            Err(VaultError::AuthenticationFailed)
        ));

        let mut ciphertext_tamper = original;
        ciphertext_tamper[HEADER_LEN] ^= 1;
        fs::write(&vault.vault_path, ciphertext_tamper).unwrap();
        assert!(matches!(
            vault.unlock(secret("passphrase")),
            Err(VaultError::AuthenticationFailed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn vault_and_permanent_lock_file_are_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("passphrase")).unwrap();
        assert_eq!(
            fs::metadata(&vault.vault_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&vault.lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        vault.put("test/value", secret("value")).unwrap();
        assert!(vault.lock_path.exists());
        assert_eq!(
            fs::metadata(&vault.vault_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn two_handles_merge_updates_and_detect_passphrase_change() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut first = vault_in(&temp);
        first.create(secret("passphrase")).unwrap();
        let mut second = vault_in(&temp);
        second.unlock(secret("passphrase")).unwrap();

        first.put("test/first", secret("one")).unwrap();
        second.put("test/second", secret("two")).unwrap();
        let stored = first.get("test/second").unwrap().unwrap();
        assert_eq!(stored.as_slice(), b"two");

        first
            .change_passphrase(secret("passphrase"), secret("new passphrase"))
            .unwrap();
        assert!(matches!(
            second.put("test/stale", secret("three")),
            Err(VaultError::StaleUnlock)
        ));
        assert!(matches!(
            second.get("test/first"),
            Err(VaultError::StaleUnlock)
        ));
    }

    #[test]
    fn concurrent_handles_serialize_read_modify_write_without_lost_updates() {
        use std::sync::{Arc, Barrier};

        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut first = vault_in(&temp);
        first.create(secret("passphrase")).unwrap();
        let mut second = vault_in(&temp);
        second.unlock(secret("passphrase")).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let first_thread = std::thread::spawn(move || {
            first_barrier.wait();
            first.put("test/first", secret("one"))
        });
        let second_thread = std::thread::spawn(move || {
            barrier.wait();
            second.put("test/second", secret("two"))
        });
        first_thread.join().unwrap().unwrap();
        second_thread.join().unwrap().unwrap();

        let mut verifier = vault_in(&temp);
        verifier.unlock(secret("passphrase")).unwrap();
        assert_eq!(
            verifier.get("test/first").unwrap().unwrap().as_slice(),
            b"one"
        );
        assert_eq!(
            verifier.get("test/second").unwrap().unwrap().as_slice(),
            b"two"
        );
    }

    #[test]
    fn pre_rename_failure_preserves_old_passphrase_and_post_rename_is_uncertain() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("old passphrase")).unwrap();

        WRITE_FAILPOINT.store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            vault.change_passphrase(secret("old passphrase"), secret("not committed")),
            Err(VaultError::Io(_))
        ));
        WRITE_FAILPOINT.store(0, std::sync::atomic::Ordering::SeqCst);
        vault.lock();
        vault.unlock(secret("old passphrase")).unwrap();

        WRITE_FAILPOINT.store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            vault.change_passphrase(secret("old passphrase"), secret("still not committed")),
            Err(VaultError::Io(_))
        ));
        WRITE_FAILPOINT.store(0, std::sync::atomic::Ordering::SeqCst);
        vault.lock();
        vault.unlock(secret("old passphrase")).unwrap();

        WRITE_FAILPOINT.store(3, std::sync::atomic::Ordering::SeqCst);
        let error = vault
            .change_passphrase(secret("old passphrase"), secret("committed maybe"))
            .unwrap_err();
        WRITE_FAILPOINT.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(error, VaultError::CommitUncertain { .. }));
        vault.lock();
        vault.unlock(secret("committed maybe")).unwrap();

        WRITE_FAILPOINT.store(4, std::sync::atomic::Ordering::SeqCst);
        let error = vault
            .change_passphrase(secret("committed maybe"), secret("directory sync maybe"))
            .unwrap_err();
        WRITE_FAILPOINT.store(0, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(error, VaultError::CommitUncertain { .. }));
        vault.lock();
        vault.unlock(secret("directory sync maybe")).unwrap();
    }

    #[test]
    fn errors_and_debug_output_do_not_include_secrets() {
        let _guard = serial();
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("private-passphrase-sentinel")).unwrap();
        vault.lock();
        let error = vault.unlock(secret("wrong-private-sentinel")).unwrap_err();
        let output = format!("{error:?} {error}");
        assert!(!output.contains("private-passphrase-sentinel"));
        assert!(!output.contains("wrong-private-sentinel"));
        let vault_debug = format!("{vault:?}");
        assert!(!vault_debug.contains("private-passphrase-sentinel"));
        assert!(!vault_debug.contains("wrong-private-sentinel"));
    }

    #[test]
    fn identifier_and_value_bounds_are_enforced() {
        let _guard = serial();
        assert!(validate_identifier("gmail/account-1/token").is_ok());
        for invalid in ["", "Gmail/token", "gmail token", "gmail/@token"] {
            assert!(matches!(
                validate_identifier(invalid),
                Err(VaultError::InvalidIdentifier)
            ));
        }
        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("passphrase")).unwrap();
        assert!(matches!(
            vault.put(
                "test/value",
                Zeroizing::new(vec![0; MAX_RECORD_VALUE_BYTES + 1])
            ),
            Err(VaultError::LimitExceeded("credential value"))
        ));
    }

    #[test]
    fn exact_passphrase_identifier_record_and_plaintext_boundaries_are_enforced() {
        let _guard = serial();
        assert!(matches!(
            validate_passphrase(&[]),
            Err(VaultError::InvalidPassphrase)
        ));
        assert!(validate_passphrase(&vec![b'p'; MAX_PASSPHRASE_BYTES]).is_ok());
        assert!(matches!(
            validate_passphrase(&vec![b'p'; MAX_PASSPHRASE_BYTES + 1]),
            Err(VaultError::InvalidPassphrase)
        ));

        assert!(validate_identifier("a").is_ok());
        let maximum_identifier = "a".repeat(MAX_RECORD_KEY_BYTES);
        assert!(validate_identifier(&maximum_identifier).is_ok());
        assert!(matches!(
            validate_identifier(&"a".repeat(MAX_RECORD_KEY_BYTES + 1)),
            Err(VaultError::InvalidIdentifier)
        ));

        let mut maximum_records = VaultRecords::default();
        for index in 0..MAX_RECORDS {
            maximum_records
                .0
                .insert(format!("record/{index:03}"), Vec::new());
        }
        let encoded = encode_records(&maximum_records).unwrap();
        assert_eq!(decode_records(&encoded).unwrap().0.len(), MAX_RECORDS);
        maximum_records
            .0
            .insert("record/overflow".to_string(), Vec::new());
        assert!(matches!(
            encode_records(&maximum_records),
            Err(VaultError::LimitExceeded("credential records"))
        ));

        let too_many_records = u32::try_from(MAX_RECORDS + 1).unwrap().to_be_bytes();
        assert!(matches!(
            decode_records(&too_many_records),
            Err(VaultError::LimitExceeded("credential records"))
        ));
        assert!(matches!(
            decode_records(&vec![0; MAX_PLAINTEXT_BYTES + 1]),
            Err(VaultError::Malformed)
        ));

        let temp = tempfile::tempdir().unwrap();
        let mut vault = vault_in(&temp);
        vault.create(secret("passphrase")).unwrap();
        vault
            .put(
                "test/maximum-value",
                Zeroizing::new(vec![7; MAX_RECORD_VALUE_BYTES]),
            )
            .unwrap();
        assert_eq!(
            vault.get("test/maximum-value").unwrap().unwrap().len(),
            MAX_RECORD_VALUE_BYTES
        );
    }
}
