# lessons-learned.md — shared format spec

The lessons-learned feedback system is DB-agnostic *machinery* seeded from the
same source across sqlink and ducklink. The **format/convention** is shared and
lives here; the **entries** stay per-repo (each repo keeps its own
`tooling/lessons-learned.md`). `t-status.py` scans whatever doc the config points
at (`feedback.lessons_doc`).

## Header

Open each repo's `lessons-learned.md` with:

1. A one-line title naming the engine, e.g.
   `# Lessons learned — <DB>-wasm extensions`.
2. A short paragraph stating the feedback loop: every porting friction becomes a
   *tooling* item (something the scaffolder / smoke harness / compat-registry /
   core should do better). Implementations drive the tooling design.
3. The **T-N convention** block (below).
4. A `## Retrospectives` section; append one entry per ship at the BOTTOM.

## The T-N convention (scanned by t-status.py)

Tooling items are tagged inline with markers `t-status.py` scans (case-insensitive,
anywhere in a line):

- `(T-N new)`    — opens tooling item N. Put a short title right after it.
- `(T-N closed)` — closes item N. Any sub-clause works: "closed inline",
  "closed in same doc", "silently closed", ...

Rules:

- An item is **open** iff it has a `new` marker and no `closed` marker.
- Numbers are allocated once and never reused.
- First-match-wins: the original open/close marker is canonical; later mentions
  (including a doc quoting the regex itself) do not move the recorded section.
- The displayed title for `T-N` is its surrounding `### ` markdown section title.

Run:

```
python3 tooling/t-status.py --config <cfg>            # all (open first, then closed)
python3 tooling/t-status.py --config <cfg> open       # just the open ones
python3 tooling/t-status.py --config <cfg> closed      # just the closed ones
```

## Per-entry pattern

```
### YYYY-MM-DD  <extension-name>

**What I built:** one-line summary.

**What worked:** where the tooling paid off.

**What surprised me:** API gotchas, crate quirks, build flags, smoke anomalies.
The compat-registry should grow proportionally.

**Tooling opportunity:** if a friction point felt repeatable, name it — open a
`(T-N new) <title>` here. Periodically batch-review these to find what to
automate next.
```
