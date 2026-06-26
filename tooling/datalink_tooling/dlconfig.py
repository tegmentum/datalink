#!/usr/bin/env python3
"""Shared config loader for the datalink tooling engine.

Every engine script (scaffold.py, smoke.py, t-status.py, compat.py,
registry.py) drives its DB-specific behaviour from a per-repo config
file matching config.schema.json. This module locates + loads that
config and resolves repo_root + relative paths.

Discovery order (when --config is not given):
  1. $DATALINK_CONFIG
  2. ./datalink.config.json
  3. ./tooling/datalink.config.json
  4. walk up from cwd looking for tooling/datalink.config.json

stdlib only; matches the existing tooling style.
"""

from __future__ import annotations

import json
import os
from pathlib import Path


class Config:
    """A loaded per-repo config with path-resolution helpers."""

    def __init__(self, data: dict, config_path: Path):
        self.data = data
        self.config_path = config_path
        self.repo_root = self._resolve_repo_root()

    def _resolve_repo_root(self) -> Path:
        rr = self.data.get("repo_root")
        if rr:
            p = Path(rr)
            if not p.is_absolute():
                p = (self.config_path.parent / p).resolve()
            return p
        # Default: parent of the dir holding the config. When the config
        # lives in <repo>/tooling/ this is <repo>.
        return self.config_path.parent.parent.resolve()

    # ---- accessors -------------------------------------------------

    def get(self, *keys, default=None):
        """Nested get: cfg.get('smoke', 'argv')."""
        node = self.data
        for k in keys:
            if not isinstance(node, dict) or k not in node:
                return default
            node = node[k]
        return node

    def path(self, rel: str | None) -> Path | None:
        """Resolve a repo-root-relative (or absolute) path string."""
        if rel is None:
            return None
        p = Path(rel)
        return p if p.is_absolute() else (self.repo_root / p)

    def rel(self, p: Path) -> str:
        """Best-effort path relative to repo_root for display."""
        try:
            return str(p.relative_to(self.repo_root))
        except ValueError:
            return str(p)

    @property
    def db_name(self) -> str:
        return self.data.get("db_name", "")

    @property
    def registration_abi(self) -> str:
        return self.data.get("registration_abi", "")


def _discover() -> Path | None:
    env = os.environ.get("DATALINK_CONFIG")
    if env:
        return Path(env)
    cwd = Path.cwd()
    for cand in (cwd / "datalink.config.json",
                 cwd / "tooling" / "datalink.config.json"):
        if cand.is_file():
            return cand
    for parent in [cwd, *cwd.parents]:
        cand = parent / "tooling" / "datalink.config.json"
        if cand.is_file():
            return cand
    return None


def load(explicit: str | None = None) -> Config:
    """Load the config. `explicit` is the --config value if provided."""
    path = Path(explicit) if explicit else _discover()
    if path is None:
        raise SystemExit(
            "error: no datalink config found. Pass --config <path>, set "
            "$DATALINK_CONFIG, or run from a repo with "
            "tooling/datalink.config.json"
        )
    if not path.is_file():
        raise SystemExit(f"error: config not found: {path}")
    with path.open() as f:
        data = json.load(f)
    return Config(data, path.resolve())


def add_config_arg(parser, default: str | None = None) -> None:
    """Register the standard --config argument on an argparse parser.

    `default` is the fallback config path a consuming repo's thin delegator
    injects (e.g. `tooling/datalink.config.json`). An explicit `--config` on the
    command line always overrides it; if neither is given the loader falls back
    to its own discovery (env / cwd walk)."""
    parser.add_argument(
        "--config",
        metavar="PATH",
        default=default,
        help="path to a datalink config (config.schema.json). "
             "Defaults to $DATALINK_CONFIG or a discovered "
             "tooling/datalink.config.json.",
    )
