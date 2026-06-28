#!/usr/bin/env bash
# Build the gdal-endpoint compose:dynlink/endpoint provider, then wac-compose
# it ONCE with the prebuilt GDAL component to produce the single resident
# provider artifact `gdal-provider.wasm`.
#
# Result: a self-contained provider that exports only compose:dynlink/endpoint
# (+ imports WASI). GDAL/PROJ + proj.db are bundled into the provider artifact
# ONCE (like pylon bundles CPython+numpy) — the host loads it once and shares
# it as a resident across every geo extension, instead of GDAL being
# wac-inlined into each consumer at build time.
#
# NOTE: the prebuilt gdal.component.wasm uses a few WIT identifiers with a
# digit-leading label segment (e.g. `get-extent-3d`, `promote-to-3d`). wac /
# wasm-tools tolerate them, but pinned wasmtime builds (e.g. the ducklink host's
# 39.0.0) reject them as non-kebab extern names. We rename those substrings in
# the gdal component binary to a same-length kebab-valid form (`-3d`->`-d3`,
# `-2d`->`-d2`) before composing; the rename is consistent across extern names,
# core export names and the embedded WIT type section, and gdal-endpoint never
# calls any renamed function (it uses only gdal:core/srs).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

GDAL="${GDAL_COMPONENT:-$HOME/git/gdal-wasm/build/bin/gdal.component.wasm}"
RAW="target/wasm32-wasip2/release/gdal_endpoint.wasm"
OUT="${OUT:-gdal-provider.wasm}"
GDAL_FIXED="$(mktemp -t gdal-renamed.XXXXXX).wasm"
trap 'rm -f "$GDAL_FIXED"' EXIT

echo "==> Building gdal-endpoint (wasm32-wasip2, release)"
cargo build --release --target wasm32-wasip2

[[ -f "$RAW" ]] || { echo "missing $RAW" >&2; exit 1; }
[[ -f "$GDAL" ]] || { echo "GDAL component not found at $GDAL" >&2; exit 1; }

echo "==> Pre-composition import (typed gdal:core/srs):"
wasm-tools component wit "$RAW" | grep -E 'import gdal|export compose:dynlink/endpoint' || true

echo "==> Renaming digit-leading WIT labels in the GDAL component"
python3 - "$GDAL" "$GDAL_FIXED" <<'PY'
import sys
src, dst = sys.argv[1], sys.argv[2]
data = open(src, "rb").read()
repls = [
    (b"get-extent-3d", b"get-extent-d3"), (b"get_extent_3d", b"get_extent_d3"),
    (b"promote-to-3d", b"promote-to-d3"), (b"promote_to_3d", b"promote_to_d3"),
    (b"demote-to-2d",  b"demote-to-d2"),  (b"demote_to_2d",  b"demote_to_d2"),
    (b"distance-3d",   b"distance-d3"),   (b"distance_3d",   b"distance_d3"),
    (b"envelope-3d",   b"envelope-d3"),   (b"envelope_3d",   b"envelope_d3"),
    (b"flatten-to-2d", b"flatten-to-d2"), (b"flatten_to_2d", b"flatten_to_d2"),
    (b"set-point-2d",  b"set-point-d2"),  (b"set_point_2d",  b"set_point_d2"),
    (b"add-point-2d",  b"add-point-d2"),  (b"add_point_2d",  b"add_point_d2"),
    (b"is-3d",         b"is-d3"),         (b"is_3d",         b"is_d3"),
]
for a, b in repls:
    assert len(a) == len(b)
    data = data.replace(a, b)
open(dst, "wb").write(data)
PY

echo "==> wac plug gdal-endpoint <- gdal.component.wasm -> $OUT"
wac plug "$RAW" --plug "$GDAL_FIXED" -o "$OUT"

echo "==> Composed resident provider: $SCRIPT_DIR/$OUT ($(wc -c < "$OUT") bytes)"
echo "==> Post-composition world (should export endpoint, import only wasi):"
wasm-tools component wit "$OUT" | grep -E 'import gdal|import wasi|export compose:dynlink/endpoint' | sort -u || true
