#!/bin/bash
# Push an encrypted WASM binary to the hash-artifacts branch.
#
# Files are AES-256-GCM encrypted using the WASM's SHA256 hash as the key,
# and stored under SHA256(hash) so the filename doesn't reveal the key.
# This prevents accidental exposure of dev builds with sensitive info.
#
# By default, artifacts are stored under tmp/<hash-hash> (temporary).
# Use --persist to store under <prefix>/<hash-hash> (permanent).
# All tmp/* files are scrubbed from the branch history on each push.
#
# Usage: scripts/push-hash-artifact.sh [--persist]

set -euo pipefail

cd "$(dirname "$0")/.."

PERSIST=false
for arg in "$@"; do
  case "$arg" in
    --persist) PERSIST=true ;;
    *) echo "Unknown argument: $arg"; exit 1 ;;
  esac
done

WASM_FILE="rust/pkg/tor_js_bg.wasm"

if [ ! -f "$WASM_FILE" ]; then
  echo "Error: $WASM_FILE not found. Run npm run build:release first."
  exit 1
fi

# Compute hashes and encrypt using Node.js
ENCRYPT_OUTPUT=$(WASM_INPUT="$WASM_FILE" node --input-type=module <<'SCRIPT'
import { readFileSync, writeFileSync } from 'node:fs';
import { createHash, createCipheriv } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const wasm = readFileSync(process.env.WASM_INPUT);

// hash = SHA256(wasm)
const hashBytes = createHash('sha256').update(wasm).digest();
const hash = hashBytes.toString('hex');

// hashHash = SHA256(hash bytes) — used for filename
const hashHash = createHash('sha256').update(hashBytes).digest('hex');

// Encrypt: AES-256-GCM, key = hashBytes, IV = hashBytes[0:12]
const iv = hashBytes.subarray(0, 12);
const cipher = createCipheriv('aes-256-gcm', hashBytes, iv);
const encrypted = Buffer.concat([cipher.update(wasm), cipher.final(), cipher.getAuthTag()]);

// Write encrypted file to temp location
const outPath = join(tmpdir(), hashHash);
writeFileSync(outPath, encrypted);

// Output values for the shell script
console.log(`HASH=${hash}`);
console.log(`HASH_HASH=${hashHash}`);
console.log(`ENCRYPTED_FILE=${outPath}`);
SCRIPT
)

eval "$ENCRYPT_OUTPUT"

if [ "$PERSIST" = true ]; then
  PREFIX="${HASH_HASH:0:2}"
  DEST="$PREFIX/$HASH_HASH"
else
  DEST="tmp/$HASH_HASH"
fi

echo "WASM hash:      $HASH"
echo "hash(hash):     $HASH_HASH"
echo "Artifact path:  $DEST"
echo "Mode:           $([ "$PERSIST" = true ] && echo "persistent" || echo "temporary")"

REMOTE=$(git remote get-url origin)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK" "$ENCRYPTED_FILE"' EXIT

# Clone the hash-artifacts branch (full history needed for rewriting)
if git ls-remote --exit-code --heads "$REMOTE" hash-artifacts >/dev/null 2>&1; then
  git clone --branch hash-artifacts --single-branch "$REMOTE" "$WORK"
else
  echo "Creating new orphan branch hash-artifacts..."
  git init "$WORK"
  git -C "$WORK" remote add origin "$REMOTE"
  git -C "$WORK" checkout --orphan hash-artifacts
fi

# Scrub all tmp/* from history
if git -C "$WORK" ls-tree -r --name-only HEAD 2>/dev/null | grep -q '^tmp/'; then
  echo "Scrubbing tmp/* from history..."
  FILTER_BRANCH_SQUELCH_WARNING=1 git -C "$WORK" filter-branch --force \
    --index-filter 'git rm --cached --ignore-unmatch -r tmp/' \
    --prune-empty -- hash-artifacts || true

  # If all commits were pruned, the branch ref is gone — recreate as orphan
  if ! git -C "$WORK" rev-parse --verify hash-artifacts >/dev/null 2>&1; then
    git -C "$WORK" checkout --orphan hash-artifacts
    git -C "$WORK" rm -rf --cached . >/dev/null 2>&1 || true
  fi

  # Clean leftover tmp/ files from working tree
  rm -rf "$WORK/tmp"
  NEEDS_PUSH=true
fi

# Check if artifact already exists
if [ -f "$WORK/$DEST" ]; then
  echo "Artifact $DEST already exists on hash-artifacts branch."
  if [ "${NEEDS_PUSH:-}" = true ]; then
    git -C "$WORK" push --force origin hash-artifacts
    echo "Pushed (scrubbed tmp/* from history)."
  else
    echo "Nothing to do."
  fi
  exit 0
fi

# Add the new artifact
mkdir -p "$(dirname "$WORK/$DEST")"
cp "$ENCRYPTED_FILE" "$WORK/$DEST"
git -C "$WORK" add "$DEST"
git -C "$WORK" commit -m "Add $DEST"

git -C "$WORK" push --force origin hash-artifacts

echo "Pushed $DEST to hash-artifacts branch."
