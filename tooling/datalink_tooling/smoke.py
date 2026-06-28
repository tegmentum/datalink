#!/usr/bin/env python3
"""Run an extension's smoke.sql through the host CLI + optional assertions.

DB-AGNOSTIC ENGINE. Every DB-specific bit is resolved from the per-repo
config (config.schema.json -> smoke.*):
  - CLI invocation         smoke.argv (tokens {HOST_BIN},{CLI_COMPONENT},
                           {EXTENSIONS_DIR},{NAME},{DB}); SQL piped on stdin
  - prompt stripping       smoke.prompt_pattern
  - NULL / mode preamble   smoke.sql_preamble, smoke.null_token
  - load banners           smoke.banner_prefixes
  - failure heuristics     smoke.panic_markers
  - artifact requirements  smoke.{host_bin,cli_component,required_artifacts}
  - per-extension build    smoke.supports_build + build_argv + build_output
  - extra env / net grant  smoke.env

smoke.expected wildcards (shared): exact | `~~` skip | `?` any-non-empty |
leading `# ` comment.

Usage:
    smoke.py [--config PATH] <name>
    smoke.py [--config PATH] --all [-j N]
    smoke.py [--config PATH] --build <name>     (if smoke.supports_build)
    smoke.py [--config PATH] --list
    smoke.py [--config PATH] --seed-expected NAME
    smoke.py [--config PATH] --dry-run <name>   (print resolved argv, run nothing)
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

from . import dlconfig  # noqa: E402


def _suffix(cfg) -> str:
    return cfg.get("smoke", "package_suffix", default="")


def bare(cfg, name: str) -> str:
    suf = _suffix(cfg)
    return name[: -len(suf)] if suf and name.endswith(suf) else name


def ext_dir(cfg, name: str) -> Path:
    sc_dir = cfg.get("scaffold", "extensions_dir", default="extensions")
    return cfg.repo_root / sc_dir / f"{bare(cfg, name)}{_suffix(cfg)}"


def _cargo_toml(cfg, name: str) -> dict:
    """Parse the component's Cargo.toml; {} if absent or unparseable."""
    cargo = ext_dir(cfg, name) / "Cargo.toml"
    if not cargo.exists():
        return {}
    try:
        return tomllib.loads(cargo.read_text())
    except (OSError, tomllib.TOMLDecodeError):
        return {}


def _lib_name(cfg, name: str) -> str | None:
    """Custom [lib] name from the component's Cargo.toml, if it sets one."""
    return (_cargo_toml(cfg, name).get("lib") or {}).get("name")


def _pkg_name(cfg, name: str) -> str | None:
    """Actual [package] name from the component's Cargo.toml, if present.

    The cargo package id is NOT always derivable from the directory name:
    a few components' dirs use "_" while the package uses "-" (or vice
    versa), so derive the build -p target from the manifest when we can.
    """
    return (_cargo_toml(cfg, name).get("package") or {}).get("name")


def find_smoke_files(cfg) -> list[Path]:
    sc_dir = cfg.get("scaffold", "extensions_dir", default="extensions")
    return sorted((cfg.repo_root / sc_dir).glob("*/smoke.sql"))


def prompt_re(cfg) -> re.Pattern:
    pat = cfg.get("smoke", "prompt_pattern", default=r"^$a")
    return re.compile(pat)


def parse_results(cfg, raw: str) -> list[str]:
    pr = prompt_re(cfg)
    banners = cfg.get("smoke", "banner_prefixes", default=[])
    out: list[str] = []
    for line in raw.splitlines():
        stripped = pr.sub("", line).rstrip()
        if not stripped:
            continue
        if any(stripped.startswith(b) for b in banners):
            continue
        out.append(stripped)
    return out


def parse_expected(path: Path) -> list[str]:
    out: list[str] = []
    for line in path.read_text().splitlines():
        s = line.rstrip()
        if not s.strip():
            continue
        ls = s.lstrip()
        if ls == "#" or ls.startswith("# "):
            continue
        out.append(s)
    return out


def count_smoke_selects(path: Path) -> int:
    text = path.read_text()
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.DOTALL)
    text = re.sub(r"--[^\n]*", " ", text)
    text = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("."))
    count = 0
    for stmt in text.split(";"):
        if stmt.strip().lower().startswith("select"):
            count += 1
    return count


