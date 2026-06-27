// DB-agnostic component instantiation for datalink browser runtimes.
//
// Lifted + generalized from ducklink/web/run-core.mjs `instantiateCore`
// (CONSOLIDATION.md Tier 3). The mechanism — jco-transpile the wasip2 component
// via createRuntimeBindgen, wire the polyfill's import providers, and configure
// JSPI (async suspend) — is identical for any SQL engine. The DuckDB-specific
// pieces (the TVM spill host imports, the exact async import/export list) are
// passed in, so SQLite or any other engine reuses this verbatim.
//
// Browser-targeted: createRuntimeBindgen imports the jco-generated module from a
// `blob:` URL (browser-only). Use from a browser bundle (Vite serving native
// ESM with jco/wasi-polyfill excluded from dep pre-bundling).
import { createRuntimeBindgen } from '@tegmentum/wasi-polyfill/wasip2/runtime'

/** JSPI feature-detect: WebAssembly.Suspending requires Chrome 137+ (or a flag). */
export function jspiAvailable() {
  return typeof WebAssembly !== 'undefined' && typeof WebAssembly.Suspending === 'function'
}

/**
 * Transpile + instantiate a wasip2 SQL-engine component.
 *
 * @param {Uint8Array} componentBytes  the engine component wasm
 * @param {object} opts
 * @param {import('@tegmentum/wasi-polyfill/wasip2').Polyfill} opts.polyfill  configured polyfill
 * @param {Record<string, any>} [opts.additionalImports]  host imports the component needs
 * @param {object} [opts.jspi]    JSPI config: { asyncMode, asyncImports, asyncExports }
 * @returns {Promise<{ instance:any, exports:any }>}
 */
export async function instantiateComponent(componentBytes, opts) {
  const { polyfill, additionalImports = {}, jspi } = opts || {}
  if (!polyfill) throw new Error('instantiateComponent: opts.polyfill is required')

  const jcoOptions = jspi
    ? { asyncMode: jspi.asyncMode ?? 'jspi', asyncImports: jspi.asyncImports ?? [], asyncExports: jspi.asyncExports ?? [] }
    : undefined

  const bindgen = createRuntimeBindgen({ polyfill, additionalImports, jcoOptions })
  const instance = await bindgen.instantiate(componentBytes)
  const exports = instance.exports ?? instance
  return { instance, exports }
}
