#!/usr/bin/env bash
# Build the s3-endpoint compose:dynlink/endpoint provider component.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "==> Building s3-endpoint (wasm32-wasip2, release)"
cargo build --release --target wasm32-wasip2

ARTIFACT="target/wasm32-wasip2/release/s3_endpoint.wasm"
echo "    component: $SCRIPT_DIR/$ARTIFACT"
echo "==> Exported world:"
wasm-tools component wit "$ARTIFACT" | grep -E 'export compose:dynlink/endpoint' || true
