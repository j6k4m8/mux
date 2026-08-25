# Mux

Mux is a local-first desktop mail client under active development. There is now one application in this repository: a Tauri 2 shell, a Svelte interface, and a Rust/SQLite core. The former Node server and browser UI were removed after their store, search, operation, interaction, and 100,000-message gates were replaced.

The current build opens as a deterministic demo. A Gmail synchronization and mailbox-mutation core plus a bounded installed-desktop Google authorization flow now compile behind the Rust provider boundary and are exercised with hermetic protocol fixtures. The existing ignored Tidings desktop registration has passed Mux's bounded loader, but no Google consent grant, endpoint, or live Gmail account has been exercised. The shipped demo therefore does not yet connect to Gmail, JMAP, IMAP, SMTP, POP, or any other real provider, and Gmail support is not claimed.

## Run it on macOS

Requirements:

- Node.js 22.13 or newer
- Rust stable
- Xcode Command Line Tools

Install the frontend tools once:

```bash
npm --prefix native install
```

Open the native application:

```bash
npm run dev
```

Build a local `.app`:

```bash
npm run build
```

The bundle is written below `native/src-tauri/target/release/bundle/macos/`. Local builds are explicitly ad-hoc signed and do not require an Apple Developer account. Developer ID signing, notarization, hosted-download UX, and Gatekeeper behavior on another Mac are separate work and are not claimed here.

## What works in the demo

- Default light interface with a unified three-pane mailbox; optional persisted dark appearance
- Inbox, Starred, Snoozed, Sent, Drafts, Archive, Trash, All mail, account, and smart views
- Long demo conversations with collapsible message cards and a simple Show older fallback beyond the initial local page
- Local search with boolean expressions, phrases, dates, fields, attachment/invitation predicates, and FTS body matching
- Archive/restore, trash/untrash, read/unread, star/unstar, snooze, delayed undo, stale-undo rejection, and automatic read-on-focus
- Invitation display and local Accept/Maybe/Decline state
- Rich compose, reply, reply-all, and forward with safe links, formatting, recipient chips, local drafts, and attachments
- Inline reply that expands into a rich card after 100 characters and can always pop into the composer
- Delayed send, undo-send, durable operation activity, and explicit recovery from an uncertain send without retrying it
- Provider-neutral RFC 5322/MIME construction with stable Date/Message-ID/reply threading, multipart alternative/attachments, envelope-only Bcc, deterministic bytes, and an explicit ASCII-only SMTPUTF8 policy
- Passphrase-encrypted local credential vault whose secrets never cross the Tauri IPC boundary
- Typed Gmail connect/cancel lifecycle using the system browser, state, PKCE S256, an exact bounded loopback callback, `gmail.modify`, profile identity binding, and vault-only tokens
- Offline-exercised Gmail bootstrap/history pagination, bounded label reconciliation, vault-only authority lookup, invalid-history rescan, and desired-state mailbox mutations behind provider-neutral Rust batches
- Bounded standards-based MIME/charset ingestion with exact binary attachment preservation and a typed 20 MiB raw attachment-read ceiling
- HTML5-tree sanitization, remote-resource denial, controlled render nodes, and typed external-link opening
- Responsive navigation and reader layouts down to the configured macOS window minimum
- Keyboard navigation and a working command palette

All mail content and provider acknowledgements in this build are synthetic.

## Keyboard

Global mailbox shortcuts are disabled while focus is in an input, editor, button, link, select, or dialog. `Tab` and `Shift-Tab` are never captured by the mailbox shortcut layer.

| Key | Action |
| --- | --- |
| `/` | Focus search |
| `j` / `k` | Next / previous conversation |
| `e` | Archive or restore |
| `h` | Snooze |
| `s` | Toggle star |
| `u` | Toggle read state |
| `r` | Reply |
| `a` | Reply all |
| `f` | Forward |
| `c` | Compose |
| `⌘K` / `Ctrl-K` | Open command palette outside an editor |
| `Esc` | Close the topmost modal, mobile reader/navigation, or active search |

Inside the rich editor, the usual formatting shortcuts are available and `⌘K`/`Ctrl-K` inserts a validated `http`, `https`, or `mailto` link.

## Search examples

```text
from:jane@example.com after:2026-08-01 is:unread
subject:"architecture review" has:attachment
(category:Work OR category:Finance) -is:starred
has:invite in:all
```

Input length, token count, nesting, page size, and date parsing are bounded before SQL compilation. SQL parameters remain bound values.

## Verify it

```bash
npm run verify
npm run e2e
npm run test:provider-contract
npm run benchmark
```

`npm run e2e` is the production-Svelte interaction gate using typed Tauri IPC mocks. It does not use Playwright, start a second product server, or claim WebView/pixel coverage. A release still requires building, launching, and inspecting the actual `.app` on macOS.

The offline provider-contract gate exercises atomic worker projection, exact replay/conflict behavior, cursor crash safety, lease recovery, send uncertainty, and the 32 MiB aggregate batch boundary without a network or real account. The benchmark creates and removes an isolated temporary native database containing 100,000 messages across 50,000 threads, including a populated v12 migration and interrupted-operation recovery. It never touches the application database. Message-detail pages split early when their actual JSON would exceed 16 MiB; attachment bytes use a separate exact lookup capped at 20 MiB raw and 28 MiB serialized.

## Storage and credentials

Mailbox projection, pending intent, Mux metadata, drafts, and the work journal live in SQLite under the operating-system application-data directory. Provider credentials do not live in SQLite. The current vault is passphrase-encrypted and intentionally independent of macOS Keychain, code signing, and an Apple Developer account. It locks at process start, so unattended provider sync is not yet possible.

## Current boundary

Do not treat this demo as a production email client. The bounded MIME/HTML trust boundary and Gmail adapter are exercised only with hostile/hermetic fixtures; no real mailbox feeds them yet. The authorization lifecycle exists, but live Google consent and provider execution remain unproven. Remote-image consent, attachment quarantine/scanning, S/MIME/PGP, Developer ID signing/notarization, unattended synchronization, and non-macOS builds also remain unproven. See [Known Limitations](docs/KNOWN_LIMITATIONS.md) and [Architecture](docs/ARCHITECTURE.md).