def staleness(cfg, name: str) -> str | None:
    smoke = ext_dir(cfg, name) / "smoke.sql"
    expected = ext_dir(cfg, name) / "smoke.expected"
    if not smoke.exists() or not expected.exists():
        return None
    n_select = count_smoke_selects(smoke)
    n_expected = len(parse_expected(expected))
    if n_select > 0 and n_expected == 0:
        return f"smoke.sql has {n_select} SELECT(s) but smoke.expected is empty"
    return None


def compare(actual: list[str], expected: list[str]) -> list[str]:
    diffs: list[str] = []
    if len(actual) != len(expected):
        diffs.append(f"length mismatch: actual={len(actual)} lines, expected={len(expected)}")
    for i, (got, want) in enumerate(zip(actual, expected)):
        if want == "~~":
            continue
        if want == "?":
            if not got:
                diffs.append(f"line {i+1}: expected any non-empty value, got empty")
            continue
        if got != want:
            diffs.append(f"line {i+1}: expected {want!r}, got {got!r}")
    return diffs


def _sql_for(cfg, name: str) -> str:
    smoke = ext_dir(cfg, name) / "smoke.sql"
    comment = cfg.get("smoke", "comment_prefix", default="--")
    sql = "\n".join(
        line for line in smoke.read_text().splitlines()
        if not line.lstrip().startswith(comment)
    )
    preamble = cfg.get("smoke", "sql_preamble", default="")
    return preamble + sql


def _argv(cfg, name: str) -> list[str]:
    host_bin = cfg.path(cfg.get("smoke", "host_bin"))
    cli_comp = cfg.path(cfg.get("smoke", "cli_component"))
    sc_dir = cfg.get("smoke", "extensions_dir")
    ext_artifacts = cfg.path(sc_dir) if sc_dir else None
    db = cfg.get("smoke", "db", default=":memory:")
    sub = {
        "{HOST_BIN}": str(host_bin) if host_bin else "",
        "{CLI_COMPONENT}": str(cli_comp) if cli_comp else "",
        "{EXTENSIONS_DIR}": str(ext_artifacts) if ext_artifacts else "",
        "{NAME}": bare(cfg, name),
        "{DB}": db,
    }
    out = []
    for tok in cfg.get("smoke", "argv", default=[]):
        for k, v in sub.items():
            tok = tok.replace(k, v)
        out.append(tok)
    return out


def _env(cfg, name: str) -> dict:
    env = {**os.environ}
    for k, v in (cfg.get("smoke", "env", default={}) or {}).items():
        env[k] = v.replace("{NAME}", bare(cfg, name))
    return env


def _run_cli(cfg, name: str, sql: str, timeout: int) -> subprocess.CompletedProcess:
    return subprocess.run(
        _argv(cfg, name), input=sql, capture_output=True, text=True,
        timeout=timeout, cwd=cfg.repo_root, env=_env(cfg, name),
    )


def build_component(cfg, name: str) -> tuple[bool, str]:
    target = cfg.get("smoke", "build_target", default="wasm32-wasip2")
    # Prefer the manifest's real [package] name; the dir->pkg convention
    # (bare+suffix) breaks for components whose dir/package separators differ.
    pkg = _pkg_name(cfg, name) or f"{bare(cfg, name)}{_suffix(cfg)}"
    underscore = bare(cfg, name).replace("-", "_")
    argv = [t.replace("{PKG}", pkg).replace("{TARGET}", target)
            for t in cfg.get("smoke", "build_argv", default=[])]
    result = subprocess.run(argv, cwd=cfg.repo_root, capture_output=True, text=True)
    if result.returncode != 0:
        return (False, "\n".join(result.stderr.split("\n")[-30:]))
    out_tmpl = cfg.get("smoke", "build_output")
    target_dir = cfg.repo_root / "target" / target / "release"
    # Resolve the REAL cargo-component output. Components that set a custom
    # [lib] name emit "{lib_name}.wasm"; otherwise cargo derives the artifact
    # from the package name, matching the configured "{UNDERSCORE}_component"
    # template. Probe the lib-name candidate first, then the template, then
    # fall back to the newest *.wasm cargo-component just produced.
    candidates = []
    lib_name = _lib_name(cfg, name)
    if lib_name:
        candidates.append(target_dir / f"{lib_name}.wasm")
    candidates.append(cfg.path(out_tmpl.replace("{UNDERSCORE}", underscore)
                                       .replace("{TARGET_DIR}", str(target_dir))))
    built = next((c for c in candidates if c.exists()), None)
    if built is None:
        wasms = sorted(target_dir.glob("*.wasm"), key=lambda p: p.stat().st_mtime)
        built = wasms[-1] if wasms else candidates[-1]
    if not built.exists():
        return (False, f"expected build output {cfg.rel(built)} not found")
    ext_artifacts = cfg.path(cfg.get("smoke", "extensions_dir"))
    ext_artifacts.mkdir(parents=True, exist_ok=True)
    dest = ext_artifacts / f"{bare(cfg, name)}.wasm"
    shutil.copy2(built, dest)
    return (True, f"built + copied {cfg.rel(dest)}")


