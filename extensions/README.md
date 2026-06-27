# Extension pull-up — shared cores + generated per-DB shims

Write an extension ONCE — its DB-agnostic logic plus a capability
declaration — and generate BOTH the ducklink (`duckdb:extension`) and
sqlink (`sqlite:extension`) shims, instead of hand-maintaining the same
algorithm in three places (ducklink `lib.rs`, sqlink `lib.rs`, sqlink
`embed.rs`) with three different registration ABIs and value-marshalling
conventions.

This realizes CONSOLIDATION.md step 2 (land `ValueModel`/`NeutralType`
first, no wasmtime) — the extension cores are its natural first consumer.

## The pieces

| Crate / dir | Role |
| --- | --- |
| `crates/datalink-valuemodel` | `NeutralType` + `NeutralValue` (the closed FROZEN-v1 type set) + a single `Complex` escape hatch (DuckDB `complex(type-expr,json)` ~= SQLite `wit-value`). `FnDecl` / `NullHandling`. No wasmtime, `no_std + alloc`. |
| `crates/datalink-extcore` | The `declare!` macro (a core names its capability table once) + the `duckdb_shim!` / `sqlite_shim!` / `embed_shim!` codegen macros + the `ArgExt` arg-helper trait. Parameterized by each repo's WIT binding paths. |
| `extensions/<name>-core` | The neutral logic + the `declare!` table. Built on `datalink-extcore`; never names a host WIT type. |

## The split (measured on the pilots)

The CORE is the logic + a ~6-line-per-function declaration. Everything
else — registration, handle/func-id dispatch, the six DuckDB `call_*`
arms, the SQLite `describe()`/`Manifest`, value marshalling, NULL
handling — is GENERATED. Each generated shim's hand-written surface is
the `wit_bindgen::generate!` block + ONE macro invocation:

- `aba`: core declares 3 scalars in ~30 lines of logic + a 16-line
  `declare!`; each generated shim is ~12 lines (gen + one macro).
- `baseN`: core declares 6 scalars; same ~12-line generated shims.

## Per-DB conventions the codegen encodes

- **Boolean**: DuckDB `Duckvalue::Boolean(bool)` vs SQLite
  `SqlValue::Integer(0|1)`. A core declares `boolean`; both shims do the
  right thing.
- **NULL**: DuckDB's C scalar API propagates NULL by default; SQLite has
  no host default, so the generated SQLite/embed dispatch enforces
  `NullHandling::Propagate` itself. Both reach the same `NULL` result.
- **NULL-on-error** (Option->Null) and **BLOB<->TEXT** live in the core
  body (e.g. `baseN` decode returns `NeutralValue::Null` on failure).
- **Escape hatch**: anything outside the closed set rides `Complex` ->
  `complex(type-expr,json)` / `wit-value`. The codegen NEVER emits a new
  `duckvalue`/`logicaltype`/`sql-value` arm (the frozen-type-set rule).

## Per-repo WIT parameterization

The two repos are on different FROZEN contracts and different
wit-bindgen versions; each consuming crate runs its own
`wit_bindgen::generate!`. The shim macros take the resulting binding
PATHS as parameters — nothing hardcodes one repo's package/version:

- ducklink: `duckdb:extension@2.2.0`, wit-bindgen 0.41, world
  `duckdb:extension/duckdb-extension`.
- sqlink: `sqlite:extension@1.0.0`, wit-bindgen 0.44, world `minimal`.

## Authoring a core (the Phase-1 recipe)

For each of the ~49 still-overlapping scalar extensions:

1. Create `extensions/<name>-core` depending on `datalink-extcore`
   (+ any upstream codec crates, with the same feature flags the
   pre-pullup extensions used — see each repo's `compat-registry.json`).
2. Move the DB-agnostic logic into a `logic` module (`no_std + alloc`).
   RECONCILE any surface drift to the SUPERSET so both DBs gain it.
3. Write the `declare!` table: one `scalar name(args) -> ret
   [null_handling, deterministic] = |args| { ... }` line per function.
4. Generate the two shims (and sqlink's embed target) by adding a tiny
   crate per repo whose entire body is `wit_bindgen::generate!` + the
   matching `duckdb_shim!` / `sqlite_shim!` / `embed_shim!` invocation.
5. Build to a wasm component and diff its smoke/conformance output
   against the committed baseline. Parity (or an explained reconciled
   drift) is the gate; then the 2-3 hand-written sources are deleted.

The capability gradient (from the design): scalar = clean codegen
(this); aggregate = a richer template (state init/step/finalize); table
fns = mostly (row-production shape differs); the DB-only long tail
(DuckDB replacement-scan/storage/catalog; SQLite vtab/hooks/authorizer/
dot-commands) stays hand-written and DB-private.
