# Security Notes

## Exercised controls

- There is no product HTTP server. Tauri loads one bundled Svelte frontend; development assets bind to `127.0.0.1`.
- Tauri commands are explicitly allowlisted and tested. The default window capability currently requests only `core:default`.
- SQL uses bound parameters. Search, cursor, identifier, page, text, recipient, attachment, and operation inputs are bounded and validated in Rust.
- Raw mail is bounded before and after standards-based MIME parsing. MIME graph/header/text/attachment/address budgets and exact binary-digest tests are exercised.
- UI rendering treats message data as hostile. HTML is rebuilt through an HTML5 tree into a small allowlist; active content and remote resources are rejected.
- Message links are normalized and revalidated by a typed Rust command. The frontend has no generic opener authority, and WebView navigation is restricted to exact app/development origins.
- Provider credentials are excluded from SQLite, fixtures, screenshots, logs, IPC, and release manifests.
- Provider credentials live in the macOS Keychain, reached only from Rust through zeroizing buffers. Mux has no password of its own and no lock lifecycle.
- Send replay is not treated as idempotent. Lost post-submission acknowledgement becomes an explicit uncertain outcome.
- Workspace databases, build output, dependencies, and transient screenshots are excluded from release manifests.

## Credential threat boundary

Keychain items are encrypted under keys tied to the macOS login password and are available only while that user is logged in. They are written with `SecItemAdd`, which sets no per-application ACL, so any process running as the same user can read them without a prompt — the same boundary the mailbox database already has. Requiring presence per read would need `kSecAttrAccessControl` with biometry, which Mux does not yet use. A credential already borrowed by in-flight provider I/O cannot be revoked mid-flight.

## Not yet production-safe

Do not use real accounts until the following have dedicated adversarial tests and an exercised provider implementation:

- remote-image and tracking-resource fetching against a live host; the consent flow, policy scoping, and URL/address validation are adversarially tested, but no test performs a real fetch;
- streaming/deadline controls for worst-case MIME parsing;
- S/MIME/PGP processing and attachment quarantine/scanning;
- provider attachment fetch/cache/preview/open policy;
- OAuth/device-flow callback handling or password authentication, as applicable;
- provider delta tokens, pagination, deletions, rate limits, retry classification, and send reconciliation;
- signed/notarized update and distribution flows;
- security review of each target platform's data protection and lifecycle behavior.

The synthetic demo content contains no real credentials or mailbox data.
