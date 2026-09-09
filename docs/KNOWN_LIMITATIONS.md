# Known limitations

This file is intentionally blunt. Mux syncs real mail now, but it is not a finished mail client.

## Only Gmail can send

Gmail submission is real: the worker hands the frozen snapshot to the Gmail API as raw MIME and reconciles the acknowledgement. Nothing else has a transport. There is no SMTP client, so an IMAP account can receive mail and never answer it, and a queued send on such an account has nowhere to go.

Draft attachments are not persisted at all — the `drafts` table has no column for them — so they are not in the send snapshot either. Internationalized envelope addresses are rejected until an adapter proves and requests SMTPUTF8 support.

## Provider coverage is uneven

Gmail and IMAP can both be connected from the interface, but they are not equals. IMAP is read-only: nothing is pushed back, so a thread archived or starred in an IMAP account diverges from the server silently and permanently. Only implicit TLS on a port of your choosing is supported — no STARTTLS, no OAuth, no PREAUTH — and the password is the only mechanism. There is no JMAP, POP, Exchange, or Outlook adapter.

Permanent deletion is excluded everywhere. Moving a conversation offers only the destinations one flag change can reach — restore, archive, trash, untrash — because two journal entries for one gesture would leave the undo toast able to take back half of it; there is no picker for arbitrary labels. A partially labelled thread can be normalized to all or none, but a confirmed normalization has no exact undo, since the prior per-message distribution is not recoverable from a thread-level inverse. Cancelling before execution is safe.

## The MIME and HTML boundary is not a mail-security product

The parser is non-streaming behind a 32 MiB raw-message ceiling and has no independent CPU deadline. Attachment reads are also non-streaming and limited to 20 MiB raw, 28 MiB serialized over typed IPC.

Remote-image consent is implemented and its validators are adversarially tested, but the fetch has never run against a live host, so its timeout, redirect, and content-type behavior against real servers is unproven. A message ingested before that feature landed carries no loadable image candidates until it is re-ingested, which is what "Re-download all mail" in settings is for.

S/MIME and PGP, malware and quarantine scanning, provider attachment fetching and cache cleanup, and a safe preview and open policy are all still required. The typed external opener is contract-tested but has not been exercised against a live external destination.

## Credential storage tradeoffs

Credentials live in the macOS Keychain. That needs no Apple Developer Program membership, but it does need a *stable* code signature: a keychain item is bound to the identity that created it, and an ad-hoc signature is a hash of the binary, so every rebuild would otherwise make macOS ask for the login password again. Development builds are therefore signed through a cargo runner that finds whatever Apple Development certificate is in the local keychain. Release bundles sign ad-hoc unless `signingIdentity` is set locally, since a certificate belongs to one machine and does not belong in the repository. Neither path is a Developer ID, so distributing to other people remains unclaimed.

Any process running as the same user can read a stored credential without a prompt — the same boundary the mailbox database has. Requiring presence per read would need the data-protection keychain, which returns `errSecMissingEntitlement` without a signed entitlement, or `kSecAttrAccessControl` with biometry. Mux is macOS-only until another platform gets its own store, and there is no password fallback by design. A secret already borrowed by in-flight provider work is not protected, and nothing here defends a compromised live process.

## Platform and distribution

macOS is the only exercised runtime. Windows, Linux, Android, and iOS are not supported claims; the generated icon assets for those targets are not evidence that they compile or run.

Developer ID signing, notarization, hosted-download UX, auto-update, Gatekeeper behavior on another Mac, sandbox entitlements, and release-channel security are all incomplete.

## Test boundary

Vitest and jsdom mount the real Svelte components and enforce strict Tauri IPC expectations. They do not render the macOS WebView, compare pixels, or prove native window behavior, so `npm run e2e` is an interaction gate and macOS work still ends with a real `.app` launch and a look at it. The mailbox stress gate covers 50,000 rows; it is not a million-row or real-frame-time claim.

## Approximations that are on purpose

- Invitation conflict text is fixture data, not a real calendar lookup, and there is no calendar API.
- Search and stats take the local UTC offset the browser reports. An offset change inside a queried DST span stays an approximation until full timezone rules reach Rust.
- Activity is a bounded local journal view, not a provider audit log.
- Long conversations render every loaded message, with a "Show older" control beyond the first local page.
- There is no AI, and no plugin or MCP surface.
