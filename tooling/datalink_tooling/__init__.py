"""datalink_tooling — the shared, DB-agnostic catalog tooling engine.

Tier 1 of the sqlink + ducklink consolidation. A consuming repo ships one
per-repo config matching ``config.schema.json`` (discovered as
``tooling/datalink.config.json``) and depends on this package; its thin
delegator scripts import the modules below and call ``module.main(config=...)``.

Public modules:

  dlconfig   shared config loader (discovery, repo_root + path resolution)
  compat     read/validate compat-registry.json (upstream-crate wasm32 status)
  registry   read/validate the catalog index CORE fields
  identity   content-addressed identity (witcanon contract digest + content_digest)
  gen        stamp the identity into every registry entry (+ extra_outputs hook)
  verify     enforce the identity (+ repo-specific extra_checks hook)
  scaffold   generate a new extension crate from templates/ + the compat-registry
  smoke      run an extension's smoke.sql through the host CLI
  tstatus    scan lessons-learned.md for (T-N new)/(T-N closed) markers

Each module exposes ``main(config=None, argv=None, **kw)``; ``verify`` and
``gen`` additionally accept an extensibility hook (``extra_checks`` /
``extra_outputs``) so a repo can inject its repo-specific checks / generation
while delegating the shared engine.
"""

from __future__ import annotations

from . import (  # noqa: F401
    compat,
    dlconfig,
    gen,
    identity,
    registry,
    scaffold,
    smoke,
    tstatus,
    verify,
)

__all__ = [
    "compat",
    "dlconfig",
    "gen",
    "identity",
    "registry",
    "scaffold",
    "smoke",
    "tstatus",
    "verify",
]
