# Mux

Mux is a local-first desktop mail client: a Tauri 2 shell, a Svelte interface, and a Rust/SQLite core. macOS is the only platform that has been built and run.

The interface reads the local SQLite projection and nothing else, so ordinary navigation never waits on the network. A background worker syncs each account on its own cadence and pushes changes back out; the mailbox re-reads when it hears the `mux://mailbox-changed` event.

## Run it on macOS

Requirements: Node.js 22.13 or newer, Rust stable, and the Xcode command line tools.

```bash
npm --prefix native install   # once
npm run dev                   # open the app
npm run build                 # build a local .app
```

The bundle lands under `native/src-tauri/target/release/bundle/macos/`. It is signed with an Apple Development certificate rather than a Developer ID, because a keychain item is bound to the identity that created it and an ad-hoc signature changes with every build — without a stable one macOS asks for the login password again after each rebuild. That is enough for local use and is not a distribution story.

A fresh database seeds a demo mailbox so that the first launch has something to read. To connect a real Gmail account, set up a client registration first: see [Google authorization](docs/GOOGLE_AUTHORIZATION_DEVELOPMENT.md).

## Layout

- `native/src` — the Svelte interface, plus the pure helpers the components share. Provider-agnostic by rule.
- `native/src-tauri/src` — the Rust core: store and migrations, search, the durable worker, provider adapters, the MIME/HTML trust boundary, keychain access.
- `scripts` — the repository-level gates `npm run verify` and `npm run build` call.
- `website` — the marketing site, built and deployed on its own.

## Providers

Gmail is the one provider you can connect from the interface. Settings opens the system browser for an installed-desktop authorization, and from then on the account bootstraps, follows the history feed, reconciles labels, and applies archive/read/star/trash mutations through the durable journal.

There is a complete read-only IMAP sync adapter — implicit TLS, capability re-read after login, UID-only search and fetch, resumable cursors, credentials in the keychain — wired into the worker alongside Gmail. No command creates an IMAP account, so in a normal build it only ever runs for accounts provisioned by its own tests.

Gmail is also the only account that can send. A queued send freezes a durable snapshot, waits out the undo window, and is then submitted to the Gmail API as raw MIME; delayed send, undo-send, and the uncertain-outcome recovery path all sit around that. There is no SMTP transport, so an IMAP account can receive and never reply. There is no JMAP, POP, Exchange, or Outlook adapter.

## Keyboard

Single letters act only when nothing else is listening: they are ignored while focus is in an input, editor, button, link, select, or dialog, and `Tab` is never captured. The command chords stay live inside text fields, since search and the palette have to be reachable while typing. `⌘` and `Ctrl` are interchangeable.

| Key | Action |
| --- | --- |
| `j` / `k` | Next / previous conversation |
| `Enter` | Move focus into the reader |
| `g` / `Shift-G` | Go to a folder, in any account or this one |
| `e` | Archive or restore |
| `b` | Snooze |
| `s` | Toggle star |
| `u` | Toggle read state |
| `m` | Move within the conversation's own account |
| `c` | Compose |
| `r` / `a` / `f` | Reply, reply all, forward |
| `⌘F` | Focus search |
| `⌘K` | Command palette |
| `⌘\` | Narrow the sidebar to icons, or widen it |
| `⌘,` | Settings |
| `?` | Show every shortcut |
| `Esc` | Close the topmost dialog, leave the reader, or clear the search |

The shortcut sheet is generated from the same table the dispatcher reads, so it cannot drift. Inside the rich editor the usual formatting shortcuts apply and `⌘K` inserts a validated `http`, `https`, or `mailto` link.

## Search

```text
from:jane@example.com after:2026-08-01 is:unread
subject:"architecture review" has:attachment
(category:Work OR category:Finance) -is:starred
has:invite in:all
```

The fields are `from`, `to`, `subject`, `account`, `in`, `label`, `category`, `is`, `has`, `after`, `before`, `date`, `filename`, and `domain`. Input length, token count, nesting, page size, and date parsing are all bounded before anything is compiled to SQL, and values stay bound parameters.

## Verify it

```bash
npm run verify                  # boundary checks, Svelte check, UI tests, Vite build, Rust tests, fmt, clippy
npm run e2e                     # production Svelte components behind strict typed Tauri IPC mocks
npm run test:provider-contract  # offline worker and adapter conformance
npm run benchmark               # 100,000 messages across 50,000 threads in a temporary database
```

`npm run e2e` mounts the real components under jsdom. It renders no macOS window and compares no pixels, so a claim about macOS still needs `npm run build`, a launch, and a look at the running app. The provider contract runs the production worker projector — replay, conflict, cursor crash safety, lease recovery, send uncertainty, batch ceilings — with no network and no account. The benchmark builds and removes its own database, including a populated migration from the oldest released schema and an interrupted-operation recovery, and never touches the application's.

## Storage and credentials

Mailbox projection, pending intent, Mux's own metadata, drafts, and the work journal live in SQLite under the OS application-data directory. Credentials do not: they live in the macOS Keychain, which the login password has already unlocked. Mux therefore has no password of its own and nothing blocks unattended sync.

## Boundary

This is not a production mail client yet. Remote images are blocked until you consent, and an approved one is fetched in Rust and handed to the interface only as a bounded `data:` image. Attachment quarantine and scanning, S/MIME and PGP, Developer ID signing and notarization, and non-macOS builds are all absent. See [Known Limitations](docs/KNOWN_LIMITATIONS.md) for the rest, and [Architecture](docs/ARCHITECTURE.md) for how the pieces fit.
