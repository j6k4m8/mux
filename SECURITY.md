# Security notes

## Exercised controls

- There is no product HTTP server. Tauri loads one bundled Svelte frontend, and the development assets bind to `127.0.0.1`. WebView navigation is restricted to the exact app and development origins.
- Tauri commands are explicitly allowlisted and tested. The default window capability requests only `core:default`, and a test asserts that no command name on the invoke surface looks like a credential, token, or password.
- SQL uses bound parameters. Search, cursor, identifier, page, text, recipient, attachment, and operation inputs are bounded and validated in Rust.
- Raw mail is bounded before and after standards-based MIME parsing. Graph, header, text, attachment, and address budgets are enforced, and exact binary digests are asserted.
- Rendering treats message data as hostile. HTML is rebuilt through an HTML5 tree into a small allowlist, active content and remote resources are rejected, and the result is displayed inside a frame that is denied `allow-scripts` — an engine-level omission rather than something a sanitizer has to keep catching.
- Message links are normalized and revalidated by a typed Rust command. The frontend has no generic opener authority, and a link that tries to navigate the frame is refused and handed to that same guarded opener.
- Remote images are blocked until the reader consents. A blocked resource leaves an inert marker carrying no URL; on consent Rust resolves and fetches the image itself over HTTPS with DNS pinned to a pre-validated public address, same-host redirects only, a bitmap content-type allowlist, and a byte ceiling, and returns a bounded `data:` URL. The app CSP grants no `https:` image authority.
- Provider credentials are excluded from SQLite, fixtures, logs, IPC, and release manifests; a repository scan rejects credential-shaped filenames and common secret shapes in source and fixtures.
- Credentials live in the macOS Keychain, reached only from Rust through zeroizing buffers. Mux has no password of its own and no lock lifecycle.
- Send replay is not treated as idempotent. A lost post-submission acknowledgement becomes an explicit uncertain outcome rather than a retry.

## Credential threat boundary

Keychain items are encrypted under keys tied to the macOS login password and are readable only while that user is logged in. They are written with `SecItemAdd`, which sets no per-application ACL, so any process running as the same user can read them without a prompt — the same boundary the mailbox database already has. Requiring presence per read would need `kSecAttrAccessControl` with biometry, which Mux does not use. A credential already borrowed by in-flight provider I/O cannot be revoked mid-flight.

## Not yet production-safe

The following have no adversarial coverage, and a real account is exposed to them:

- worst-case MIME parsing has no streaming or CPU deadline behind its raw-message ceiling;
- the remote-image fetch has never run against a live host, so its timeout, redirect, and content-type behavior against real servers is untested;
- S/MIME and PGP processing, and attachment quarantine or scanning, do not exist;
- there is no provider attachment fetch, cache, preview, or open policy;
- signed and notarized update and distribution flows are absent;
- no target platform other than macOS has had its data protection or lifecycle behavior reviewed.
