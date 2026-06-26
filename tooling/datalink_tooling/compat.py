#!/usr/bin/env python3
"""Reader for the per-repo compat-registry.json (upstream-crate wasm32
status). The `_schema` (status_values + meaning) is the SHARED contract;
the per-crate data is per-repo and stays in each repo's
tooling/compat-registry.json.

Used standalone and as a library by scaffold.py.

Usage:
  compat.py [--config PATH] --list-broken   # crates not clean/unverified
  compat.py [--config PATH] --check NAME     # status + notes for one crate
  compat.py [--config PATH] --validate       # check data conforms to _schema
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from . import dlconfig  # noqa: E402


def load_registry(cfg: dlconfig.Config) -> dict:
    rel = cfg.get("compat_registry", "path", default="tooling/compat-registry.json")
    path = cfg.path(rel)
    if not path.is_file():
        raise SystemExit(f"error: compat-registry not found: {path}")
    with path.open() as f:
        return json.load(f)


def check_crate(name: str, registry: dict) -> tuple[str, str]:
    """Return (status, notes); 'unverified' if unseen."""
    entry = registry.get("crates", {}).get(name)
    if not entry:
        return ("unverified", "")
    return (entry.get("status", "unverified"), entry.get("notes", ""))


def suggest_version(name: str, registry: dict) -> str:
    entry = registry.get("crates", {}).get(name)
    if not entry or "version_tested" not in entry:
        return "*"
    parts = entry["version_tested"].split(".")
    if len(parts) >= 2:
        return f"{parts[0]}.{parts[1]}"
    return parts[0]


def list_broken(registry: dict) -> list[tuple[str, str, str]]:
    rows = []
    for crate, entry in registry.get("crates", {}).items():
        status = entry.get("status", "unverified")
        if status not in ("clean", "unverified"):
            rows.append((status, crate, entry.get("notes", "")[:80]))
    rows.sort()
    return rows


def validate(registry: dict) -> list[str]:
    """Check the data conforms to the shared _schema (status values known)."""
    errs: list[str] = []
    schema = registry.get("_schema", {})
    allowed = set(schema.get("status_values", {}).keys())
    if not allowed:
        errs.append("_schema.status_values missing or empty")
        return errs
    for crate, entry in registry.get("crates", {}).items():
        st = entry.get("status", "unverified")
        if st not in allowed:
            errs.append(f"{crate}: unknown status {st!r} (allowed: {sorted(allowed)})")
    return errs


def main(config: str | None = None, argv=None) -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    dlconfig.add_config_arg(p, default=config)
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--list-broken", action="store_true")
    g.add_argument("--check", metavar="NAME")
    g.add_argument("--validate", action="store_true")
    args = p.parse_args(argv)

    cfg = dlconfig.load(args.config)
    registry = load_registry(cfg)

    if args.check:
        status, notes = check_crate(args.check, registry)
        print(f"{args.check}: {status}")
        if notes:
            print(f"  {notes}")
        return

    if args.list_broken:
        rows = list_broken(registry)
        if not rows:
            print("no flagged crates  registry is clean")
            return
        width = max(len(c) for _, c, _ in rows)
        for status, crate, note in rows:
            print(f"  {status:20s} {crate:<{width}}  {note}")
        return

    if args.validate:
        errs = validate(registry)
        if errs:
            for e in errs:
                print(f"FAIL  {e}", file=sys.stderr)
            sys.exit(1)
        n = len(registry.get("crates", {}))
        print(f"OK  {n} crate entries conform to _schema")


if __name__ == "__main__":
    main()
