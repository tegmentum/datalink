// Minimal promise-based RPC over postMessage (Worker <-> main thread).
//
// The non-JSPI WORKER FALLBACK runs the SQL-engine wasm component inside a Web
// Worker in SYNC transpile mode (no WebAssembly.Suspending), and the main-thread
// facade drives it over this RPC. Because the public facade API is already async
// (create/connect/query return Promises), an id-correlated request/response RPC
// over postMessage maps it cleanly to the worker WITHOUT JSPI and WITHOUT a
// SharedArrayBuffer (so it needs no cross-origin isolation and runs in every
// browser with Web Workers — Firefox, Safari, older Chrome included).
//
// Resources (connections, prepared statements, result streams) live in the
// worker; the main thread holds opaque integer handles and pulls results per
// call, mirroring the WIT resource model. Large byte payloads (wasm bytes, Arrow
// IPC, registered files) are passed as Uint8Array (structured-clone) or, when a
// `transfer` list is given, moved zero-copy.

/**
 * Main-thread side: wrap a Worker (or MessagePort) as an RPC client.
 * @param {Worker|MessagePort} target
 * @returns {{ call:(method:string,args?:any,transfer?:Transferable[])=>Promise<any>, dispose:()=>void }}
 */
export function createRpcClient(target) {
  let nextId = 1
  const pending = new Map() // id -> { resolve, reject }

  const onMessage = (ev) => {
    const msg = ev.data
    if (!msg || typeof msg.__rpc !== 'number') return
    const entry = pending.get(msg.__rpc)
    if (!entry) return
    pending.delete(msg.__rpc)
    if (msg.ok) entry.resolve(msg.result)
    else entry.reject(rehydrateError(msg.error))
  }
  target.addEventListener('message', onMessage)

  // A worker that throws at module-eval / instantiation (e.g. a transpile
  // failure) would otherwise leave every in-flight call hanging forever; surface
  // it as a rejection instead. `error` carries a useful message; `messageerror`
  // means a reply failed to deserialize.
  const onError = (ev) => {
    const detail = ev && (ev.message || ev.error || ev.type) || 'worker error'
    for (const { reject } of pending.values()) reject(new Error('worker error: ' + detail))
    pending.clear()
  }
  if (typeof target.addEventListener === 'function') {
    target.addEventListener('error', onError)
    target.addEventListener('messageerror', onError)
  }

  return {
    call(method, args, transfer) {
      const id = nextId++
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject })
        try {
          target.postMessage({ __rpc: id, method, args }, transfer || [])
        } catch (e) {
          pending.delete(id)
          reject(e)
        }
      })
    },
    dispose() {
      target.removeEventListener('message', onMessage)
      for (const { reject } of pending.values()) reject(new Error('rpc disposed'))
      pending.clear()
    },
  }
}

/**
 * Worker side: serve RPC calls from a handler table.
 * @param {DedicatedWorkerGlobalScope|MessagePort} target  usually `self`
 * @param {Record<string, (args:any)=>any|Promise<any>>} handlers
 *   Each handler returns a result, or `{ __result, __transfer }` to move
 *   transferables back zero-copy.
 */
export function serveRpc(target, handlers) {
  target.addEventListener('message', async (ev) => {
    const msg = ev.data
    if (!msg || typeof msg.__rpc !== 'number' || typeof msg.method !== 'string') return
    const fn = handlers[msg.method]
    if (typeof fn !== 'function') {
      target.postMessage({ __rpc: msg.__rpc, ok: false, error: serializeError(new Error('no such rpc method: ' + msg.method)) })
      return
    }
    try {
      let result = await fn(msg.args)
      let transfer = []
      if (result && result.__rpcTransfer) {
        transfer = result.__transfer || []
        result = result.__result
      }
      target.postMessage({ __rpc: msg.__rpc, ok: true, result }, transfer)
    } catch (e) {
      target.postMessage({ __rpc: msg.__rpc, ok: false, error: serializeError(e) })
    }
  })
}

/** Tag a handler return value so its byte payloads are transferred zero-copy. */
export function withTransfer(result, transfer) {
  return { __rpcTransfer: true, __result: result, __transfer: transfer }
}

// Errors don't structured-clone with their custom class/name/payload, so carry
// the fields explicitly and rebuild a plain Error with them on the client.
function serializeError(e) {
  if (e == null) return { message: 'unknown error' }
  return {
    message: e.message != null ? String(e.message) : String(e),
    name: e.name,
    // DuckDB/engine errors carry a structured `payload` ({tag,val}); preserve it.
    payload: e.payload,
    stack: e.stack,
  }
}

function rehydrateError(info) {
  if (!info) return new Error('unknown rpc error')
  const err = new Error(info.message)
  if (info.name) err.name = info.name
  if (info.payload !== undefined) err.payload = info.payload
  if (info.stack) err.stack = info.stack
  return err
}
