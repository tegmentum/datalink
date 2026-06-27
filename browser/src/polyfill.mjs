// DB-agnostic WASI polyfill configuration for datalink browser runtimes.
//
// Lifted + generalized from ducklink/web/run-core.mjs `configurePolyfill`
// (CONSOLIDATION.md Tier 3). The original hard-codes DuckDB's `~/.duckdb`
// mkdir and the ws-gateway default; here those are options so both DuckDB and
// SQLite (and any other wasip2 component) can share one configurator.
//
// The plugin set (cli/io/fs/clocks/random/sockets on @tegmentum/wasi-polyfill)
// is identical across engines; only the filesystem preopens/mkdirs and the
// socket gateway differ per engine, so they are parameters.
import { Polyfill, AllowAllPolicy } from '@tegmentum/wasi-polyfill/wasip2'
import * as cli from '@tegmentum/wasi-polyfill/wasip2/plugins/cli'
import * as io from '@tegmentum/wasi-polyfill/wasip2/plugins/io'
import * as fsplugin from '@tegmentum/wasi-polyfill/wasip2/plugins/filesystem'
import * as clocks from '@tegmentum/wasi-polyfill/wasip2/plugins/clocks'
import * as random from '@tegmentum/wasi-polyfill/wasip2/plugins/random'
import * as sockets from '@tegmentum/wasi-polyfill/wasip2/plugins/sockets'

/**
 * Build a configured wasi-polyfill `Polyfill` for a browser SQL-engine
 * component.
 *
 * @param {object} [opts]
 * @param {Array<{path:string}>} [opts.preopens]  filesystem preopens (default `[{path:'/'}]`)
 * @param {string[]} [opts.mkdirs]                directories to pre-create (engine state dir; non-recursive)
 * @param {boolean} [opts.network]                enable real browser networking (DoH + ws-gateway tcp)
 * @param {string}  [opts.gatewayUrl]             ws-gateway URL for tunneled tcp (when network)
 * @param {Record<string,string[]>} [opts.staticDnsMappings] static DNS name->addr mappings
 * @param {boolean} [opts.asyncReadYield]         yield a macrotask on empty reads (network tunnels)
 * @param {typeof AllowAllPolicy} [opts.PolicyBase]  policy base class (default dev AllowAllPolicy)
 * @param {import('@tegmentum/wasi-polyfill/wasip2/plugins/filesystem').MemoryFileSystem} [opts.memfs]
 *   a shared in-memory filesystem to seed the polyfill with (so the caller can
 *   write files the guest sees, e.g. registerFileBuffer). When omitted, a fresh
 *   isolated FS is created. Reachable afterwards as `polyfill.__memfs`.
 */
export function configurePolyfill(opts = {}) {
  const {
    preopens = [{ path: '/' }],
    mkdirs = [],
    network = false,
    gatewayUrl,
    staticDnsMappings = { localhost: ['::1'] },
    asyncReadYield = true,
    PolicyBase = AllowAllPolicy,
    memfs,
  } = opts

  // A caller-controlled MemoryFileSystem so registerFile* can write files the
  // guest resolves (FROM 'name.csv'). Seeded as prepopulatedFs for this polyfill
  // context; the mkdirs are pre-created on it too. MemoryFileSystem comes from
  // the same namespace import as the fs plugins (a separate named import of the
  // filesystem module double-instantiates it under Vite and breaks the plugins).
  const memFs = memfs || new fsplugin.MemoryFileSystem()
  for (const dir of mkdirs) {
    try { memFs.mkdirp(dir) } catch {}
  }

  // On an empty read return a Promise that yields a macrotask then re-reads, so
  // a guest that busy-drains a socket (read_to_end) over a WebSocket tunnel
  // doesn't starve the browser event loop. No-op unless `input-stream.read` is
  // marked async in the JSPI transpile (see runtime.mjs asyncImports).
  io.setAsyncReadYield(asyncReadYield)

  const resolvedGateway =
    gatewayUrl ||
    (typeof globalThis !== 'undefined' && globalThis.__WS_GATEWAY_URL__) ||
    'ws://localhost:8080'

  class EnginePolicy extends PolicyBase {
    configure(iface) {
      const cfg = super.configure(iface)
      if (iface.package === 'wasi:filesystem') {
        // A writable in-memory FS with a `/` preopen + the engine's pre-created
        // state dir (CreateDirectory is non-recursive in the engines). Seed the
        // caller-controlled FS so registerFile* writes are visible to the guest.
        cfg.implementation = 'memory'
        cfg.options = { ...(cfg.options || {}), preopens, mkdirs, prepopulatedFs: memFs }
      }
      if (iface.package === 'wasi:sockets' && network) {
        if (iface.name === 'ip-name-lookup') {
          cfg.implementation = 'doh'
          cfg.options = { ...(cfg.options || {}), staticMappings: staticDnsMappings }
        } else if (iface.name === 'tcp' || iface.name === 'tcp-create-socket') {
          cfg.implementation = 'tunneled'
          cfg.options = { ...(cfg.options || {}), gatewayUrl: resolvedGateway }
        }
      }
      return cfg
    }
  }

  const polyfill = new Polyfill({ policy: new EnginePolicy() })
  for (const p of [
    cli.environmentPlugin,
    cli.exitPlugin, cli.stdoutPlugin, cli.stderrPlugin, cli.stdinPlugin,
    cli.terminalInputPlugin, cli.terminalOutputPlugin, cli.terminalStdinPlugin,
    cli.terminalStdoutPlugin, cli.terminalStderrPlugin,
    io.streamsPlugin, io.pollPlugin, io.errorPlugin,
    fsplugin.filesystemTypesPlugin, fsplugin.filesystemPreopensPlugin,
    clocks.monotonicClockPlugin, clocks.wallClockPlugin,
    random.randomPlugin, random.insecureRandomPlugin, random.insecureSeedPlugin,
    // Engines link socket-using extensions and import wasi:sockets
    // unconditionally; register the (virtual) socket plugins so the component
    // instantiates even when no query touches the network.
    ...sockets.socketPlugins,
  ]) {
    polyfill.registerPlugin(p)
  }
  // Expose the shared FS so the facade's registerFile* can write into it.
  polyfill.__memfs = memFs
  return polyfill
}
