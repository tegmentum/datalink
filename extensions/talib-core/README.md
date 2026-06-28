# talib-core

DB-neutral TA-Lib-style technical indicators, written ONCE and generated
into both the ducklink (`duckdb:extension`) and sqlink
(`sqlite:extension`) shims by `datalink-extcore`.

## The window model

Indicators are declared as `aggregate`s in the core's `declare!` table and
computed over the rows of the *current frame*. The period is the SQL
`OVER (... ROWS BETWEEN n-1 PRECEDING AND CURRENT ROW)` width, not a
separate argument, e.g. a 3-period SMA:

```sql
sma(close) OVER (ORDER BY t ROWS BETWEEN 2 PRECEDING AND CURRENT ROW)
```

The same neutral `dispatch_aggregate` fold drives both engines:

* **ducklink / DuckDB** — registered as an aggregate; DuckDB's window
  executor resolves the frame and calls the aggregate over exactly the
  frame's rows (the `call-aggregate-window` path). Proven via
  `datalink-extcore::duckdb_agg_shim!`.
* **sqlink / SQLite** — advertised `is-window=true`; the loader registers
  it through `create_window_function` and SQLite drives
  `xStep / xInverse / xValue / xFinal`. The generated
  `datalink-extcore::sqlite_agg_shim!` keeps the frame's rows in a
  per-context FIFO buffer (step pushes, inverse pops the oldest) and
  re-runs this same fold for each `value()` — so a core never has to write
  a bespoke incremental inverse.

## Landed indicators

| name  | form                         | notes                                   |
|-------|------------------------------|-----------------------------------------|
| `sma` | mean of the frame            | simple moving average                   |
| `ema` | EMA, `alpha = 2/(N+1)`       | N = frame size; seeded with first value |
| `rsi` | 100 - 100/(1+RS) over frame  | simple-average gain/loss; <2 rows = NULL|

All three are proven end-to-end in BOTH ducklink and sqlink from this one
core (see each shim dir's `smoke.sql`).

## Deferred

* `macd` — MACD is `EMA(fast) - EMA(slow)` with two distinct periods and
  classically a 3-line output (MACD / signal / histogram). It does not map
  to a single-frame, single-output window aggregate. Options for a later
  pass: a 2-arg scalar-over-ordered-series form, or a struct/`complex()`
  return carrying the three lines. Documented as out-of-scope for the
  window-pipeline-establishing pass.
* The wider TA-Lib catalog (WMA, DEMA, TEMA, Bollinger, ATR, Stochastic,
  ...) fans out cheaply now that the window pipeline exists: add the math
  here as another `aggregate` declaration and BOTH shims regenerate. Multi-
  input indicators (high/low/close) need a multi-arg frame row, which the
  neutral row already supports.
