#!/usr/bin/env python3
"""Scaffold a new extension component from templates/ + the compat-registry.

DB-AGNOSTIC ENGINE. Every DB-specific bit is resolved from the per-repo
config (config.schema.json -> scaffold.*):
  - package naming         scaffold.package_suffix (sqlite '' / ducklink '-component')
  - name rules             scaffold.name_pattern
  - templates dir          scaffold.templates_dir
  - WIT world + copy        wit.{package,world,source_dir}
  - lib.rs template(s)     scaffold.worlds  (single-world repos use one entry)
  - registration ABI       registration_abi (manifest|imperative) — drives which
                           template you point at; the engine just renders it
  - workspace registration scaffold.register_workspace_member
  - build-check            scaffold.build_check.{argv,cwd,target,shared_target_dir}

Usage:
    scaffold.py [--config PATH] <name> [--crate c1,c2] [--description "..."]
                [--world W] [--dry-run]
    scaffold.py [--config PATH] --list-broken
    scaffold.py [--config PATH] --list-worlds
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dlconfig  # noqa: E402
import compat  # noqa: E402


def _default_world(worlds: dict) -> str:
    for cand in ("minimal", "default"):
        if cand in worlds:
            return cand
    return next(iter(worlds)) if worlds else "default"


def render(templates_dir: Path, template_name: str, **vars_: str) -> str:
    raw = (templates_dir / template_name).read_text()
    return raw.format(**vars_)


def crate_block(crate_specs: list[str], registry: dict) -> tuple[str, list[str]]:
    """Render the [dependencies] block lines. Returns (block, warnings)."""
    lines: list[str] = []
    warnings: list[str] = []
    for spec in crate_specs:
        if "@" in spec:
            name, ver = spec.split("@", 1)
        else:
            name, ver = spec, None
        status, notes = compat.check_crate(name, registry)
        if status == "broken":
            warnings.append(f"  x {name}: BROKEN  {notes}")
            lines.append(f"# BROKEN per compat-registry: {notes}")
            lines.append(f'# {name} = "{ver or "*"}"')
            continue
        if status == "hand-roll-preferred":
            warnings.append(f"  ~ {name}: hand-roll preferred  {notes}")
            lines.append(f"# hand-roll preferred per compat-registry: {notes}")
            continue
        if status == "needs-bootstrap":
            warnings.append(f"  ! {name}: needs RUSTC_BOOTSTRAP=1  {notes}")
        elif status == "needs-feature-tweak":
            warnings.append(f"  ! {name}: needs feature tweak  {notes}")
        elif status == "unverified":
            warnings.append(f"  ? {name}: unverified  evaluate before relying on it")
        if notes:
            lines.append(f"# {notes}")
        version_str = ver or compat.suggest_version(name, registry)
        lines.append(f'{name} = "{version_str}"')
    return ("\n".join(lines) if lines else "# add your upstream crate deps here", warnings)


def register_workspace_member(workspace_toml: Path, member: str) -> bool:
    """Insert `member` into [workspace].members. True if added, False if present."""
    text = workspace_toml.read_text()
    if f'"{member}"' in text:
        return False
    lines = text.splitlines()
    in_members = False
    for idx, line in enumerate(lines):
        if re.match(r"\s*members\s*=\s*\[", line):
            in_members = True
            continue
        if in_members and line.strip().startswith("]"):
            lines.insert(idx, f'    "{member}",')
            workspace_toml.write_text("\n".join(lines) + "\n")
            return True
    raise SystemExit("error: could not locate [workspace].members in Cargo.toml")


def resolve(cfg: dlconfig.Config, name: str, world: str | None) -> dict:
    """Resolve all the DB-specific scaffold parameters for a name (no writes)."""
    sc = cfg.get("scaffold", default={})
    suffix = sc.get("package_suffix", "")
    package = f"{name}{suffix}"
    ext_dir = cfg.path(sc["extensions_dir"]) / package
    templates_dir = cfg.path(sc["templates_dir"])
    worlds = sc.get("worlds") or {"default": ["lib.rs.tmpl", "scalar functions"]}
    w = world or _default_world(worlds)
    if w not in worlds:
        raise SystemExit(f"error: unknown world {w!r}; valid: {', '.join(sorted(worlds))}")
    lib_tmpl, world_hint = worlds[w]
    pattern = sc.get("name_pattern", "^[a-z][a-z0-9-]*$")
    bc = sc.get("build_check", {})
    return {
        "package": package,
        "ext_dir": ext_dir,
        "templates_dir": templates_dir,
        "world": w,
        "lib_template": lib_tmpl,
        "world_hint": world_hint,
        "name_pattern": pattern,
        "lib_template_vars": sc.get("lib_template_vars",
                                    ["NAME", "NAME_UNDERSCORE", "DESCRIPTION_SHORT"]),
        "register_workspace": sc.get("register_workspace_member", False),
        "wit_source": cfg.path(cfg.get("wit", "source_dir")),
        "wit_world": cfg.get("wit", "world"),
        "build_check": bc,
    }


def scaffold(cfg: dlconfig.Config, name: str, crates: list[str],
             description: str, world: str | None, dry_run: bool) -> None:
    r = resolve(cfg, name, world)

    if not re.match(r["name_pattern"], name):
        sys.exit(f"error: extension name must match {r['name_pattern']} (got {name!r})")
    suffix = cfg.get("scaffold", "package_suffix", default="")
    if suffix and name.endswith(suffix):
        sys.exit(f"error: pass the bare name; {suffix!r} is appended automatically")

    target: Path = r["ext_dir"]
    if dry_run:
        print(f"db_name:           {cfg.db_name}")
        print(f"registration_abi:  {cfg.registration_abi}")
        print(f"package:           {r['package']}")
        print(f"target dir:        {cfg.rel(target)}")
        print(f"templates dir:     {cfg.rel(r['templates_dir'])}")
        print(f"world:             {r['world']}  ({r['world_hint']})")
        print(f"lib.rs template:   {r['lib_template']}")
        if r["wit_world"]:
            print(f"WIT world:         {r['wit_world']}")
            print(f"WIT source:        {cfg.rel(r['wit_source']) if r['wit_source'] else '(none)'}")
        print(f"register workspace: {r['register_workspace']}")
        bc = r["build_check"]
        if bc:
            argv = _build_argv(bc, r["package"], name)
            print(f"build-check:       {' '.join(argv)}  (cwd={bc.get('cwd','crate')})")
        return

    if target.exists():
        sys.exit(f"error: {cfg.rel(target)} already exists")

    registry = compat.load_registry(cfg)
    deps_block, warnings = crate_block(crates, registry)

    target.mkdir(parents=True)
    (target / "src").mkdir()

    if r["wit_source"]:
        if not r["wit_source"].is_dir():
            sys.exit(f"error: WIT source {cfg.rel(r['wit_source'])} not found")
        shutil.copytree(r["wit_source"], target / "wit")

    name_underscore = name.replace("-", "_")
    desc_short = description.splitlines()[0][:200] if description else f"{name} scalars"

    (target / "Cargo.toml").write_text(
        render(r["templates_dir"], "Cargo.toml.tmpl",
               NAME=name, DESCRIPTION=description or f"{name} extension", DEPS=deps_block)
    )
    all_vars = {"NAME": name_underscore if cfg.db_name == "sqlite" else name,
                "NAME_UNDERSCORE": name_underscore,
                "DESCRIPTION_SHORT": desc_short}
    # sqlite minimal template uses {NAME} as the underscore form; ducklink uses
    # both NAME (bare) and NAME_UNDERSCORE. lib_template_vars selects the subset.
    lib_vars = {k: all_vars[k] for k in r["lib_template_vars"]}
    (target / "src" / "lib.rs").write_text(
        render(r["templates_dir"], r["lib_template"], **lib_vars)
    )
    (target / "smoke.sql").write_text(
        render(r["templates_dir"], "smoke.sql.tmpl",
               NAME=name, NAME_UNDERSCORE=name_underscore)
    )

    print(f"created {cfg.rel(target)}/Cargo.toml")
    print(f"created {cfg.rel(target)}/src/lib.rs")
    print(f"created {cfg.rel(target)}/smoke.sql")
    if r["wit_source"]:
        print(f"copied  {cfg.rel(target / 'wit')}/  ({r['wit_world']})")

    if r["register_workspace"]:
        ws = cfg.repo_root / "Cargo.toml"
        ext_rel = cfg.rel(target)
        if register_workspace_member(ws, ext_rel):
            print(f"registered {ext_rel} as a workspace member")
        else:
            print(f"{ext_rel} already a workspace member")

    if warnings:
        print("\ncompat notes:")
        for w in warnings:
            print(w)

    bc = r["build_check"]
    if bc and shutil.which("cargo"):
        argv = _build_argv(bc, r["package"], name)
        cwd = target if bc.get("cwd", "crate") == "crate" else cfg.repo_root
        print(f"\nrunning: {' '.join(argv)}  (cwd={cfg.rel(cwd)})")
        env = {**os.environ}
        std = bc.get("shared_target_dir")
        if std:
            env["CARGO_TARGET_DIR"] = str(cfg.path(std))
        result = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, env=env)
        if result.returncode != 0:
            print("FAILED  build-check exited non-zero")
            print("\n".join(result.stderr.split("\n")[-30:]))
            sys.exit(1)
        print("OK  skeleton compiles clean")

    print(f"\nnext (world={r['world']}, focus: {r['world_hint']}):")
    print(f"  1. edit {cfg.rel(target)}/src/lib.rs")
    print(f"  2. edit {cfg.rel(target)}/smoke.sql")
    print(f"  3. make ext NAME={r['package']}")


def _build_argv(bc: dict, package: str, name: str) -> list[str]:
    target = bc.get("target", "wasm32-wasip2")
    out = []
    for tok in bc.get("argv", []):
        out.append(tok.replace("{PKG}", package)
                      .replace("{NAME}", name)
                      .replace("{TARGET}", target))
    return out


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    dlconfig.add_config_arg(p)
    p.add_argument("name", nargs="?", help="extension name (bare)")
    p.add_argument("--crate", default="", help="comma-separated upstream crates; '@x.y' to pin")
    p.add_argument("--description", default="")
    p.add_argument("--world", default=None, help="WIT world (see --list-worlds)")
    p.add_argument("--dry-run", action="store_true",
                   help="resolve + print the DB-specific scaffold parameters; write nothing")
    p.add_argument("--list-broken", action="store_true")
    p.add_argument("--list-worlds", action="store_true")
    args = p.parse_args()

    cfg = dlconfig.load(args.config)

    if args.list_broken:
        registry = compat.load_registry(cfg)
        rows = compat.list_broken(registry)
        if not rows:
            print("no flagged crates  registry is clean")
            return
        width = max(len(c) for _, c, _ in rows)
        for status, crate, note in rows:
            print(f"  {status:20s} {crate:<{width}}  {note}")
        return

    if args.list_worlds:
        worlds = cfg.get("scaffold", "worlds") or {"default": ["lib.rs.tmpl", "scalar functions"]}
        dflt = _default_world(worlds)
        for w, (tmpl, hint) in worlds.items():
            mark = " (default)" if w == dflt else ""
            print(f"  {w:<14}{mark:<10} {tmpl:<24} {hint}")
        return

    if not args.name:
        p.error("the following arguments are required: name")

    crates = [c.strip() for c in args.crate.split(",") if c.strip()]
    scaffold(cfg, args.name, crates, args.description, args.world, args.dry_run)


if __name__ == "__main__":
    main()
