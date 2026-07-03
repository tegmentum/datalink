#!/usr/bin/env bash
# Build the compression-endpoint compose:dynlink/endpoint provider component.
# libzstd (zstd-sys) is C, so the wasm32-wasip2 CC/AR/CFLAGS must point at the
# wasi-sdk clang. Built direct to wasip2 (no cargo-component wasip1 transform).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

WASI_SDK="${WASI_SDK:-$HOME/wasi-sdk}"
export CC_wasm32_wasip2="$WASI_SDK/bin/clang"
export AR_wasm32_wasip2="$WASI_SDK/bin/ar"
export CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK/share/wasi-sysroot -target wasm32-wasip2 -msimd128"

echo "==> Building compression-endpoint (wasm32-wasip2, release)"
cargo build --release --target wasm32-wasip2

ARTIFACT="target/wasm32-wasip2/release/compression_endpoint.wasm"
echo "    component: $SCRIPT_DIR/$ARTIFACT"
echo "==> Exported world:"
wasm-tools component wit "$ARTIFACT" | grep -E 'export compose:dynlink/endpoint' || true
