//! Provider credential storage backed by the macOS Keychain.
//!
//! The login keychain is already unlocked by the time a person reaches their
//! desktop, so Mux needs no password of its own and no lock lifecycle. Items are
//! written with `SecItemAdd`, which grants no per-application ACL: any process
//! running as this user can read them, exactly like the mailbox database beside
//! them. Requiring presence per read would need `kSecAttrAccessControl`.

use std::path::Path;

use security_framework::os::macos::keychain::SecKeychain;
use thiserror::Error;
use zeroize::Zeroizing;

/// One keychain service groups every Mux credential; the identifier is the account.
const SERVICE: &str = "com.jordanmatelsky.mux.credentials";
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_SECRET_BYTES: usize = 64 * 1024;
/// Read once at startup to tell "no credential yet" apart from "keychain broken".
const PROBE_IDENTIFIER: &str = "store.probe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialStoreStatus {
    /// The keychain answered, so credentials can be read and written.
    Available,
    /// The keychain could not be reached; accounts cannot connect.
    Unavailable,
}

#[derive(Debug, Error)]
pub(crate) enum CredentialError {
    #[error("Credential identifier is invalid")]
    InvalidIdentifier,
    #[error("Credential {0} is too large")]
    LimitExceeded(&'static str),
    #[error("The system keychain is unavailable")]
    Unavailable,
}

#[derive(Clone)]
pub(crate) struct CredentialStore {
    service: String,
    /// The keychain to use. Production uses the default (login) keychain; tests
    /// pass a throwaway one so a test run never touches real credentials — and
    /// never triggers the access prompt an unstable dev signature would cause.
    keychain: SecKeychain,
}

impl CredentialStore {
    pub(crate) fn for_database(_database_path: &Path) -> Result<Self, CredentialError> {
        Ok(Self {
            service: SERVICE.to_owned(),
            keychain: SecKeychain::default().map_err(|_| CredentialError::Unavailable)?,
        })
    }

    /// A throwaway keychain file, used by tests so nothing reaches the login keychain.
    #[cfg(test)]
    pub(crate) fn temporary(path: &Path) -> Self {
        use security_framework::os::macos::keychain::CreateOptions;
        // This keychain is thrown away with the test's temporary directory and
        // is only ever reached through the handle below, so its password
        // protects nothing. It is derived rather than written down because a
        // password literal in source is indistinguishable from a real
        // credential to anything scanning for them.
        let throwaway = format!("mux-test-{}", path.display());
        Self {
            service: SERVICE.to_owned(),
            keychain: CreateOptions::new()
                .password(&throwaway)
                .create(path)
                .expect("temporary keychain"),
        }
    }

    pub(crate) fn status(&self) -> CredentialStoreStatus {
        // A missing probe item is the normal case and still proves the keychain
        // answered; any other failure means it did not.
        match self
            .keychain
            .find_generic_password(&self.service, PROBE_IDENTIFIER)
        {
            Ok(_) => CredentialStoreStatus::Available,
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {
                CredentialStoreStatus::Available
            }
            Err(_) => CredentialStoreStatus::Unavailable,
        }
    }

