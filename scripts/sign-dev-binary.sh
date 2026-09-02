#!/bin/sh
# Cargo runner for local development.
#
# A macOS keychain item is bound to the code identity that created it. An
# ad-hoc signature is a hash of the binary, so every rebuild produces a new
# identity and macOS asks for the login password again. Signing each dev build
# with a stable certificate keeps that identity constant, so an approval given
# once keeps holding.
#
# Without a certificate this is a no-op: the binary still runs, it just falls
# back to ad-hoc signing and its keychain prompts return.
set -e
binary="$1"
shift

identity=$(security find-identity -v -p codesigning 2>/dev/null \
  | sed -n 's/.*"\(Apple Development: [^"]*\)".*/\1/p' | head -1)

if [ -n "$identity" ]; then
  codesign --force --sign "$identity" \
    --identifier com.jordanmatelsky.mux "$binary" >/dev/null 2>&1 || true
fi

exec "$binary" "$@"
