//! Neutral core for the `cache` DuckDB extension — the wasm-side
//! counterpart to the native `duckdb-cache` (see
//! https://github.com/tegmentum/duckdb-cache).
//!
//! # Surface
//!
//! Two scalar overloads, byte-identical to the native extension's:
//!
//! ```text
//! cache(uri VARCHAR) -> VARCHAR
//! cache(config_json VARCHAR, uri VARCHAR) -> VARCHAR
//! ```
//!
//! Given a URI, the scalar returns a `file://` URI pointing at a locally
//! cached copy of the same bytes. `file://` inputs are returned unchanged.
//! Anything else supported by the resolver (currently `http` / `https`;
//! `s3` / `gs` / `az` / `azure` are stubbed with the same error string
//! the native uses) is fetched, hashed, atomically published into the
//! content-addressed store, and recorded in a SQLite catalog.
//!
//! # Layout parity
//!
//! The on-disk layout is byte-compatible with `duckdb-cache`:
//!
//! ```text
//! <cache_root>/
//!   metadata.db                     -- SQLite catalog (schema below)
//!   objects/<sha256[:2]>/<sha256[2:]>
//!   locks/<sha256-of-source-uri>.lock
//!   tmp/<random>
//! ```
//!
//! [`SCHEMA_SQL`] mirrors `duckdb-cache/src/schema.rs`.
//!
//! # Split of responsibilities
//!
//! This crate stays `no_std + alloc` so the same core compiles for
//! either DB shim. It owns:
//!
//! - the `declare!` capability table (`cache(uri)` +
//!   `cache(config_json, uri)`),
//! - JSON [`Config`] parsing (mirrors the native's `config.rs`),
//! - the [`Policy`] loop and the `file://` pass-through logic
//!   (mirrors the native's `resolver.rs`),
//! - the sha256 helper and the content-addressed path math
//!   (mirrors the native's `store.rs`),
//! - the metadata schema SQL as constants.
//!
//! The actual I/O — running SQL on the sibling metadata catalog,
//! filesystem writes, HTTP fetches — is delegated to function pointers
//! the shim installs at `load()`. Function-pointer indirection here
//! mirrors the pattern `fieldbook-core` uses for `nested_exec` (a
//! `declare!` scalar body cannot capture host state, but it can call
//! through an installed pointer).

// `std` (not `no_std`): `serde_json` is built with std here; `extern crate
// alloc` keeps the `declare!`-generated `::alloc` paths resolvable.
extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicPtr, Ordering};

use datalink_extcore::NeutralValue;
// `ArgExt` supplies `arg_text` on `[NeutralValue]`; imported implicitly
// by the `declare!` macro expansion.

// ---------------------------------------------------------------------------
// Metadata schema (byte-identical to duckdb-cache/src/schema.rs)
// ---------------------------------------------------------------------------

/// The single table the catalog uses. Kept intentionally minimal, and
/// byte-for-byte identical to the native `duckdb-cache` schema so a
/// `metadata.db` written by one arm is readable by the other.
pub const CREATE_CACHE_ENTRIES: &str = "\
CREATE TABLE IF NOT EXISTS cache_entries (
    cache_name     TEXT NOT NULL,
    source_uri     TEXT NOT NULL,
    scheme         TEXT NOT NULL,
    etag           TEXT,
    last_modified  TEXT,
    content_hash   TEXT NOT NULL,
    content_length INTEGER,
    resolved_path  TEXT NOT NULL,
    retrieved_at   TEXT NOT NULL,
    validated_at   TEXT NOT NULL,
    expires_at     TEXT,
    PRIMARY KEY (cache_name, source_uri)
)";

/// Convenience index for admin queries over one blob's referrers.
pub const CREATE_HASH_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_cache_entries_hash ON cache_entries(content_hash)";

/// The two DDL statements the shim runs at metadata-catalog bootstrap.
pub fn schema_sql() -> String {
    let mut s = String::new();
    s.push_str(CREATE_CACHE_ENTRIES);
    s.push_str(";\n");
    s.push_str(CREATE_HASH_INDEX);
    s.push_str(";\n");
    s
}

// ---------------------------------------------------------------------------
// Config (mirrors duckdb-cache/src/config.rs)
// ---------------------------------------------------------------------------

/// Where the cache root sits on disk. `Local` (default) is per-project;
/// `Global` is per-user under the platform's cache home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Local,
    Global,
}