    pub(crate) fn get(
        &self,
        identifier: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialError> {
        validate_identifier(identifier)?;
        match self
            .keychain
            .find_generic_password(&self.service, identifier)
        {
            Ok((password, _)) => Ok(Some(Zeroizing::new(password.to_vec()))),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }

    pub(crate) fn put(
        &self,
        identifier: &str,
        value: Zeroizing<Vec<u8>>,
    ) -> Result<(), CredentialError> {
        validate_identifier(identifier)?;
        if value.len() > MAX_SECRET_BYTES {
            return Err(CredentialError::LimitExceeded("value"));
        }
        self.keychain
            .set_generic_password(&self.service, identifier, value.as_slice())
            .map_err(|_| CredentialError::Unavailable)
    }

    /// Returns whether a credential existed. Removing a missing one is a safe no-op.
    pub(crate) fn remove(&self, identifier: &str) -> Result<bool, CredentialError> {
        validate_identifier(identifier)?;
        match self
            .keychain
            .find_generic_password(&self.service, identifier)
        {
            Ok((_, item)) => {
                item.delete();
                Ok(true)
            }
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }
}

/// `errSecItemNotFound` from Security.framework.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// Identifiers reach the keychain as an account name, so they stay in the same
/// narrow alphabet the durable `credential_ref` column already enforces.
fn validate_identifier(identifier: &str) -> Result<(), CredentialError> {
    let bytes = identifier.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_IDENTIFIER_BYTES || !bytes[0].is_ascii_lowercase() {
        return Err(CredentialError::InvalidIdentifier);
    }
    if bytes
        .iter()
        .skip(1)
        .any(|byte| !matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b':' | b'/' | b'-'))
    {
        return Err(CredentialError::InvalidIdentifier);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Every test gets a throwaway keychain file, so a test run never reads,
    /// writes, or prompts for anything in the real login keychain.
    fn temporary_store() -> (tempfile::TempDir, CredentialStore) {
        let directory = tempdir().expect("temporary directory");
        let store = CredentialStore::temporary(&directory.path().join("mux-test.keychain"));
        (directory, store)
    }

    #[test]
    fn secrets_round_trip_and_missing_secrets_are_absent_rather_than_errors() {
        let (_directory, store) = temporary_store();
        let identifier = "gmail:account-a";

        assert!(store.get(identifier).unwrap().is_none());
        store
            .put(identifier, Zeroizing::new(b"refresh-token".to_vec()))
            .unwrap();
        assert_eq!(
            store.get(identifier).unwrap().unwrap().as_slice(),
            b"refresh-token"
        );

        // Writing again replaces rather than duplicating.
        store
            .put(identifier, Zeroizing::new(b"rotated".to_vec()))
            .unwrap();
        assert_eq!(
            store.get(identifier).unwrap().unwrap().as_slice(),
            b"rotated"
        );

        assert!(store.remove(identifier).unwrap());
        // Removal is idempotent, and the secret is gone rather than emptied.
        assert!(!store.remove(identifier).unwrap());
        assert!(store.get(identifier).unwrap().is_none());
    }

    #[test]
    fn separate_identifiers_never_read_each_others_secrets() {
        let (_directory, store) = temporary_store();
        store
            .put("gmail:account-a", Zeroizing::new(b"first".to_vec()))
            .unwrap();
        store
            .put("gmail:account-b", Zeroizing::new(b"second".to_vec()))
            .unwrap();

        assert_eq!(
            store.get("gmail:account-a").unwrap().unwrap().as_slice(),
            b"first"
        );
        assert_eq!(
            store.get("gmail:account-b").unwrap().unwrap().as_slice(),
            b"second"
        );
        // Removing one leaves the other intact.
        assert!(store.remove("gmail:account-a").unwrap());
        assert!(store.get("gmail:account-a").unwrap().is_none());
        assert_eq!(
            store.get("gmail:account-b").unwrap().unwrap().as_slice(),
            b"second"
        );
    }

    #[test]
    fn hostile_identifiers_and_oversized_secrets_are_rejected_before_the_keychain() {
        let (_directory, store) = temporary_store();
        for identifier in [
            "",
            "Uppercase",
            "1leading-digit",
            ".leading-dot",
            "has space",
            "has\0null",
            "has\"quote",
        ] {
            assert!(
                matches!(
                    store.get(identifier),
                    Err(CredentialError::InvalidIdentifier)
                ),
                "accepted {identifier:?}"
            );
        }
        let long = "a".repeat(MAX_IDENTIFIER_BYTES + 1);
        assert!(matches!(
            store.get(&long),
            Err(CredentialError::InvalidIdentifier)
        ));
        assert!(matches!(
            store.put(
                "gmail:oversized",
                Zeroizing::new(vec![0; MAX_SECRET_BYTES + 1])
            ),
            Err(CredentialError::LimitExceeded("value"))
        ));
    }

    #[test]
    fn a_reachable_keychain_reports_itself_available() {
        let (_directory, store) = temporary_store();
        assert_eq!(store.status(), CredentialStoreStatus::Available);
    }
}