def _missing_artifacts(cfg, name: str) -> str | None:
    host_bin = cfg.path(cfg.get("smoke", "host_bin"))
    if host_bin and not host_bin.exists():
        return f"host runner not built: {cfg.rel(host_bin)} missing"
    cli_comp = cfg.path(cfg.get("smoke", "cli_component"))
    if cli_comp and not cli_comp.exists():
        return f"cli component not built: {cfg.rel(cli_comp)} missing"
    for req in cfg.get("smoke", "required_artifacts", default=[]):
        p = cfg.path(req)
        if not p.exists():
            return f"required artifact missing: {cfg.rel(p)}"
    # per-extension wasm artifact, if the host resolves one from extensions_dir
    sc_dir = cfg.get("smoke", "extensions_dir")
    if sc_dir and cfg.get("smoke", "supports_build", default=False):
        art = cfg.path(sc_dir) / f"{bare(cfg, name)}.wasm"
        if not art.exists():
            return (f"extension artifact missing: {cfg.rel(art)}; "
                    f"run: smoke.py --build {bare(cfg, name)}")
    return None


def smoke_one(cfg, name: str, timeout: int) -> tuple[bool, str]:
    smoke = ext_dir(cfg, name) / "smoke.sql"
    if not smoke.exists():
        return (False, f"no smoke.sql at {cfg.rel(smoke)}")
    if (miss := _missing_artifacts(cfg, name)):
        return (False, miss)

    try:
        result = _run_cli(cfg, name, _sql_for(cfg, name), timeout)
    except subprocess.TimeoutExpired:
        return (False, f"timeout after {timeout}s")

    out = result.stdout + result.stderr
    markers = cfg.get("smoke", "panic_markers", default=[])
    if any(m in out for m in markers):
        return (False, out)

    if (stale := staleness(cfg, name)):
        out = f"WARN: {stale}\n{out}"

    expected_path = ext_dir(cfg, name) / "smoke.expected"
    null_token = cfg.get("smoke", "null_token", default="NULL")
    min_rows = cfg.get("smoke", "all_null_min_rows", default=5)
    if not expected_path.exists():
        actual = parse_results(cfg, result.stdout)
        if len(actual) >= min_rows and all(row == null_token for row in actual):
            out = ("WARN: every parsed line is " + null_token + "  is your scalar "
                   "wired up? (no smoke.expected yet; seed one with "
                   "--seed-expected)\n" + out)

    if expected_path.exists():
        actual = parse_results(cfg, result.stdout)
        expected = parse_expected(expected_path)
        diffs = compare(actual, expected)
        if diffs:
            msg = ["output mismatch vs smoke.expected:"]
            msg.extend(f"  {d}" for d in diffs)
            msg.append("--- parsed actual ---")
            msg.extend(f"  {i+1}: {row}" for i, row in enumerate(actual))
            return (False, "\n".join(msg))

    return (True, out)


def seed_expected(cfg, name: str, timeout: int) -> None:
    expected = ext_dir(cfg, name) / "smoke.expected"
    if expected.exists():
        print(f"smoke.expected already exists at {cfg.rel(expected)}", file=sys.stderr)
        print("delete it first if you intend to reseed.", file=sys.stderr)
        sys.exit(1)
    smoke = ext_dir(cfg, name) / "smoke.sql"
    if not smoke.exists():
        print(f"no smoke.sql at {cfg.rel(smoke)}", file=sys.stderr)
        sys.exit(1)
    r = _run_cli(cfg, name, _sql_for(cfg, name), timeout)
    rows = parse_results(cfg, r.stdout)
    header = (
        "# AUTO-SEEDED by smoke.py --seed-expected. Review and trim:\n"
        "#   - replace nondeterministic lines (timestamps, rng) with ~~\n"
        "#   - replace order-sensitive lines with ? if any-non-empty is OK\n"
        "#   - delete this banner once you've reviewed each line\n"
    )
    expected.write_text(header + "\n".join(rows) + "\n")
    print(f"wrote {len(rows)} lines to {cfg.rel(expected)}")


