#!/usr/bin/env bash
# Sign the exact bytes of a manifest with minisign (ADR 0005, D3).
# Usage: MINISIGN_SECRET_KEY=... MINISIGN_PASSWORD=... scripts/sign-manifest.sh latest.json
# Writes latest.json.minisig beside the file, then checks it with the public key in the repository.
# The key text comes from the environment. It never goes on a command line or into the log.
set -euo pipefail

file=${1:?usage: sign-manifest.sh <file>}
: "${MINISIGN_SECRET_KEY:?set MINISIGN_SECRET_KEY}"
: "${MINISIGN_PASSWORD:?set MINISIGN_PASSWORD}"
pub="$(dirname "$0")/../assets/update/minisign.pub"

umask 077
key=$(mktemp)
trap 'rm -f "$key"' EXIT
printf '%s\n' "$MINISIGN_SECRET_KEY" >"$key"

# minisign reads the password from stdin when no terminal is present.
printf '%s\n' "$MINISIGN_PASSWORD" | minisign -S -s "$key" -m "$file" -x "$file.minisig" \
  -t "mqtt-navigator manifest"
minisign -V -p "$pub" -m "$file" -x "$file.minisig"
