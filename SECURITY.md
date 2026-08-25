# Security Notes

## Exercised controls

- There is no product HTTP server. Tauri loads one bundled Svelte frontend; development assets bind to `127.0.0.1`.
- Tauri commands are explicitly allowlisted and tested. The default window capability currently requests only `core:default`.
- SQL uses bound parameters. Search, cursor, identifier, page, text, recipient, attachment, and operation inputs are bounded and validated in Rust.
- Raw mail is bounded before and after standards-based MIME parsing. MIME graph/header/text/attachment/address budgets and exact binary-digest tests are exercised.
- UI rendering treats message data as hostile. HTML is rebuilt through an HTML5 tree into a small allowlist; active content and remote resources are rejected.
- Message links are normalized and revalidated by a typed Rust command. The frontend has no generic opener authority, and WebView navigation is restricted to exact app/development origins.
- Provider credentials are excluded from SQLite, fixtures, screenshots, logs, IPC, and release manifests.
- The credential vault uses a versioned authenticated envelope, bounded Argon2id, XChaCha20-Poly1305, fresh nonces, a stable `0600` lock file, atomic replacement, and zeroizing buffers.
- Send replay is not treated as idempotent. Lost post-submission acknowledgement becomes an explicit uncertain outcome.
- Workspace databases, build output, dependencies, and transient screenshots are excluded from release manifests.

## Vault threat boundary

The vault is an Apple-account-independent local encrypted file. It protects credentials at rest while locked. It does not prevent deletion, offline password guessing, inspection by a compromised live process, or continued use of a credential already borrowed by in-flight provider I/O. Locking prevents new borrows; it cannot safely revoke an already-submitted send. Forgotten passphrases cannot be recovered.

## Not yet production-safe

Do not use real accounts until the following have dedicated adversarial tests and an exercised provider implementation:

- remote-image and tracking-resource consent;
- streaming/deadline controls for worst-case MIME parsing;
- S/MIME/PGP processing and attachment quarantine/scanning;
- provider attachment fetch/cache/preview/open policy;
- OAuth/device-flow callback handling or password authentication, as applicable;
- provider delta tokens, pagination, deletions, rate limits, retry classification, and send reconciliation;
- signed/notarized update and distribution flows;
- security review of each target platform's data protection and lifecycle behavior.

The synthetic demo content contains no real credentials or mailbox data.
