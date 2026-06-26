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

## DEFERRED — the identity module (next step)

The **content-addressed identity** machinery is intentionally NOT lifted here, because
a separate agent is doing ducklink's Phase 1 identity work right now (it owns
`tooling/gen-catalog.py`, `tooling/verify-catalog.py`, and `registry/index.json`).
Deferred to a follow-up once Phase 1 lands:

- `witcanon:1` contract digest + the `content_digest` (sha256, `compose-core` scheme)
- `gen-catalog.py` (index generation) and `verify-catalog.py` (digest verification)
- pointing sqlink's `checksum` at the same shared scheme
- the canonical shared `datalink-identity` (per CONSOLIDATION.md Tier 1 / quick wins)

`registry.py` here deliberately validates **CORE fields only** and reads the index
read-only; it does NOT verify any digest. Identity-specific fields
(`content_digest`, `wit_contract`, `checksum`, …) are passed through untouched.

Other sqlink-only generic patterns noted as low-priority follow-ups: `plan-add.py`
(plan-table row append — repo-doc-format-specific) and `next-fid.py` (max FID+1 — tied
to the manifest FID const convention). Generalize if/when needed.

## Follow-up sequencing

1. **Identity lift** (after ducklink Phase 1): witcanon + content_digest + gen/verify
   as `datalink-identity` + the registry digest verification in `registry.py`.
2. **Per-repo consume-and-delete**: drop `tooling/datalink.config.json` into each repo,
   add the dep on `datalink/tooling`, delete the mirrored `scaffold.py` / `smoke.py` /
   `t-status.py` (keep each repo's `templates/`, `compat-registry.json` data,
   `lessons-learned.md`). This pass did NOT modify sqlink or ducklink.
