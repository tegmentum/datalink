# datalink/tooling — shared, DB-agnostic catalog tooling engine

Tier 1 of the `sqlink` + `ducklink` consolidation (see `../CONSOLIDATION.md`).

`ducklink` (DuckDB→wasm, `duckdb:extension` components) was mirrored from `sqlink`
(SQLite→wasm, `sqlite:extension` components). Their catalog tooling is a
near-verbatim duplicate. This directory is the **single engine**, parameterized by
a per-repo config, so each repo can later replace its mirrored scripts with a thin
config + a dependency on datalink.

## What's lifted (this pass)

| script | role | DB-specific bits → config |
| --- | --- | --- |
| `scaffold.py` | generate a new extension crate from `templates/` + the compat-registry | package suffix, name regex, templates dir, WIT world+copy, the `worlds`→lib.rs map, registration ABI (manifest/imperative), workspace-member registration, build-check argv/cwd/target |
| `smoke.py` | run `<ext>/smoke.sql` through the host CLI, diff vs `smoke.expected` (`~~`/`?`/`# ` wildcards), all-NULL warn, `--all -j N`, `--seed-expected`, `--list` | CLI argv template, host bin / cli component / required artifacts, prompt regex, SQL preamble (`.nullvalue`/`.mode csv`), null token, banner prefixes, panic markers, per-ext `--build`, network-grant env |
| `t-status.py` | scan `lessons-learned.md` for `(T-N new)`/`(T-N closed)` markers | doc path only (the format is shared) |
| `compat.py` | read/validate `compat-registry.json` (upstream-crate wasm32 status); `--list-broken`, `--check`, `--validate` | registry path only (the `_schema` is shared; per-crate data stays per-repo) |
| `registry.py` | read/validate the catalog index **CORE** fields; `--list`, `--validate`, `--show` | index path + entries key (DB-specific fields are opaque pass-through) |
| `identity.py` | the shared content-addressed identity: `witcanon_digest(cfg)` (contract/shape), `content_digest(artifact)` (byte), `imported_contract_{version,major}(artifact, package)` (the `@MAJOR` cross-check via `wasm-tools`) | `identity.{wit_source_dir, contract_package, contract_major, contract_version, artifacts_dir, wasm_tools_bin, exclude}` |
| `gen.py` | stamp `wit_contract` + `wit_contract_version` + `content_digest` into every entry whose artifact is present (idempotent; preserves 2-space indent + trailing newline); `--check` reports drift without writing | identity params + registry path |
| `verify.py` | enforce `wit_contract` == recomputed witcanon digest + the `@MAJOR` cross-check (**default**); `content_digest` == `sha256(artifact)` only under `--verify-content`/`--strict`; `--no-artifacts` for the toolchain-free subset | identity params + registry path |
| `dlconfig.py` | shared config loader (discovery, repo_root + path resolution) | — |
| `config.schema.json` | the per-repo config contract (JSON Schema) | — |
| `spec/lessons-learned.spec.md` | the shared lessons-learned **format**/`(T-N …)` convention | entries stay per-repo |

`t-status.py` and the compat `_schema` were byte-identical across both repos;
`scaffold.py`/`smoke.py` were ~90% identical with the DB bits now in config.

## The config contract

Each consuming repo ships one config matching `config.schema.json`, discovered as
`tooling/datalink.config.json` (or `$DATALINK_CONFIG`, or `--config PATH`). Two
filled, working examples are in `examples/` (real values pulled from each repo):

- `examples/ducklink.config.json` — `db_name: duckdb`, `registration_abi: imperative`,
  `-component` suffix, WIT `duckdb:extension`, single world, workspace registration,
  `ducklink --extensions-dir … -- :memory: --load-extension <name>` smoke, `.mode csv`,
  `--build` (cargo component) supported, `DUCKLINK_NETWORK_GRANT` env.
- `examples/sqlink.config.json` — `db_name: sqlite`, `registration_abi: manifest`,
  no suffix, no WIT copy, five `worlds` (minimal/collating/tabular/stateful/authorizing),
  inline `[workspace]`, `sqlink <cli.component> --db :memory:` smoke, `.nullvalue <NULL>`.

