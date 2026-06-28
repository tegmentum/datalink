// datalink/browser — DB-agnostic browser runtime plumbing (CONSOLIDATION.md
// Tier 3). Engine facades (@tegmentum/ducklink, @tegmentum/sqlink) build on
// these shared pieces, supplying only the engine wasm + the value/type model.
export { configurePolyfill } from './polyfill.mjs'
export { instantiateComponent, jspiAvailable } from './runtime.mjs'
export { createExtensionHost } from './extension-host.mjs'
export { selectProvider, resolveExtension } from './resolver.mjs'
export { fetchBytes, sha256Hex } from './fetch-bytes.mjs'
export { createRpcClient, serveRpc, withTransfer } from './worker-rpc.mjs'