/// How aggressively the cache should try to talk to the origin.
///
/// * `Auto` — revalidate if the origin provided validators, honour a
///   TTL if the caller set one, otherwise trust the on-disk copy.
/// * `Revalidate` — always issue a conditional GET.
/// * `Immutable` — requires `sha256`; never touches the network once
///   the pinned blob is present.
/// * `Offline` — error out if the entry isn't already cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Auto,
    Revalidate,
    Immutable,
    Offline,
}

/// Per-call configuration. `Default::default()` matches the one-argument
/// form `cache(uri)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub scope: Scope,
    pub name: String,
    pub policy: Policy,
    pub sha256: Option<String>,
    pub ttl_secs: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scope: Scope::Local,
            name: String::from("default"),
            policy: Policy::Auto,
            sha256: None,
            ttl_secs: None,
        }
    }
}

/// Raw JSON shape; every field is optional so a caller can omit any of
/// them and inherit the [`Config::default()`] value.
#[derive(Debug, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    policy: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    ttl: Option<String>,
}

impl Config {
    /// Parse the JSON blob a caller passed. An empty / missing config
    /// maps to `Config::default()`.
    pub fn from_json(s: &str) -> Result<Self, String> {
        let raw: RawConfig =
            serde_json::from_str(s).map_err(|e| format!("cache config: {e}"))?;
        let mut cfg = Config::default();
        if let Some(v) = raw.scope {
            cfg.scope = match v.as_str() {
                "local" => Scope::Local,
                "global" => Scope::Global,
                other => return Err(format!("cache config: unknown scope {other:?}")),
            };
        }
        if let Some(v) = raw.name {
            if v.is_empty() {
                return Err(String::from("cache config: name must be non-empty"));
            }
            cfg.name = v;
        }
        if let Some(v) = raw.policy {
            cfg.policy = match v.as_str() {
                "auto" => Policy::Auto,
                "revalidate" => Policy::Revalidate,
                "immutable" => Policy::Immutable,
                "offline" => Policy::Offline,
                other => return Err(format!("cache config: unknown policy {other:?}")),
            };
        }
        if let Some(v) = raw.sha256 {
            if v.len() != 64 || !v.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(String::from(
                    "cache config: sha256 must be 64 lowercase hex chars",
                ));
            }
            cfg.sha256 = Some(v.to_lowercase());
        }
        if let Some(v) = raw.ttl {
            cfg.ttl_secs = Some(parse_ttl(&v)?);
        }
        if cfg.policy == Policy::Immutable && cfg.sha256.is_none() {
            return Err(String::from(
                "cache config: policy \"immutable\" requires a sha256 field",
            ));
        }
        Ok(cfg)
    }
}

/// A tiny suffixed duration parser: accepts `s`, `m`, `h`, `d`. A bare
/// integer is treated as seconds.
fn parse_ttl(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(String::from("cache config: ttl must not be empty"));
    }
    let (num, unit) = split_ttl(s);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("cache config: ttl not a number: {s:?}"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n
            .checked_mul(60)
            .ok_or_else(|| String::from("cache config: ttl overflow"))?,
        "h" => n
            .checked_mul(3600)
            .ok_or_else(|| String::from("cache config: ttl overflow"))?,
        "d" => n
            .checked_mul(86400)
            .ok_or_else(|| String::from("cache config: ttl overflow"))?,
        other => return Err(format!("cache config: unknown ttl unit {other:?}")),
    };
    Ok(secs)
}

fn split_ttl(s: &str) -> (&str, &str) {
    let idx = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    (&s[..idx], &s[idx..])
}

// ---------------------------------------------------------------------------
// sha256 helper + content-addressed path math
// (mirrors duckdb-cache/src/store.rs)
// ---------------------------------------------------------------------------

/// Compute sha256 of a byte slice and return lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    hex_encode(&h.finalize())
}

/// Lowercase-hex-encode a byte slice.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Blob path shard: `objects/<hash[:2]>/<hash[2:]>`. Same 2-hex-char
/// shard the native uses so blobs are byte-portable between the two arms.
pub fn blob_relpath(hash: &str) -> String {
    debug_assert!(hash.len() >= 2, "sha256 hex must be at least 2 chars");
    let mut s = String::with_capacity(8 + 1 + 2 + 1 + (hash.len() - 2));
    s.push_str("objects/");
    s.push_str(&hash[..2]);
    s.push('/');
    s.push_str(&hash[2..]);
    s
}

/// Lock-file path relative to the cache root: `locks/<sha256-of-uri>.lock`.
/// See v0 caveat in the shim: WASI 0.2 does not expose flock cleanly, so
/// the wasm arm does not actually take the lock — the content-addressed
/// rename-atomic publish gives us safe multi-writer semantics even without
/// one (identical bytes hash to identical paths).
pub fn lock_relpath(source_uri: &str) -> String {
    let key = sha256_hex(source_uri.as_bytes());
    format!("locks/{key}.lock")
}

