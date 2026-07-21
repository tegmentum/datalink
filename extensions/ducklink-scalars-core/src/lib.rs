//! Neutral core for the two always-available ducklink control-plane scalars.
//!
//!   * `ducklink_version() -> text`
//!     Returns `"ducklink X.Y.Z"` where the version is baked in at build time.
//!     Self-contained smoke test — `SELECT ducklink_version();` proves the
//!     ducklink surface loaded correctly on any host.
//!
//!   * `ducklink_help(name text) -> text`
//!     Renders markdown help for a named function or module. On hosts that
//!     ship a populated `ducklink.docs` view this returns the rendered doc;
//!     until then, and as a portable fallback, it returns a "no help
//!     available for '<name>'" line so the surface stays predictable across
//!     hosts even before docs infrastructure exists.
//!
//! Committed as part of the shared surface in
//! `ducklink-extension/STABILITY.md § 1.1`. Both the native extension and
//! the workspace host must register these; this core is the shared
//! implementation that lets each host mint its own binding.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod logic {
    use alloc::format;
    use alloc::string::String;

    /// The version string returned by `ducklink_version()`. Prefixed with
    /// `"ducklink "` for parity with the native extension's implementation
    /// (see `ducklink-extension/src/reg_duckdb.rs::DucklinkVersion`).
    ///
    /// The caller passes in the version — usually `env!("CARGO_PKG_VERSION")`
    /// from the shim crate — so each host reports its own crate version.
    pub fn version(v: &str) -> String {
        let mut out = String::with_capacity(9 + v.len());
        out.push_str("ducklink ");
        out.push_str(v);
        out
    }

    /// Portable fallback body for `ducklink_help(name)` on hosts that don't
    /// (yet) populate `ducklink.docs`. Returns a legible markdown line the
    /// caller can render as-is. Hosts that DO have a docs view should query
    /// it first and only fall back to this on miss.
    pub fn help_fallback(name: &str) -> String {
        format!("# {name}\n\nNo help available.\n")
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "ducklink_scalars";
    version = env!("CARGO_PKG_VERSION");

    scalar ducklink_version() -> text [propagate, deterministic] = |_args| {
        Ok(NeutralValue::Text(logic::version(env!("CARGO_PKG_VERSION"))))
    };

    scalar ducklink_help(text) -> text [propagate, deterministic] = |args| {
        let name = args.arg_text(0, "ducklink_help")?;
        Ok(NeutralValue::Text(logic::help_fallback(&name)))
    };
}