def main(config: str | None = None, argv=None) -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    dlconfig.add_config_arg(p, default=config)
    p.add_argument("name", nargs="?")
    p.add_argument("--all", action="store_true")
    p.add_argument("--build", action="store_true",
                   help="build component + copy artifact first (if smoke.supports_build)")
    p.add_argument("--list", action="store_true")
    p.add_argument("--dry-run", action="store_true",
                   help="print the resolved CLI argv + env for <name> and exit")
    p.add_argument("--timeout", type=int, default=60)
    p.add_argument("-j", "--jobs", type=int, default=1)
    p.add_argument("--seed-expected", metavar="NAME")
    args = p.parse_args(argv)

    cfg = dlconfig.load(args.config)

    if not (args.name or args.all or args.list or args.seed_expected):
        p.error("specify <name>, --all, --list, or --seed-expected")

    if args.dry_run:
        nm = args.name or args.seed_expected
        print(f"db_name:    {cfg.db_name}")
        print(f"argv:       {' '.join(_argv(cfg, nm))}")
        env_extra = {k: v.replace('{NAME}', bare(cfg, nm))
                     for k, v in (cfg.get('smoke', 'env', default={}) or {}).items()}
        if env_extra:
            print(f"env:        {env_extra}")
        print(f"preamble:   {cfg.get('smoke','sql_preamble',default='')!r}")
        print(f"prompt re:  {cfg.get('smoke','prompt_pattern')!r}")
        print(f"null token: {cfg.get('smoke','null_token',default='NULL')!r}")
        print(f"smoke dir:  {cfg.rel(ext_dir(cfg, nm))}")
        return

    if args.seed_expected:
        if args.build and cfg.get("smoke", "supports_build", default=False):
            ok, msg = build_component(cfg, args.seed_expected)
            print(("OK  " if ok else "FAIL  ") + msg)
            if not ok:
                sys.exit(1)
        seed_expected(cfg, args.seed_expected, args.timeout)
        return

    if args.list:
        for f in find_smoke_files(cfg):
            has_expected = (f.parent / "smoke.expected").exists()
            stale = staleness(cfg, f.parent.name) if has_expected else None
            marker = ""
            if has_expected:
                marker = " [asserted, STALE]" if stale else " [asserted]"
            line = f"{f.parent.name}{marker}"
            if stale:
                line += f"  {stale}"
            print(line)
        return

    if args.all:
        targets = [bare(cfg, f.parent.name) for f in find_smoke_files(cfg)]
    else:
        targets = [bare(cfg, args.name)]

    if args.build and cfg.get("smoke", "supports_build", default=False):
        for name in targets:
            ok, msg = build_component(cfg, name)
            print(("OK  " if ok else "FAIL  ") + f"build {name}: {msg}")
            if not ok:
                sys.exit(1)

    fails: list[str] = []
    if args.jobs == 1 or len(targets) == 1:
        for name in targets:
            ok, output = smoke_one(cfg, name, args.timeout)
            print(f"{'PASS' if ok else 'FAIL'}  {name}")
            if not ok:
                fails.append(name)
                for line in output.split("\n")[:30]:
                    print(f"    {line}")
    else:
        import concurrent.futures
        workers = args.jobs if args.jobs > 0 else (os.cpu_count() or 4)
        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as ex:
            futures = {ex.submit(smoke_one, cfg, name, args.timeout): name
                       for name in targets}
            for fut in concurrent.futures.as_completed(futures):
                name = futures[fut]
                ok, output = fut.result()
                print(f"{'PASS' if ok else 'FAIL'}  {name}")
                if not ok:
                    fails.append(name)
                    for line in output.split("\n")[:30]:
                        print(f"    {line}")

    if fails:
        print(f"\n{len(fails)} failed: {', '.join(fails)}")
        sys.exit(1)
    print(f"\nall {len(targets)} passed")


if __name__ == "__main__":
    main()
