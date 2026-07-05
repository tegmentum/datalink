#!/usr/bin/env bash
# Stable build recipe for Phase 3 (#823) dynlink bridges.
#
# Emits + compiles a datafission bridge component per postgis
# sub-extension (postgis_core, postgis_sfcgal, postgis_raster,
# postgis_format_encoders) and stages the .wasm files under a
# consumer-visible output directory. Downstream callers feed the paths
# into `PluginLoaderConfig::sub_ext_bridge_paths` so `CREATE EXTENSION
# postgis_<sub_ext>` registers real SQL functions instead of stopping at
# provider materialization.
#
# The bridges are NOT committed to the tree — each is ~1.3 MB; four of
# them would add ~5 MB per contributor. Users produce them once, then
# reuse.
#
# Usage:
#   ./scripts/build-bridges.sh
#     — default paths: reads $HOME/git/postgis-shim-interface/postgis-interface.sqlite,
#       writes to $HOME/git/datafission/extensions/postgis-dynlink-bridges/
#
#   INTERFACE_DB=/path/db.sqlite OUT_DIR=/path/out ./scripts/build-bridges.sh
#     — override via env
#
#   ./scripts/build-bridges.sh --only postgis_core
#     — build just one sub-ext (repeatable). Anything after --once is
#       passed straight through to the underlying binary.
#
# Prereqs:
#   * postgis-shim-interface repo cloned next to this one (or
#     INTERFACE_DB pointed at the .sqlite).
#   * `wasm32-wasip2` rustup target installed.
#   * cargo on PATH.
#
# The heavy lifting lives in the `build_bridges` binary in this crate;
# this script exists so a plain "run this to get the bridges" line in
# the README doesn't need to know cargo argv shape.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

INTERFACE_DB="${INTERFACE_DB:-${HOME}/git/postgis-shim-interface/postgis-interface.sqlite}"
OUT_DIR="${OUT_DIR:-${HOME}/git/datafission/extensions/postgis-dynlink-bridges}"

if [[ ! -f "${INTERFACE_DB}" ]]; then
  echo "error: interface DB missing at ${INTERFACE_DB}" >&2
  echo "  set INTERFACE_DB=<path> or clone tegmentum/postgis-shim-interface next to this repo" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not on PATH" >&2
  exit 1
fi

if ! rustup target list --installed 2>/dev/null | grep -q '^wasm32-wasip2$'; then
  echo "warning: wasm32-wasip2 target not installed. Attempting: rustup target add wasm32-wasip2" >&2
  rustup target add wasm32-wasip2
fi

mkdir -p "${OUT_DIR}"

: "${CARGO_NET_GIT_FETCH_WITH_CLI:=true}"
export CARGO_NET_GIT_FETCH_WITH_CLI

echo "== building datafission dynlink bridges ==" >&2
echo "  interface db : ${INTERFACE_DB}" >&2
echo "  output dir   : ${OUT_DIR}" >&2
echo "  extra args   : $*" >&2

cd "${CRATE_DIR}"
exec cargo run --release --bin build_bridges -- \
  --interface-db "${INTERFACE_DB}" \
  --out          "${OUT_DIR}" \
  "$@"
