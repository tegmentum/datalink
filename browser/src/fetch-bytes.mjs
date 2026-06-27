// Fetch bytes over HTTPS (or a relative URL) as a Uint8Array.
export async function fetchBytes(url) {
  const r = await fetch(url)
  if (!r.ok) throw new Error(`fetch ${url}: ${r.status}`)
  return new Uint8Array(await r.arrayBuffer())
}

// sha256 hex of bytes, via SubtleCrypto (browser + Node 20+).
export async function sha256Hex(bytes) {
  const buf = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes)
  const digest = await crypto.subtle.digest('SHA-256', buf)
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('')
}
