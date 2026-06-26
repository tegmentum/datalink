#!/usr/bin/env python3
"""Stamp the content-addressed identity into every registry entry.

The gen half of the shared identity engine (lifted from ducklink's
tooling/gen-catalog.py stamping logic, parameterized via the per-repo config).
For every entry whose artifact is present it writes, idempotently:

  * wit_contract          -- the witcanon digest of the contract WIT (all entries
                             share one contract, so all get the same digest)
  * wit_contract_version  -- the human-readable contract version (config)
  * content_digest        -- sha256 of the entry's deployed .wasm (only when the
                             artifact is present; a missing artifact leaves any
                             existing content_digest untouched, exactly like
                             ducklink's gen-catalog)

Rewrites the index only if something changed, preserving the existing JSON
formatting (2-space indent + trailing newline) and adding only those fields.

  gen.py [--config PATH]            # stamp the registry in place
  gen.py [--config PATH] --check    # report drift, write nothing (exit 1 if any)

stdlib only; matches the existing tooling style.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dlconfig  # noqa: E402
import identity  # noqa: E402
import registry  # noqa: E402


def _index_path(cfg) -> Path:
    rel = cfg.get("registry", "index_path")
    if not rel:
        raise SystemExit("error: config.registry.index_path is not set")
    return cfg.path(rel)


def _entries(data, cfg):
    """Return the in-place list of entry dicts to stamp, for the configured
    entries_key. Returns the list object so edits land in `data`."""
    key = cfg.get("registry", "entries_key", default="extensions")
    if key:
        return data.get(key, [])
    return data if isinstance(data, list) else []


def stamp(cfg, check: bool = False) -> int:
    path = _index_path(cfg)
    if not path.is_file():
        raise SystemExit(f"error: catalog index not found: {path}")
    with path.open() as f:
        data = json.load(f)

    digest = identity.witcanon_digest(cfg)
    version = identity.contract_version(cfg)
    entries = _entries(data, cfg)

    changed = 0
    stamped_content = 0
    for e in entries:
        if not isinstance(e, dict):
            continue
        name = e.get("name")
        if not name:
            continue
        if e.get("wit_contract") != digest:
            e["wit_contract"] = digest
            changed += 1
        if e.get("wit_contract_version") != version:
            e["wit_contract_version"] = version
            changed += 1
        # CONTENT identity: only when the artifact is present (tolerate missing,
        # like ducklink's gen-catalog -- don't drop an existing content_digest).
        art = identity.artifact_path(cfg, name)
        if art.exists():
            cd = identity.content_digest(art)
            if e.get("content_digest") != cd:
                e["content_digest"] = cd
                changed += 1
            stamped_content += 1

    if check:
        if changed:
            print(f"DRIFT  {changed} field(s) would change "
                  f"(contract {digest[:12]}…); run gen.py to stamp", file=sys.stderr)
            return 1
        print(f"OK  registry up to date (contract {digest[:12]}…, "
              f"{stamped_content} content_digest(s))")
        return 0

    if changed:
        with path.open("w") as fh:
            json.dump(data, fh, indent=2)
            fh.write("\n")
        print(f"stamped contract digest {digest[:12]}… + {stamped_content} "
              f"content_digest(s) into {cfg.rel(path)} ({changed} field(s) changed)")
    else:
        print(f"unchanged — contract {digest[:12]}…, "
              f"{stamped_content} content_digest(s) already current")
    return 0


def main() -> None:
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    dlconfig.add_config_arg(p)
    p.add_argument("--check", action="store_true",
                   help="report drift without writing (exit 1 if anything would change)")
    args = p.parse_args()
    cfg = dlconfig.load(args.config)
    sys.exit(stamp(cfg, check=args.check))


if __name__ == "__main__":
    main()
