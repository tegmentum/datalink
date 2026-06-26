#!/usr/bin/env python3
"""Shared, DB-agnostic content-addressed identity computation.

The Tier-1 identity engine, lifted from ducklink's Phase-1 scheme
(tooling/gen-catalog.py + tooling/verify-catalog.py, finalized for the
`duckdb:extension` contract) and generalized via the per-repo config so the
same code serves sqlink's `sqlink:wasm` contract (and any future engine).

TWO digest schemes, both reimplemented byte-identically from the orchestration
framework's `compose-core::blobs` (in ~/git/webassembly-component-orchestration,
SPEC §4.1), as trivial Python tooling (no Rust dep) so the values interoperate
with the framework's blob store / trust model:

  * witcanon (CONTRACT / shape identity) -- compose-core::blobs::compute_wit_digest
        digest = sha256(b"witcanon:1" || bytes)
    where `bytes` = the canonical WIT files of the contract package, sorted by
    filename, concatenated. Hex. Changes iff the WIT shape changes; truly
    reproducible, so it is the DEFAULT-enforced identity. Mirrors ducklink's
    crates/ducklink-runtime/build.rs (which embeds the same value as a const).

  * content (BYTE identity) -- compose-core::blobs::compute_digest
        digest = sha256(bytes)
    of a component's OWN .wasm bytes. Hex. Re-stamped per deploy; enforced only
    under an opt-in flag because wasm builds are byte-reproducible within a fixed
    toolchain but NOT across rustc / cargo-component versions.

Parameterized entirely by config.identity (see config.schema.json):
  wit_source_dir   -- the canonical contract WIT dir (*.wit, sorted, hashed)
  contract_package -- the WIT package id parsed from a built artifact's imports
  contract_major   -- the contract major the catalog targets
  contract_version -- the human-readable contract version stamped into entries
  artifacts_dir    -- where <name>.wasm deployed artifacts live
  wasm_tools_bin   -- the `wasm-tools` binary used for the @MAJOR cross-check

stdlib only; matches the existing tooling style.
"""

from __future__ import annotations

import hashlib
import re
import subprocess
from pathlib import Path

# ---- the two digest schemes (compose-core::blobs, reimplemented) ----------


def witcanon_digest_bytes(wit_bytes: bytes) -> str:
    """compute_wit_digest: sha256(b"witcanon:1" || bytes), hex.

    Byte-identical to compose-core::blobs::compute_wit_digest and to
    crates/ducklink-runtime/build.rs (the witcanon:1 scheme)."""
    return hashlib.sha256(b"witcanon:1" + wit_bytes).hexdigest()


def witcanon_digest(cfg) -> str:
    """The AUTHORITATIVE content-addressed CONTRACT identity: the witcanon
    digest over the canonical contract WIT files in config.identity.wit_source_dir
    (every top-level *.wit, read in sorted-by-filename order and concatenated).

    The set of files == the shared contract package (for ducklink: the 16
    wit/duckdb-extension/*.wit files; for sqlink: wit/*.wit, the sqlink:wasm
    package). `deps.toml` and any non-.wit file are excluded by the glob, exactly
    as ducklink's gen-catalog / build.rs do."""
    wit_dir = identity_wit_source_dir(cfg)
    if not wit_dir.is_dir():
        raise SystemExit(
            f"error: contract WIT dir not found: {wit_dir} "
            f"(config.identity.wit_source_dir)"
        )
    buf = b"".join(p.read_bytes() for p in sorted(wit_dir.glob("*.wit")))
    if not buf:
        raise SystemExit(f"error: no *.wit files under {wit_dir}")
    return witcanon_digest_bytes(buf)


def content_digest(artifact_path) -> str:
    """compute_digest: sha256(bytes) of a component's OWN .wasm, hex.

    Byte-identical to compose-core::blobs::compute_digest. The CONTENT identity
    of the actual binary (distinct from the witcanon CONTRACT identity)."""
    return hashlib.sha256(Path(artifact_path).read_bytes()).hexdigest()


# ---- the @MAJOR cross-check (mirrors verify-catalog.component_contract) ----


