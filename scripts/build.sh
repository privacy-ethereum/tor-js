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

# --- Reproducibility ---------------------------------------------------------
# Release artifacts must hash identically across machines (CI, local, third-
# party verifiers reproducing the anon-rpc worker pin). Everything that gets
# baked into dist/tor_js_bg.wasm is therefore pinned:
#
#   rustc          rust-toolchain.toml
#   crate versions Cargo.lock (enforced with --locked below)
#   wasm-bindgen   Cargo.lock — wasm-pack fetches the CLI matching the locked
#                  crate version, so it needs no separate pin
#   wasm-pack      PINNED_WASM_PACK below
#   wasm-opt       PINNED_WASM_OPT below (binaryen)
#   esbuild        package-lock.json
#
# Bump deliberately — each bump is a new artifact hash.
#
# One input is NOT pinnable: a dirty working tree makes build.rs embed a
# timestamp (see crates/tor-js-wasm/build.rs), so only clean-tree builds are
# reproducible. That is by design — it marks an unreproducible build as such.
PINNED_WASM_PACK="0.14.0"
PINNED_WASM_OPT="116"

# rustc embeds panic-location paths, and two families of them are absolute and
# per-machine. Both are remapped to fixed virtual prefixes.
#
# 1. Dependency sources under CARGO_HOME ($HOME differs per machine).
# 2. The standard library. rustc normally emits these as `/rustc/<commit>/...`,
#    already machine-independent — BUT only while the `rust-src` component is
#    absent. With rust-src installed (rust-analyzer pulls it in, so most dev
#    machines have it) rustc resolves std paths to the real sysroot instead, and
#    bakes in `$HOME/.rustup/toolchains/<version>-<host-triple>/...`. That
#    embeds both the username and the host architecture, so a dev machine and CI
#    produce different artifacts. Remapping it back to the canonical
#    `/rustc/<commit>` form makes the output identical either way — and is a
#    no-op on a machine where rust-src is absent, since nothing matches.
#
# Both values are derived from rustc, so a toolchain bump needs no edit here.
# Set (not append) RUSTFLAGS so a machine-local ~/.cargo/config.toml or an
# ambient RUSTFLAGS cannot perturb the build either.
RUST_SYSROOT="$(rustc --print sysroot)"
RUSTC_COMMIT="$(rustc -vV | sed -ne 's/^commit-hash: //p')"
export RUSTFLAGS="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo-home"
export RUSTFLAGS="$RUSTFLAGS --remap-path-prefix=$RUST_SYSROOT/lib/rustlib/src/rust=/rustc/$RUSTC_COMMIT"

if [[ "$PROFILE" == "--release" ]]; then
    # wasm-opt must be on PATH at the pinned version: without one, wasm-pack
    # silently downloads its own binaryen (a different version per wasm-pack
    # release), and the optimizer version changes every byte of the output.
    have_wasm_pack=$(wasm-pack --version 2>/dev/null | awk '{print $2}')
    if [[ "$have_wasm_pack" != "$PINNED_WASM_PACK" ]]; then
        echo "error: wasm-pack $PINNED_WASM_PACK required for release builds (found: ${have_wasm_pack:-none})" >&2
        echo "  curl https://drager.github.io/wasm-pack/installer/init.sh -sSf | VERSION=v$PINNED_WASM_PACK sh" >&2
        exit 1
    fi
    # `wasm-opt --version` prints e.g. "wasm-opt version 116 (version_116)".
    have_wasm_opt=$(wasm-opt --version 2>/dev/null | awk '{print $3}')
    if [[ "$have_wasm_opt" != "$PINNED_WASM_OPT" ]]; then
        echo "error: wasm-opt (binaryen) version $PINNED_WASM_OPT required on PATH for release builds (found: ${have_wasm_opt:-none})" >&2
        echo "  https://github.com/WebAssembly/binaryen/releases/tag/version_$PINNED_WASM_OPT" >&2
        exit 1
    fi
fi

# Start from a clean dist/ so stale artifacts never leak into a build (or a
# published tarball).
rm -rf dist

echo "Building tor-js WASM ($PROFILE)..."
# --locked: the committed Cargo.lock is part of the reproducible input set; if
# it would need changing, fail rather than resolve silently.
wasm-pack build crates/tor-js-wasm --target web $PROFILE -- --locked

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
