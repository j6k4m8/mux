# Known Limitations

This file is intentionally blunt. The current build is a native demo, not a production email client.

## No live mail provider support

Mux now contains a Gmail synchronization and mailbox-mutation core, compiled Google HTTPS/refresh transports, and a bounded installed-desktop authorization lifecycle. State/PKCE, exact loopback parsing/deadlines, cancellation, profile binding, keychain-only persistence, and the existing ignored Tidings registration loader have been exercised without exposing values. No Google consent grant, Google endpoint, or live mailbox has been exercised. The demo performs no network mail operation, and Gmail support is not claimed. There is no JMAP, IMAP, SMTP, POP, Exchange, or Outlook adapter.

The provider-neutral outgoing builder is compiled and tested, but it is not a transport: no real SMTP, Gmail, or JMAP submission or provider reconciliation request has run. Draft attachments are not yet persisted or wired into the durable send snapshot. Internationalized envelope addresses are explicitly rejected until a real adapter proves and requests SMTPUTF8 support.

Before Gmail support can be claimed, Mux still needs a completed live consent grant, production HTTP conformance, a dedicated live test account, and exercised macOS auth/bootstrap/delta/offline/rate-limit/deletion/mutation behavior. The mutation core has never touched Google; permanent deletion is excluded, custom-label changes have no Svelte picker, and sending remains absent. A partially labelled thread can be normalized to all/none, but a confirmed normalization cannot offer exact undo because the prior per-message distribution is not recoverable through a thread-level inverse; cancelling before execution remains safe. Other providers have no adapter contract yet.

## Bounded MIME/HTML boundary is not a complete mail-security product

Mux now exercises a bounded standards parser for MIME trees, common transfer encodings and charsets, RFC 2047/2231 metadata, nested messages, and exact binary attachments. Hostile HTML is rebuilt through an HTML5 tree into controlled render nodes; active content and remote resources are denied, and external links use a typed HTTP(S)-only opener policy.

The parser is non-streaming behind a 32 MiB raw-message ceiling and has no independent CPU deadline. Attachment reads are also non-streaming and limited to 20 MiB raw (28 MiB serialized over typed IPC). Remote-image consent is implemented and its validators are adversarially tested, but the network fetch has never run against a live host, so its timeout, redirect, and content-type behavior against real servers is unproven; a message imported before the feature landed carries no loadable image candidates until it is re-ingested, which is what the Re-download all mail action in settings is for. S/MIME/PGP, malware/quarantine scanning, provider attachment fetching/cache cleanup, and safe preview/open policy remain required. The bundled WebView origin policy has been exercised by the running app; the typed external opener remains contract-tested but has not been exercised against a live external destination.

## Credential-storage tradeoffs

Credentials live in the macOS Keychain. This needs no Apple Developer Program membership, but it does need a *stable* code signature: a keychain item is bound to the identity that created it, and an ad-hoc signature is a hash of the binary, so every rebuild would otherwise make macOS ask for the login password again. Development builds are therefore signed with an Apple Development certificate through a cargo runner, and release bundles through `signingIdentity`. Neither is a Developer ID, so distribution to other people remains unclaimed.

Any process running as the same user can read the stored credential without a prompt — the same boundary the mailbox database has. Requiring presence per read would need the data-protection keychain, which returns `errSecMissingEntitlement` without a signed entitlement, or `kSecAttrAccessControl` with biometry. Mux is macOS-only until another platform gets its own store; there is no password fallback by design. Secrets already borrowed by in-flight provider work are not protected, nor is anything against a compromised live process.

## Platform and distribution boundary

macOS is the only exercised runtime. Windows, Linux, Android, and iOS are not supported claims. The repository contains generated icon assets for future targets, but those assets are not evidence that those targets compile or run.

Local `.app` builds do not require an Apple Developer account and are explicitly ad-hoc signed. Developer ID signing, notarization, hosted-download UX, auto-update, Gatekeeper behavior on another Mac, sandbox entitlements, and release-channel security have not been completed.

## Interaction-test boundary

Vitest/jsdom mounts the real Svelte components and enforces strict Tauri IPC expectations. It does not render the macOS WebView, provide pixel comparisons, or prove native window behavior. `npm run e2e` therefore remains an interaction gate, followed by a real `.app` launch/inspection for macOS work.

## Deliberate demo approximations

- The demo provider worker is deterministic and local; Gmail behavior is hermetic/offline only.
- Invitation conflict text is fixture data, not a real calendar lookup.
- Gmail fixture bytes pass through the bounded MIME/HTML trust boundary; no live provider has exercised that path yet.
- Large thread lists use bounded native paging and a 120-row render window. Conversations render every loaded message with a simple Show older fallback beyond the initial local page. Pure/UI stress gates cover 50,000 mailbox rows and 10,000 messages, but this is not a measured million-row or real-WebView frame-time claim.
- Search relative dates use the browser-supplied local offset; offset changes within a queried DST span remain a documented approximation until full timezone rules are passed to Rust.
- Activity is a bounded local journal view, not a provider audit log.
- Real AI and provider-facing plugins/MCP are absent.
