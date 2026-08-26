# Google OAuth development setup

Mux uses Google's installed desktop application flow. The app binds an IPv4
loopback listener on a random port, generates a new state value and PKCE S256
challenge for every attempt, and opens the authorization URL in the system
browser. It requests only:

```text
https://www.googleapis.com/auth/gmail.modify
```

The Google client registration is not bundled or committed. Put the path to an
ignored installed-client JSON file in `.env` at the repository root:

```text
MUX_GOOGLE_OAUTH_CLIENT_CONFIG=/absolute/path/to/client.json
```

`npm run dev` loads that file before starting the native process, so the
variable is present without exporting it by hand. A value already in the
environment wins, so a one-off override still works:

```bash
MUX_GOOGLE_OAUTH_CLIENT_CONFIG=/other/client.json npm run dev
```

Without the variable, connecting an account fails with
`oauth_client_unavailable` and no browser opens.

The referenced JSON must be a Google `installed` client registration with a
loopback redirect. Mux reads it in Rust. Neither the client value nor any OAuth
code or token crosses WebView IPC. Once authorization succeeds, the client
registration, refresh token, and current access token are stored together in the
macOS Keychain; SQLite stores only the normalized mailbox identity and an opaque
reference to that keychain item.

The existing ignored Tidings desktop registration may be supplied as the file
above. Do not copy it into this repository or add it to a release archive.

This setup enables local development authorization only. It is not evidence of
a generally distributable or verified Google OAuth application, and no Gmail
support claim should be made until the live sync and mutation contracts have
also run successfully.