def _import_re(package: str) -> re.Pattern:
    # `import <package>/<iface>[@MAJOR.MINOR.PATCH]` -- the package is
    # parameterized (duckdb:extension vs sqlink:wasm). The interface name segment
    # matches ducklink's [A-Za-z0-9\-]+.
    pkg = re.escape(package)
    return re.compile(
        rf"\bimport\s+{pkg}/[A-Za-z0-9\-]+(?:@([0-9]+\.[0-9]+\.[0-9]+))?"
    )


def imported_contract_version(artifact_path, package: str, host_bin: str = "wasm-tools"):
    """The contract version a built component imports, read from
    `wasm-tools component wit <artifact>`. Returns the version string (e.g.
    '2.0.0'), 'unversioned' for a legacy pre-versioning component, or None if the
    package isn't imported / wasm-tools is unavailable.

    Mirrors verify-catalog.py::component_contract, with `package` and the
    wasm-tools binary parameterized."""
    try:
        out = subprocess.run(
            [host_bin or "wasm-tools", "component", "wit", str(artifact_path)],
            capture_output=True, text=True, check=True,
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    rx = _import_re(package)
    versioned = None
    saw_unversioned = False
    for m in rx.finditer(out):
        if m.group(1):
            versioned = m.group(1)
            break
        saw_unversioned = True
    if versioned:
        return versioned
    return "unversioned" if saw_unversioned else None


def imported_contract_major(artifact_path, package: str, host_bin: str = "wasm-tools"):
    """The MAJOR of the contract version the built component imports, or None
    (or 'unversioned'). Convenience wrapper over imported_contract_version for
    the @MAJOR cross-check."""
    v = imported_contract_version(artifact_path, package, host_bin)
    if v is None or v == "unversioned":
        return v
    return v.split(".")[0]


# ---- config accessors (defaults mirror ducklink/sqlink shape) -------------


def identity_wit_source_dir(cfg) -> Path:
    """Canonical contract WIT dir. Prefer config.identity.wit_source_dir; fall
    back to config.wit.source_dir for repos that reuse one dir."""
    rel = cfg.get("identity", "wit_source_dir") or cfg.get("wit", "source_dir")
    if not rel:
        raise SystemExit(
            "error: config.identity.wit_source_dir (or wit.source_dir) is not set"
        )
    return cfg.path(rel)


def contract_package(cfg) -> str:
    pkg = cfg.get("identity", "contract_package") or cfg.get("wit", "package")
    if not pkg:
        raise SystemExit(
            "error: config.identity.contract_package (or wit.package) is not set"
        )
    return pkg


def contract_major(cfg) -> str:
    cm = cfg.get("identity", "contract_major")
    if cm is not None:
        return str(cm)
    cv = contract_version(cfg)
    return cv.split(".")[0]


def contract_version(cfg) -> str:
    cv = cfg.get("identity", "contract_version")
    if not cv:
        raise SystemExit("error: config.identity.contract_version is not set")
    return str(cv)


def artifacts_dir(cfg) -> Path:
    rel = (cfg.get("identity", "artifacts_dir")
           or cfg.get("smoke", "extensions_dir"))
    if not rel:
        raise SystemExit(
            "error: config.identity.artifacts_dir (or smoke.extensions_dir) "
            "is not set"
        )
    return cfg.path(rel)


def wasm_tools_bin(cfg) -> str:
    return cfg.get("identity", "wasm_tools_bin", default="wasm-tools")


def artifact_path(cfg, name: str) -> Path:
    """Deployed <name>.wasm under artifacts_dir."""
    return artifacts_dir(cfg) / f"{name}.wasm"


if __name__ == "__main__":
    import argparse
    import sys

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import dlconfig  # noqa: E402

    p = argparse.ArgumentParser(
        description="Print the witcanon contract digest (and optionally a content "
                    "digest) for a repo, from its datalink config.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    dlconfig.add_config_arg(p)
    p.add_argument("--content", metavar="ARTIFACT",
                   help="also print content_digest(ARTIFACT) (sha256 of the .wasm)")
    args = p.parse_args()
    cfg = dlconfig.load(args.config)
    print(f"witcanon_digest({contract_package(cfg)}) = {witcanon_digest(cfg)}")
    if args.content:
        ap = Path(args.content)
        if not ap.is_absolute():
            ap = cfg.path(args.content)
        print(f"content_digest({ap.name}) = {content_digest(ap)}")
