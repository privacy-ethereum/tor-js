#!/usr/bin/env bash
# RUSTC_WRAPPER that makes `-C metadata` host-independent.
#
# Why: cargo derives each unit's `-C metadata` hash from inputs that include
# host-specific information, so the same target (e.g. wasm32-unknown-unknown)
# built from an x86_64 host and an aarch64 host gets different crate
# disambiguators. Those feed symbol mangling, and the mangled names influence
# codegen ordering and inlining decisions — so the *stripped* artifact differs
# too. See https://github.com/rust-lang/cargo/issues/8140.
#
# `-C metadata` accumulates rather than overrides, so RUSTFLAGS cannot fix this;
# a wrapper can, because it sees cargo's own argument and can rewrite it.
#
# The replacement must stay unique per compilation unit or rustc reports
# colliding StableCrateId values (this graph legitimately contains three
# getrandom versions, two rand_core versions, and host+target builds of the same
# proc-macro crates). It is therefore derived from the unit's identity —
# crate name, crate types, edition, target triple, cfgs, and the source path
# with machine-specific prefixes normalised away — all of which are identical
# across hosts but differ between units.
#
# Set TOR_JS_RUSTC_KEYLOG=<path> to append `key<TAB>identity` per invocation,
# for auditing uniqueness.

set -euo pipefail

rustc_bin="$1"
shift

# Passthrough for probe invocations (`rustc -vV`, `--print` queries): they carry
# no -C metadata, and rewriting anything there would confuse cargo.
have_metadata=0
for a in "$@"; do
  case "$a" in
    metadata=*|-Cmetadata=*|--codegen=metadata=*) have_metadata=1; break ;;
  esac
done
if [ "$have_metadata" -eq 0 ]; then
  exec "$rustc_bin" "$@"
fi

# --- collect the unit's host-independent identity ---------------------------
crate_name=""
target_triple=""
edition=""
crate_types=()
cfgs=()
src=""

prev=""
for a in "$@"; do
  case "$prev" in
    --crate-name) crate_name="$a" ;;
    --target)     target_triple="$a" ;;
    --crate-type) crate_types+=("$a") ;;
    --cfg)        cfgs+=("$a") ;;
  esac
  case "$a" in
    --edition=*)    edition="${a#--edition=}" ;;
    --target=*)     target_triple="${a#--target=}" ;;
    --crate-type=*) crate_types+=("${a#--crate-type=}") ;;
    --cfg=*)        cfgs+=("${a#--cfg=}") ;;
    *.rs)           src="$a" ;;
  esac
  prev="$a"
done

# Normalise machine-specific prefixes out of the source path. Registry paths
# carry the crate version (…/getrandom-0.2.16/src/lib.rs), which is exactly the
# discriminator we need between multiple versions of one crate.
norm_src="$src"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
sysroot="$("$rustc_bin" --print sysroot 2>/dev/null || true)"
norm_src="${norm_src//$cargo_home//cargo-home}"
[ -n "$sysroot" ] && norm_src="${norm_src//$sysroot//sysroot}"
[ -n "${TOR_JS_WORKSPACE_ROOT:-}" ] && norm_src="${norm_src//$TOR_JS_WORKSPACE_ROOT//workspace}"

# Sort cfgs: cargo's ordering is stable in practice, but this costs nothing and
# removes it as a variable.
sorted_cfgs="$(printf '%s\n' ${cfgs[@]+"${cfgs[@]}"} | LC_ALL=C sort | tr '\n' ',')"
identity="name=$crate_name|types=${crate_types[*]-}|edition=$edition|target=$target_triple|cfgs=$sorted_cfgs|src=$norm_src"
key="$(printf '%s' "$identity" | sha256sum | cut -c1-32)"

if [ -n "${TOR_JS_RUSTC_KEYLOG:-}" ]; then
  printf '%s\t%s\n' "$key" "$identity" >> "$TOR_JS_RUSTC_KEYLOG"
fi

# --- rewrite -C metadata to the derived key --------------------------------
# extra-filename is deliberately left alone: it only names output files, and
# those must stay distinct per unit for cargo's own bookkeeping.
args=()
for a in "$@"; do
  case "$a" in
    metadata=*)           args+=("metadata=$key") ;;
    -Cmetadata=*)         args+=("-Cmetadata=$key") ;;
    --codegen=metadata=*) args+=("--codegen=metadata=$key") ;;
    *)                    args+=("$a") ;;
  esac
done

exec "$rustc_bin" "${args[@]}"
