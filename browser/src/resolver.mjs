// DB-agnostic resolver shim over a datalink registry index (registry/index.json).
//
// Selects the wasm provider for a named extension, gated by the conformance
// contract, and verifies content integrity before the caller instantiates it.
// The registry schema (entries with `wit_contract` + `providers[].conformance`)
// is shared across DuckDB and SQLite (CONSOLIDATION.md Tier 1), so this lives in
// datalink/browser. Engine-specific typed errors are thrown via injected
// constructors so the facade keeps its own error taxonomy.
import { fetchBytes, sha256Hex } from './fetch-bytes.mjs'

/**
 * Select the conformant wasm provider for `name` from a loaded registry index.
 *
 * THE GATE: a provider qualifies iff `kind === 'wasm'`,
 * `conformance.passed === true`, and `conformance.at === entry.wit_contract`
 * (the provider was conformance-tested against the contract the entry declares).
 *
 * @param {object} registry  parsed registry/index.json
 * @param {string} name      extension name
 * @param {object} [errors]  { NotFound, Conformance, Contract } error constructors
 * @returns {{ entry:object, provider:object }}
 */
export function selectProvider(registry, name, errors = {}) {
  const {
    NotFound = Error,
    Conformance = Error,
    Contract = Error,
  } = errors
  const exts = registry?.extensions || []
  const entry = exts.find((e) => e.name === name)
  if (!entry) throw new NotFound(`extension '${name}' is not in the registry`)

  const wasm = (entry.providers || []).filter((p) => p.kind === 'wasm')
  if (wasm.length === 0) {
    throw new Conformance(`extension '${name}' has no wasm provider`)
  }

  // Prefer the reference provider when several qualify (deterministic precedence).
  const ordered = [...wasm].sort((a, b) => (b.reference === true) - (a.reference === true))
  for (const p of ordered) {
    const conf = p.conformance || {}
    if (!conf.passed) continue
    if (conf.at !== entry.wit_contract) {
      // The provider exists but was tested against a different contract digest.
      throw new Contract(
        `extension '${name}' wasm provider conformance.at (${short(conf.at)}) ` +
          `does not match entry wit_contract (${short(entry.wit_contract)})`,
      )
    }
    return { entry, provider: p }
  }
  throw new Conformance(
    `extension '${name}' has a wasm provider but none passed conformance against the declared contract`,
  )
}

/**
 * Resolve + fetch + verify an extension's wasm bytes.
 *
 * @param {object} opts
 * @param {object} opts.registry        parsed registry index
 * @param {string} opts.name            extension name
 * @param {(p:object,e:object)=>string} opts.artifactUrl  maps (provider,entry) -> fetchable URL
 * @param {object} [opts.errors]        typed-error constructors (see selectProvider; + Instantiation)
 * @returns {Promise<{ entry:object, provider:object, bytes:Uint8Array, url:string }>}
 */
export async function resolveExtension(opts) {
  const { registry, name, artifactUrl, errors = {} } = opts
  const { Conformance = Error, Instantiation = Error } = errors
  const { entry, provider } = selectProvider(registry, name, errors)
  const url = artifactUrl(provider, entry)

  let bytes
  try {
    bytes = await fetchBytes(url)
  } catch (e) {
    throw new Instantiation(`failed to fetch '${name}' from ${url}: ${e?.message || e}`)
  }

  // Verify content integrity against the provider's declared digest (the gate is
  // worthless if the bytes can be swapped). content_digest is plain sha256 hex.
  const expected = provider.content_digest || entry.content_digest
  if (expected) {
    const got = await sha256Hex(bytes)
    if (got !== expected) {
      throw new Conformance(
        `extension '${name}' content digest mismatch: expected ${short(expected)}, got ${short(got)}`,
      )
    }
  }

  return { entry, provider, bytes, url }
}

function short(h) {
  return typeof h === 'string' ? h.slice(0, 12) : String(h)
}
