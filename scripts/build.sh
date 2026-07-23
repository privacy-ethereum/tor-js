#!/bin/bash
# Build the tor-js WASM module (Rust crate in rust/) and the TypeScript
# wrapper (this repo's root package).
#
# Usage: scripts/build.sh [--release]
#
#   (default)   Development build, no wasm-opt. Faster, larger WASM.
#   --release   Release build with wasm-opt (-Oz). Slower, smaller WASM.

set -e

# Run from the repo root regardless of where the script is invoked from.
cd "$(dirname "$0")/.."

PROFILE="--dev"

while [[ $# -gt 0 ]]; do
    case $1 in
        --release)
            PROFILE="--release"
            shift
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Start from a clean dist/ so stale artifacts never leak into a build (or a
# published tarball).
rm -rf dist

echo "Building tor-js WASM ($PROFILE)..."
wasm-pack build crates/tor-js-wasm --target web $PROFILE

# Copy the crate README into the wasm-pack output (pkg/).
cp crates/tor-js-wasm/README.md crates/tor-js-wasm/pkg/

echo "WASM package available at: crates/tor-js-wasm/pkg/"

# Build the TypeScript wrapper. Call build.mjs directly rather than
# `npm run build` to avoid recursing back into this script.
echo ""
echo "Building TypeScript wrapper..."
node build.mjs

echo ""
echo "Done! Package output available at: dist/"