// ---------------------------------------------------------------------------
// Bridge: shim -> core function pointers
//
// A `declare!` scalar body is a bare `fn` (no captured host state), so
// this core reaches for host services through globally installed fn
// pointers — the shim installs them at `load()`. Wasm is single-threaded,
// so `AtomicPtr` is used only to satisfy the mutable-static rules with
// no unsafe.
//
// Kept deliberately small — one bridge per side of the cache boundary:
//
//   * `RESOLVER` — end-to-end resolve; the shim owns the metadata catalog,
//     the filesystem, and the HTTP client, and returns the resolved
//     `file://` URI for the caller.
// ---------------------------------------------------------------------------

/// Resolver function pointer: given the parsed config and the input URI,
/// return the `file://` URI of the cached (or freshly-fetched) blob.
pub type ResolveFn = fn(cfg: &Config, uri: &str) -> Result<String, String>;

static RESOLVER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the shim's resolver. Called once from `load()` before any
/// scalar invocation can reach the core.
pub fn install_resolver(f: ResolveFn) {
    RESOLVER.store(f as *mut (), Ordering::SeqCst);
}

/// Call the installed resolver. Returns a clear error if the shim did
/// not run `install_resolver` (e.g. its `load()` errored earlier).
pub fn resolve(cfg: &Config, uri: &str) -> Result<String, String> {
    let p = RESOLVER.load(Ordering::SeqCst);
    if p.is_null() {
        return Err(String::from(
            "cache: resolver not installed (shim load() did not run)",
        ));
    }
    // SAFETY: The pointer was written by `install_resolver` from a
    // `ResolveFn` value; only that setter writes it, and it is a plain
    // function pointer (no captures, `'static`).
    let f: ResolveFn = unsafe { core::mem::transmute(p) };
    f(cfg, uri)
}

/// The `file://` pass-through the native's `resolver.rs` short-circuits
/// on. Reused by the shim so the two arms agree exactly on which URIs
/// bypass the resolver.
pub fn is_file_scheme(uri: &str) -> bool {
    // Match the native: `Url::parse(uri)` then `url.scheme() == "file"`.
    // We deliberately don't do full URL parsing here — the shim will
    // reject malformed inputs when it tries to fetch them.
    uri.trim_start().to_ascii_lowercase().starts_with("file:")
}

// ---------------------------------------------------------------------------
// Capability declaration
//
// Two overloads — arity 1 uses `Config::default()`, arity 2 parses the
// JSON blob first. Both dispatch through the installed resolver.
// ---------------------------------------------------------------------------

datalink_extcore::declare! {
    core = Core;
    extension = "cache";
    version = env!("CARGO_PKG_VERSION");

    scalar cache_uri(text) -> text [propagate, nondeterministic] = |args| {
        let uri = args.arg_text(0, "cache")?;
        // Pass-through mirrors the native: `file://` inputs return
        // verbatim, no catalog work.
        if is_file_scheme(&uri) {
            return Ok(NeutralValue::Text(uri));
        }
        let out = resolve(&Config::default(), &uri)?;
        Ok(NeutralValue::Text(out))
    };

    scalar cache_cfg_uri(text, text) -> text [propagate, nondeterministic] = |args| {
        let cfg_json = args.arg_text(0, "cache")?;
        let uri = args.arg_text(1, "cache")?;
        if is_file_scheme(&uri) {
            return Ok(NeutralValue::Text(uri));
        }
        let cfg = if cfg_json.is_empty() {
            Config::default()
        } else {
            Config::from_json(&cfg_json)?
        };
        let out = resolve(&cfg, &uri)?;
        Ok(NeutralValue::Text(out))
    };
}

// ---------------------------------------------------------------------------
// The two DECL entries above register as `cache_uri` and `cache_cfg_uri`
// because `declare!` uses the identifier verbatim as the DuckDB function
// name and DuckDB does not resolve overloads across `register_scalar`
// entries with distinct names. The shim rewrites `Core::DECLS[i].name`
// to `"cache"` for both entries at registration time — DuckDB itself
// dispatches on arity for the two overloads.
// ---------------------------------------------------------------------------

/// The single SQL name both overloads register under. The shim reads
/// this constant instead of `FnDecl::name` so both `Core::DECLS` entries
/// register as `cache(...)` overloads.
pub const REGISTERED_NAME: &str = "cache";
