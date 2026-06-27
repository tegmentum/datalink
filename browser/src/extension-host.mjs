// In-browser extension host with a MULTI-EXTENSION ROUTER.
//
// Lifted + generalized from ducklink/web/extension-host.mjs (CONSOLIDATION.md
// Tier 3) with the single-extension limitation removed. The original routes
// every callback to the first-loaded extension and drains registrations once,
// so load('a') + load('b') would collide. Here each loaded extension gets its
// own GLOBAL handle namespace (a per-host offset window) so the core's callback
// handle uniquely identifies which extension instance to dispatch to, and the
// pending registrations accumulate across loads instead of draining once.
//
// The registration record shapes (scalar/table/aggregate/macro/cast/...) are
// the duckdb:extension WIT contract, camelCased for jco. A #129 WIT type bump
// touches these shapes; isolate that change to this file's buildExtensionImports.
import { createRuntimeBindgen } from '@tegmentum/wasi-polyfill/wasip2/runtime'

const HANDLE_WINDOW = 1_000_000 // per-extension handle window; 1M handles/ext

function emptyPending() {
  return {
    scalars: [], tables: [], aggregates: [], macros: [],
    replacementScans: [], logicalTypes: [], casts: [],
  }
}

/**
 * Create a multi-extension host.
 *
 * @param {object} opts
 * @param {() => import('@tegmentum/wasi-polyfill/wasip2').Polyfill} opts.configurePolyfill  factory for a fresh polyfill
 * @param {object} [opts.jspi]  JSPI config for extension components (asyncMode/asyncImports/asyncExports)
 */
