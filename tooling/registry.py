#!/usr/bin/env python3
"""Reader + common-field validator for the catalog index.

The catalog index entries share a DB-agnostic CORE schema:
  name, version, description, license, authors, repository,
  keywords, categories, source, exports
diverging only in DB-tagged fields, which this reader treats as
opaque pass-through:
  min_duckdb_version / min_sqlite_version, oci_artifact / artifact,
  wit_contract, prefix / expansion, checksum / size_bytes, ...

IMPORTANT — DEFERRED: this reader does NOT verify content_digest /
witcanon contract digests. That content-addressed identity check is the
separate `identity` module (witcanon + content_digest), deferred until
ducklink's Phase 1 identity work lands (it currently owns gen-catalog.py,
verify-catalog.py, and registry/index.json). registry.py only reads the
index read-only for COMMON-field presence/shape.

Usage:
  registry.py [--config PATH] --list                # entry names + versions
  registry.py [--config PATH] --validate            # core fields present + typed
  registry.py [--config PATH] --show NAME           # dump one entry
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dlconfig  # noqa: E402

# The DB-agnostic CORE fields every catalog entry shares. (min_*_version,
# oci_artifact/artifact, wit_contract, prefix/expansion, checksum/size_bytes
# are DB-specific extension points, validated only as opaque pass-through.)
CORE_REQUIRED = ["name", "version", "description", "license"]
CORE_OPTIONAL = ["authors", "repository", "homepage", "keywords",
                 "categories", "source", "exports", "dependencies"]
# Fields that are DB-specific and intentionally NOT validated here.
DB_SPECIFIC_PASSTHROUGH = [
    "min_duckdb_version", "min_sqlite_version", "oci_artifact", "artifact",
    "wit_contract", "prefix", "expansion", "checksum", "size_bytes",
    "content_digest", "function_id_start",
]

LIST_FIELDS = ["authors", "keywords", "categories", "exports", "dependencies"]


def load_entries(cfg: dlconfig.Config) -> list[dict]:
    rel = cfg.get("registry", "index_path")
    if not rel:
        raise SystemExit("error: config.registry.index_path is not set")
    path = cfg.path(rel)
    if not path.is_file():
        raise SystemExit(f"error: catalog index not found: {path}")
    with path.open() as f:
        data = json.load(f)
    key = cfg.get("registry", "entries_key", default="extensions")
    if key:
        entries = data.get(key, [])
    else:
        entries = data
    if isinstance(entries, dict):
        # map of name -> entry
        out = []
        for name, ent in entries.items():
            ent = dict(ent)
            ent.setdefault("name", name)
            out.append(ent)
        return out
    return list(entries)


def validate_entry(entry: dict) -> list[str]:
    errs: list[str] = []
    name = entry.get("name", "<unnamed>")
    for f in CORE_REQUIRED:
        if f not in entry:
            errs.append(f"{name}: missing required core field {f!r}")
    for f in LIST_FIELDS:
        if f in entry and not isinstance(entry[f], list):
            errs.append(f"{name}: field {f!r} should be a list, got {type(entry[f]).__name__}")
    return errs


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    dlconfig.add_config_arg(p)
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--list", action="store_true")
    g.add_argument("--validate", action="store_true")
    g.add_argument("--show", metavar="NAME")
    args = p.parse_args()

    cfg = dlconfig.load(args.config)
    entries = load_entries(cfg)

    if args.list:
        for e in sorted(entries, key=lambda x: x.get("name", "")):
            print(f"  {e.get('name','<unnamed>'):<28} {e.get('version','?')}")
        print(f"\n{len(entries)} entries")
        return

    if args.show:
        for e in entries:
            if e.get("name") == args.show:
                print(json.dumps(e, indent=2))
                return
        sys.exit(f"error: no entry named {args.show!r}")

    if args.validate:
        all_errs: list[str] = []
        for e in entries:
            all_errs.extend(validate_entry(e))
        if all_errs:
            for e in all_errs:
                print(f"FAIL  {e}", file=sys.stderr)
            sys.exit(1)
        print(f"OK  {len(entries)} entries have valid CORE fields "
              f"(DB-specific + identity fields not checked here)")


if __name__ == "__main__":
    main()