To consume in-repo: copy the example to `<repo>/tooling/datalink.config.json`, drop
the absolute `repo_root` (it auto-resolves to the config's grandparent dir), and the
scripts run from anywhere in the repo.

`type_map` is a **placeholder** capturing the deep DuckDB-rich-types vs
SQLite-storage-classes divergence (the Tier-2 `ValueModel` concern). The current
Tier-1 engine does not consume it; it marks where ValueModel-aware tooling will
branch.

## Verification (run, no source repos modified)

- `t-status.py --config examples/<repo>.config.json` produces output **identical** to
  each repo's original `t-status.py` (ducklink Open 3 / Closed 2; sqlink Open 3 /
  Closed 35).
- `scaffold.py --dry-run` and `smoke.py --dry-run` resolve the correct per-repo WIT
  world, templates, CLI argv, preamble, prompt regex, and null token for **both**
  configs.
- A real `scaffold.py` run into a temp ducklink-shaped repo produced the right crate
  (WIT copied, `[package.metadata.component]` written, root `[workspace].members`
  updated); into a temp sqlink-shaped repo produced the manifest-shape crate (no WIT,
  inline `[workspace]`, world-selected lib.rs).
- `compat.py --validate` / `registry.py --validate` pass against each repo's data.

## The identity engine (lifted — ducklink Phase 1)

The **content-addressed identity** machinery is now lifted (it was deferred until
ducklink's Phase 1 finalized the scheme). `identity.py` + `gen.py` + `verify.py`
are the shared, DB-agnostic, parameterized engine. TWO digest schemes, both
reimplemented byte-identically from the orchestration framework's
`compose-core::blobs`:

- **witcanon (contract/shape identity)** — `sha256(b"witcanon:1" || bytes)` where
  `bytes` = the canonical contract WIT files (`config.identity.wit_source_dir`,
  every top-level `*.wit` sorted by filename, concatenated). Hex. The
  AUTHORITATIVE, always-enforced identity; mirrors ducklink's
  `crates/ducklink-runtime/build.rs` const. Changes iff the WIT shape changes.
- **content (byte identity)** — `sha256(bytes)` of each component's own `.wasm`.
  Hex. Re-stamped per deploy; enforced only under `--verify-content`/`--strict`
  because wasm builds are byte-reproducible within a fixed toolchain but **not**
  across rustc / cargo-component versions.

`gen.py` stamps `wit_contract` + `wit_contract_version` + `content_digest` (the
last only when the artifact is present) idempotently, preserving the index's
2-space indent + trailing newline. `verify.py` enforces the witcanon digest + the
`@MAJOR` cross-check (the built artifact's imported `<package>@MAJOR` via
`wasm-tools component wit`) by default, and the content digest under the opt-in
flag — mirroring ducklink's `verify-catalog.py` semantics exactly (`exclude`
filters the template `sample_extension`, as verify-catalog does).

`registry.py` still validates **CORE fields only** (read-only, no digest check);
identity verification lives in `verify.py`. The DB-specific divergence — sqlink
records `checksum` (an OCI artifact checksum) where ducklink records
`content_digest` — is **not** reconciled here: this lift only proves the engine
computes the right values for both (see "DEFERRED" below).

### DEFERRED — the per-repo consume

- ducklink's `gen-catalog.py` / `verify-catalog.py` delegating to this engine
  (then deleting the mirrored copies), keeping their catalog-Markdown /
  source / workspace / orphan checks.
- sqlink adopting the engine + aligning `checksum` → the shared `content_digest`
  scheme + the sqlink `@0.1.0` → `@1.0.0` contract decision.
- the canonical shared `datalink-identity` Rust crate (per CONSOLIDATION.md
  Tier 1) if/when the host engine needs it (the Python tooling is the Tier-1
  surface).

Other sqlink-only generic patterns noted as low-priority follow-ups: `plan-add.py`
(plan-table row append — repo-doc-format-specific) and `next-fid.py` (max FID+1 — tied
to the manifest FID const convention). Generalize if/when needed.

## Follow-up sequencing

1. **Identity lift** — DONE. `identity.py` + `gen.py` + `verify.py` (witcanon
   contract digest + content_digest + the `@MAJOR` cross-check), parameterized by
   `config.identity`. ducklink parity proven (witcanon `90fdc46a…`, content
   digests match, default/`--verify-content`/perturbation behaviour mirrors
   verify-catalog); sqlink generality proven read-only.
2. **Per-repo consume-and-delete**: drop `tooling/datalink.config.json` into each repo,
   add the dep on `datalink/tooling`, delete the mirrored `scaffold.py` / `smoke.py` /
   `t-status.py` (keep each repo's `templates/`, `compat-registry.json` data,
   `lessons-learned.md`). This pass did NOT modify sqlink or ducklink.
