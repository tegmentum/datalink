#!/usr/bin/env bash
# Stable build recipe for Phase 3 (#823) dynlink bridges.
#
# Emits + compiles a datafission bridge component per postgis
# sub-extension (postgis_core, postgis_sfcgal, postgis_raster,
# postgis_format_encoders) plus the mobilitydb single-provider
# bridge (mobilitydb is monolithic per Agent #887's recon; no
# `_core / _sfcgal / _raster / _format_encoders` split), and stages
# the .wasm files under a consumer-visible output directory.
# Downstream callers feed the paths into
# `PluginLoaderConfig::sub_ext_bridge_paths` so `CREATE EXTENSION
# postgis_<sub_ext>` / `CREATE EXTENSION mobilitydb` registers real
# SQL functions instead of stopping at provider materialization.
#
# The bridges are NOT committed to the tree — each is ~1.3 MB; five
# of them would add ~6 MB per contributor. Users produce them once,
# then reuse.
#
# Usage:
#   ./scripts/build-bridges.sh
#     — default paths: reads $HOME/git/postgis-shim-interface/postgis-interface.sqlite
#       + $HOME/git/mobilitydb-shim-interface/mobilitydb-interface.sqlite,
#       writes to $HOME/git/datafission/extensions/dynlink-bridges/
#
#   POSTGIS_INTERFACE_DB=/path/db.sqlite \
#   MOBILITYDB_INTERFACE_DB=/path/db.sqlite \
#   OUT_DIR=/path/out \
#     ./scripts/build-bridges.sh
#     — override via env
#
#   ./scripts/build-bridges.sh --only postgis_core
#     — build just one sub-ext (repeatable). Anything after --only is
#       passed straight through to the underlying binary.
#
#   ./scripts/build-bridges.sh --only mobilitydb
#     — build just the mobilitydb bridge (skips postgis DB check).
#
# Env var back-compat: `INTERFACE_DB` (pre-mobilitydb single-DB name)
# is honored as an alias for `POSTGIS_INTERFACE_DB`.
#
# Prereqs:
#   * postgis-shim-interface + mobilitydb-shim-interface repos cloned
#     next to this one (or the corresponding env vars pointed at the
#     .sqlite files). `--only <sub_ext>` skips the check for DBs that
#     no selected target reads from.
#   * `wasm32-wasip2` rustup target installed.
#   * cargo on PATH.
#
# The heavy lifting lives in the `build_bridges` binary in this crate;
# this script exists so a plain "run this to get the bridges" line in
# the README doesn't need to know cargo argv shape.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Postgis interface DB. Honor the legacy `INTERFACE_DB` env var too
# so pre-mobilitydb callers keep working without churn.
POSTGIS_INTERFACE_DB="${POSTGIS_INTERFACE_DB:-${INTERFACE_DB:-${HOME}/git/postgis-shim-interface/postgis-interface.sqlite}}"
MOBILITYDB_INTERFACE_DB="${MOBILITYDB_INTERFACE_DB:-${HOME}/git/mobilitydb-shim-interface/mobilitydb-interface.sqlite}"
OUT_DIR="${OUT_DIR:-${HOME}/git/datafission/extensions/dynlink-bridges}"

# Detect a targeted `--only` invocation so we can skip the DB-file
# existence check for a flavor no selected target actually uses.
# (`--only mobilitydb` on a machine without postgis-shim-interface
# shouldn't refuse to run just because the postgis DB is missing.)
ONLY_ARGS=()
NEEDS_POSTGIS=1
NEEDS_MOBILITYDB=1
if [[ $# -gt 0 ]]; then
    NEEDS_POSTGIS=0
    NEEDS_MOBILITYDB=0
    args=("$@")
    i=0
    while [[ $i -lt ${#args[@]} ]]; do
        if [[ "${args[$i]}" == "--only" ]]; then
            i=$((i+1))
            if [[ $i -ge ${#args[@]} ]]; then
                echo "error: --only requires a value" >&2
                exit 1
            fi
            ONLY_ARGS+=("--only" "${args[$i]}")
            case "${args[$i]}" in
                postgis_*) NEEDS_POSTGIS=1 ;;
                mobilitydb) NEEDS_MOBILITYDB=1 ;;
                *)
                    echo "warning: --only ${args[$i]}: unrecognized sub-ext (will be validated by underlying binary)" >&2
                    NEEDS_POSTGIS=1
                    NEEDS_MOBILITYDB=1
                    ;;
            esac
        fi
        i=$((i+1))
    done
    if [[ ${#ONLY_ARGS[@]} -eq 0 ]]; then
        # No --only flags; the full target set will be built, so
        # every DB is required.
        NEEDS_POSTGIS=1
        NEEDS_MOBILITYDB=1
    fi
fi

if [[ "$NEEDS_POSTGIS" -eq 1 && ! -f "${POSTGIS_INTERFACE_DB}" ]]; then
    echo "error: postgis interface DB missing at ${POSTGIS_INTERFACE_DB}" >&2
    echo "  set POSTGIS_INTERFACE_DB=<path> or clone tegmentum/postgis-shim-interface next to this repo" >&2
    exit 1
fi

if [[ "$NEEDS_MOBILITYDB" -eq 1 && ! -f "${MOBILITYDB_INTERFACE_DB}" ]]; then
    echo "error: mobilitydb interface DB missing at ${MOBILITYDB_INTERFACE_DB}" >&2
    echo "  set MOBILITYDB_INTERFACE_DB=<path> or clone tegmentum/mobilitydb-shim-interface next to this repo" >&2
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
echo "  postgis db    : ${POSTGIS_INTERFACE_DB}" >&2
echo "  mobilitydb db : ${MOBILITYDB_INTERFACE_DB}" >&2
echo "  output dir    : ${OUT_DIR}" >&2
echo "  extra args    : $*" >&2

cd "${CRATE_DIR}"
exec cargo run --release --bin build_bridges -- \
  --postgis-interface-db    "${POSTGIS_INTERFACE_DB}" \
  --mobilitydb-interface-db "${MOBILITYDB_INTERFACE_DB}" \
  --out                     "${OUT_DIR}" \
  "$@"
