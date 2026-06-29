#!/usr/bin/env bash
# Build the redis-endpoint compose:dynlink/endpoint provider component.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "==> Building redis-endpoint (wasm32-wasip2, release)"
cargo build --release --target wasm32-wasip2

ARTIFACT="target/wasm32-wasip2/release/redis_endpoint.wasm"
echo "    component: $SCRIPT_DIR/$ARTIFACT"
echo "==> Exported world:"
wasm-tools component wit "$ARTIFACT" | grep -E 'export compose:dynlink/endpoint' || true
echo "==> wasi:sockets import (proves the socket-client path):"
wasm-tools component wit "$ARTIFACT" | grep -E 'import wasi:sockets/tcp' || true
