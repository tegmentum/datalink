#!/usr/bin/env python3
"""Verify the content-addressed identity of every registry entry.

The verify half of the shared identity engine (lifted from ducklink's
tooling/verify-catalog.py identity checks, parameterized via the per-repo
config). Mirrors verify-catalog's semantics exactly:

  DEFAULT (always enforced):
    * wit_contract == witcanon_digest(config)   -- the AUTHORITATIVE contract
      identity; any WIT shape change that wasn't re-stamped (gen.py) fails here.
    * wit_contract_version major == config contract major (cross-check).
    * when the artifact is present, the BUILT component's imported
      <package>@MAJOR == the config contract major (the @MAJOR cross-check,
      via `wasm-tools component wit`).

  OPT-IN (--verify-content / --strict):
    * content_digest == sha256(deployed artifact). NOT default: wasm builds are
      byte-reproducible within a fixed toolchain but NOT across rustc /
      cargo-component versions, so a rebuild on a different toolchain would flip
      the bytes and fail. Release/CI runs with the flag against the canonical
      deployed artifacts.

  --no-artifacts: skip every check that needs a built .wasm (artifact presence,
    the @MAJOR cross-check, content). Lets the registry<->contract consistency be
    validated WITHOUT the wasm toolchain.

This checks IDENTITY only (the lifted Phase-1 scheme). It does NOT re-do
registry.py's CORE-field validation or ducklink's source/workspace/orphan checks
(those stay per-repo / in registry.py).

  verify.py [--config PATH]                     # default identity checks
  verify.py [--config PATH] --verify-content    # + content_digest (alias --strict)
  verify.py [--config PATH] --no-artifacts      # contract-only, no wasm toolchain

stdlib only; matches the existing tooling style.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dlconfig  # noqa: E402
import identity  # noqa: E402
import registry  # noqa: E402


def verify(cfg, verify_content: bool = False, no_artifacts: bool = False) -> int:
    entries = registry.load_entries(cfg)
    # Entries excluded from identity checks (parity with verify-catalog.py, which
    # filters `sample_extension` out of its `exts` before the digest loop). The
    # sample/template extension imports the legacy unversioned contract by design.
    exclude = set(cfg.get("identity", "exclude", default=[]))
    if exclude:
        entries = [e for e in entries if e.get("name") not in exclude]
    digest = identity.witcanon_digest(cfg)
    version = identity.contract_version(cfg)
    major = identity.contract_major(cfg)
    package = identity.contract_package(cfg)
    host_bin = identity.wasm_tools_bin(cfg)

    issues: list[str] = []
    artifacts_present = 0
    content_checked = 0

    for e in entries:
        n = e.get("name", "<unnamed>")

        # AUTHORITATIVE: wit_contract == recomputed witcanon digest.
        wc = e.get("wit_contract")
        if not wc:
            issues.append(f"{n}: missing `wit_contract` (expected digest {digest[:12]}…)")
        elif wc != digest:
            issues.append(
                f"{n}: wit_contract {wc[:12]}… != canonical WIT digest {digest[:12]}… "
                f"(re-propagate the WIT + rerun gen.py)"
            )

        # Cross-check: the human version's major names the target major.
        wcv = e.get("wit_contract_version")
        if not wcv:
            issues.append(f"{n}: missing `wit_contract_version` (expected {version})")
        elif str(wcv).split(".")[0] != major:
            issues.append(
                f"{n}: wit_contract_version {wcv} != catalog contract major {major}.x"
            )

        if no_artifacts:
            continue

        art = identity.artifact_path(cfg, n)
        if not art.exists():
            # artifacts are built/deployed locally; a missing one is not an
            # identity failure here (verify-catalog's separate artifact-presence
            # check stays per-repo). Skip artifact-dependent checks for this entry.
            continue
        artifacts_present += 1

        # OPT-IN: content_digest == sha256(deployed artifact).
        if verify_content:
            cd = e.get("content_digest")
            if not cd:
                issues.append(
                    f"{n}: missing `content_digest` (run gen.py with the artifact "
                    f"present to stamp it)"
                )
            else:
                actual_cd = identity.content_digest(art)
                if actual_cd != cd:
                    issues.append(
                        f"{n}: content_digest {cd[:12]}… != deployed artifact "
                        f"sha256 {actual_cd[:12]}… (rerun gen.py to re-stamp)"
                    )
                else:
                    content_checked += 1

        # @MAJOR cross-check: the built component's imported <package>@MAJOR.
        actual = identity.imported_contract_version(art, package, host_bin)
        if actual is None:
            pass  # no <package> import or wasm-tools missing; nothing to assert
        elif actual == "unversioned":
            issues.append(
                f"{n}: artifact imports an UNVERSIONED {package} contract (legacy) "
                f"but catalog targets {version}; rebuild it"
            )
        elif actual.split(".")[0] != major:
            issues.append(
                f"{n}: artifact imports {package}@{actual} but catalog targets "
                f"contract major {major}.x"
            )

    print(f"identity: {len(entries)} entries · contract {package}@{major}.x "
          f"({digest[:12]}…)")
    if not no_artifacts:
        print(f"  artifacts present: {artifacts_present}"
              + (f" · content verified: {content_checked}" if verify_content else
                 " · content check OFF (pass --verify-content to enforce)"))

    if issues:
        print(f"\nFAILED — {len(issues)} issue(s):")
        for i in issues:
            print(f"  - {i}")
        return 1
    print("\nOK — registry identity consistent "
          "(wit_contract + @MAJOR"
          + (" + content_digest" if verify_content else "") + ").")
    return 0


def main() -> None:
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    dlconfig.add_config_arg(p)
    p.add_argument("--verify-content", "--strict", action="store_true",
                   dest="verify_content",
                   help="also enforce content_digest == sha256(deployed artifact)")
    p.add_argument("--no-artifacts", action="store_true",
                   help="contract-only checks; skip everything needing a built .wasm")
    args = p.parse_args()
    cfg = dlconfig.load(args.config)
    sys.exit(verify(cfg, verify_content=args.verify_content,
                     no_artifacts=args.no_artifacts))


if __name__ == "__main__":
    main()