export function createExtensionHost(opts) {
  const { configurePolyfill, jspi } = opts || {}
  if (typeof configurePolyfill !== 'function') {
    throw new Error('createExtensionHost: opts.configurePolyfill factory is required')
  }

  // name -> { instance, pending, base }  (base = handle-window start for this ext)
  const loaded = new Map()
  // global handle -> the extension record that owns it (for routing dispatch)
  const handleToExt = new Map()
  let nextBase = HANDLE_WINDOW

  // Already-drained pending count per name, so a second getPendingRegistrations
  // after a later load() returns only the NEW registrations (the core drains on
  // every call). Tracked per-name so each new load is drained exactly once.
  const drainedNames = new Set()

  // Build the duckdb:extension/* imports an extension component needs. Handles
  // returned to the extension are GLOBAL (base + local), so the core's later
  // callback handle routes uniquely to this instance.
  function buildExtensionImports(pending, base, registerHandle) {
    let nextLocal = 1
    const id = () => {
      const h = base + nextLocal++
      registerHandle(h)
      return h
    }
    const tableHandles = new Map() // global handle -> function name

    class ScalarCallback { constructor(h) { this.handle = h } }
    class TableCallback { constructor(h) { this.handle = h } }
    class AggregateCallback { constructor(h) { this.handle = h } }
    class PragmaCallback { constructor(h) { this.handle = h } }
    class CastCallback { constructor(h) { this.handle = h } }

    class ScalarRegistry {
      register(name, args, returns, cb, options) {
        pending.scalars.push({ name, arguments: args, returns, callbackHandle: cb.handle, options })
        return id()
      }
    }
    class TableRegistry {
      register(name, args, columns, cb, options) {
        const handle = id()
        pending.tables.push({ name, arguments: args, columns, callbackHandle: cb.handle, options })
        tableHandles.set(handle, name)
        return handle
      }
    }
    class AggregateRegistry {
      register(name, args, returns, cb, options) {
        pending.aggregates.push({ name, arguments: args, returns, callbackHandle: cb.handle, options })
        return id()
      }
    }
    class PragmaRegistry {
      registerCall() { return id() }
    }
    class MacroRegistry {
      registerScalar() { return true }
    }

    const runtime = {
      ScalarCallback, TableCallback, AggregateCallback, PragmaCallback, CastCallback,
      ScalarRegistry, TableRegistry, AggregateRegistry, PragmaRegistry, MacroRegistry,
      getCapability(kind) {
        switch (kind) {
          case 'scalar': return { tag: 'scalar', val: new ScalarRegistry() }
          case 'table': return { tag: 'table', val: new TableRegistry() }
          case 'aggregate': return { tag: 'aggregate', val: new AggregateRegistry() }
          case 'pragma': return { tag: 'pragma', val: new PragmaRegistry() }
          case 'macro': return { tag: 'macro', val: new MacroRegistry() }
          default: return undefined
        }
      },
      listCapabilities: () => ['scalar', 'table', 'aggregate', 'pragma', 'macro'],
    }

    const catalog = {
      CastCallback,
      registerLogicalType(ty) {
        pending.logicalTypes.push({ name: ty.name, physical: ty.physical })
        return id()
      },
      registerCast(spec, cb) {
        pending.casts.push({ source: spec.from, target: spec.to, callbackHandle: cb.handle })
      },
      registerMacro(def) {
        pending.macros.push({
          schema: def.schema, name: def.name,
          parameters: def.parameters, definitionSql: def.definitionSql,
        })
      },
    }

    const files = {
      registerReplacementScan(scan) {
        pending.replacementScans.push({
          extensions: scan.extensions,
          functionName: tableHandles.get(scan.tableFunction) ?? '',
        })
        return id()
      },
      registerCopyHandler() {
        throw new Error('copy handlers are not supported')
      },
    }

    return {
      'duckdb:extension/runtime': runtime,
      'duckdb:extension/catalog': catalog,
      'duckdb:extension/files': files,
      'duckdb:extension/types': {},
    }
  }

  return {
    /** Names of currently loaded extensions, in load order. */
    loadedNames() { return [...loaded.keys()] },

    /** True if `name` is loaded. */
    has(name) { return loaded.has(name) },

    // Pre-load an extension component so the synchronous core path can use it.
    async preload(name, bytes) {
      if (loaded.has(name)) return // idempotent
      const pending = emptyPending()
      const base = nextBase
      nextBase += HANDLE_WINDOW
      const rec = { instance: null, pending, base }
      const registerHandle = (h) => handleToExt.set(h, rec)

      const polyfill = configurePolyfill()
      const bindgen = createRuntimeBindgen({
        polyfill,
        additionalImports: buildExtensionImports(pending, base, registerHandle),
        jcoOptions: jspi
          ? { asyncMode: jspi.asyncMode ?? 'jspi', asyncImports: jspi.asyncImports ?? [], asyncExports: jspi.asyncExports ?? [] }
          : undefined,
      })
      const inst = await bindgen.instantiate(bytes)
      const ext = inst.exports ?? inst
      ext.guest.load() // runs registrations into `pending` (with global handles)
      rec.instance = ext
      loaded.set(name, rec)
    },

    // Imports the CORE component needs (pass via its additionalImports). Routes
    // each callback to the extension that owns the handle (the multi-ext router).
    coreImports() {
      const route = (handle) => {
        const rec = handleToExt.get(handle)
        if (!rec) throw new Error('no extension loaded for callback handle ' + handle)
        return rec.instance
      }
      const dispatch = (method) => (handle, ...rest) =>
        route(handle).callbackDispatch[method](handle, ...rest)

      return {
        'duckdb:component/host-extension-loader': {
          requestLoad: (name) => loaded.has(name),
        },
        'duckdb:component/extension-loader-hooks': {
          getPendingRegistrations: () => {
            // Drain only extensions not yet drained, accumulating their pending
            // registrations. A later load() adds a new (undrained) name, so the
            // next call returns just that extension's registrations.
            const all = emptyPending()
            for (const [name, rec] of loaded) {
              if (drainedNames.has(name)) continue
              drainedNames.add(name)
              for (const k of Object.keys(all)) all[k].push(...(rec.pending[k] ?? []))
            }
            return all
          },
        },
        'duckdb:extension/callback-dispatch': {
          callScalar: dispatch('callScalar'),
          // The core->host crossing is batched (one call per chunk); the
          // extension is invoked per row. Each per-row call may be JSPI-promised
          // (socket-using scalars suspend on async I/O), so await sequentially to
          // preserve row + connection-state ordering. Row i's index is base + i.
          callScalarBatch: async (handle, rows, ctx) => {
            const ext = route(handle)
            const base = ctx.rowindex ?? 0n
            const out = []
            for (let i = 0; i < rows.length; i++) {
              out.push(
                await ext.callbackDispatch.callScalar(handle, rows[i], {
                  rowindex: base + BigInt(i),
                  iswindow: ctx.iswindow,
                }),
              )
            }
            return out
          },
          callTable: dispatch('callTable'),
          callAggregate: dispatch('callAggregate'),
          callPragma: dispatch('callPragma'),
          callCast: dispatch('callCast'),
        },
      }
    },
  }
}
