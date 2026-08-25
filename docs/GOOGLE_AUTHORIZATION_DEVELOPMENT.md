# Google OAuth development setup

Mux uses Google's installed desktop application flow. The app binds an IPv4
loopback listener on a random port, generates a new state value and PKCE S256
challenge for every attempt, and opens the authorization URL in the system
browser. It requests only:

```text
https://www.googleapis.com/auth/gmail.modify
```

The Google client registration is not bundled or committed. Point the native
process at an ignored installed-client JSON file when starting the development
app:

```bash
MUX_GOOGLE_OAUTH_CLIENT_CONFIG=/absolute/path/to/client.json npm run dev
```

The referenced JSON must be a Google `installed` client registration with a
loopback redirect. Mux reads it in Rust. Neither the client value nor any OAuth
code or token crosses WebView IPC. Once authorization succeeds, the client
registration, refresh token, and current access token are stored together in
the passphrase-encrypted Mux vault; SQLite stores only the normalized mailbox
identity and an opaque vault-record reference.

For local migration testing, the existing ignored Tidings desktop registration
may be supplied as the file above. Do not copy it into this repository or add
it to a release archive.

This setup enables local development authorization only. It is not evidence of
a generally distributable or verified Google OAuth application, and no Gmail
support claim should be made until the live sync and mutation contracts have
also run successfully.
