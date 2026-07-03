//! Neutral core for the `changeset` extension — a pure-Rust codec for the
//! SQLite session **changeset** blob format, written ONCE. The per-DB shims
//! (sqlink `sqlite:extension`, sqlink embed, ducklink `duckdb:extension`) are
//! generated from the [`declare!`](datalink_extcore::declare) table below.
//!
//! # Why this exists
//!
//! The previous `changeset` extension was **embed-only**: its five scalars were
//! thin FFI shims over `sqlite3changeset_*` in the CLI's own sqlite3, so it
//! could never ship as a standalone wasm provider (a WIT component that bundled
//! its own sqlite3 would be wasteful, and `libsqlite3-sys`' amalgamation does
//! not build against the wasip1 clang sysroot). This core reimplements the
//! changeset binary format in pure Rust with **zero** C dependency, so the
//! extension becomes provider-able like every other scalar extension.
//!
//! # Fidelity
//!
//! The format was matched line-by-line against `sqlite3session.c` (the
//! amalgamation shipped with the CLI):
//!
//!   * [`invert`](codec::invert) is **byte-for-byte** identical to
//!     `sessionChangesetInvert` — including the PK-aware UPDATE swap
//!     (`apVal[iCol + (abPK ? 0 : nCol)]`) and the `undefined (0x00)` vs
//!     `NULL (0x05)` distinction.
//!   * [`count`](codec::count), [`tables_json`](codec::tables_json) and
//!     [`decode_json`](codec::decode_json) reproduce the old embed path's
//!     output value-for-value (same JSON shape and escaping).
//!   * [`concat`](codec::concat) implements the full change-group merge
//!     (`sessionChangeMerge` + `sessionMergeUpdate`/`sessionMergeRecord`). The
//!     result is a **semantically equivalent** changeset — applying `A` then
//!     `B` has the same effect as applying `concat(A,B)` — but the byte layout
//!     is not guaranteed identical to SQLite's, whose row order follows an
//!     internal per-table PK hash table. This core uses deterministic ordering
//!     (first-seen table, PK-insertion order).
//!
//! # Functions
//!
//!   * `changeset_invert(blob) -> blob`  — swap INSERT/DELETE, swap UPDATE old/new.
//!   * `changeset_concat(blob, blob) -> blob` — merge two changesets.
//!   * `changeset_count(blob) -> int64` — number of change records.
//!   * `changeset_tables(blob) -> text` — JSON array of table names.
//!   * `changeset_decode(blob) -> text` — JSON dump of every change.

#![no_std]

extern crate alloc;

use datalink_extcore::NeutralValue;

pub mod codec {
    //! Pure-Rust SQLite changeset codec. Panic-free on malformed input: every
    //! read is bounds-checked and returns `Err` rather than indexing past the
    //! end. See the module docs for the fidelity guarantees.

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use alloc::collections::BTreeMap;

    // Change-op codes — match SQLITE_INSERT / SQLITE_UPDATE / SQLITE_DELETE.
    pub const OP_DELETE: u8 = 9;
    pub const OP_INSERT: u8 = 18;
    pub const OP_UPDATE: u8 = 23;

    // Serialized value type bytes. 0x00 is the "undefined / no value present"
    // placeholder (used in partial UPDATE records); the rest mirror the
    // SQLITE_* fundamental datatype codes.
    const T_UNDEF: u8 = 0x00;
    const T_INT: u8 = 1;
    const T_FLOAT: u8 = 2;
    const T_TEXT: u8 = 3;
    const T_BLOB: u8 = 4;
    const T_NULL: u8 = 5;

    /// A single serialized changeset value. `Float` keeps the raw IEEE-754
    /// bits so a parse→serialize round-trip is byte-identical (no NaN/precision
    /// drift). `Undef` (absent, 0x00) is deliberately distinct from `Null`
    /// (0x05) — the invert/merge logic depends on the difference.
    #[derive(Clone, PartialEq, Debug)]
    pub enum Val {
        Undef,
        Int(i64),
        Float(u64),
        Text(Vec<u8>),
        Blob(Vec<u8>),
        Null,
    }

    impl Val {
        #[inline]
        pub fn is_defined(&self) -> bool {
            !matches!(self, Val::Undef)
        }
    }

    /// One change record. For INSERT `old` is empty and `new` holds `n_col`
    /// values; for DELETE the reverse; for UPDATE both hold `n_col` values
    /// (with `Undef` placeholders for absent columns).
    #[derive(Clone)]
    pub struct Change {
        pub op: u8,
        pub indirect: u8,
        pub old: Vec<Val>,
        pub new: Vec<Val>,
    }

    /// A table block: its header (column count + PK flags + name) and the
    /// change records that follow it in the stream.
    pub struct Table {
        pub n_col: usize,
        pub pk: Vec<u8>,
        pub name: Vec<u8>,
        pub changes: Vec<Change>,
    }

    // ---- varint (matches sqlite3PutVarint / sqlite3GetVarint) --------------

    /// Append `v` as a SQLite varint (big-endian, 7 data bits per byte, high
    /// bit = continuation; a full 9th byte carries 8 bits). Byte-for-byte
    /// identical to `sqlite3PutVarint`.
    pub fn put_varint(v: u64, out: &mut Vec<u8>) {
        if v <= 0x7f {
            out.push((v & 0x7f) as u8);
            return;
        }
        if v <= 0x3fff {
            out.push((((v >> 7) & 0x7f) | 0x80) as u8);
            out.push((v & 0x7f) as u8);
            return;
        }
        // putVarint64
        if v & (0xff000000u64 << 32) != 0 {
            let mut buf = [0u8; 9];
            buf[8] = v as u8;
            let mut vv = v >> 8;
            for i in (0..8).rev() {
                buf[i] = ((vv & 0x7f) | 0x80) as u8;
                vv >>= 7;
            }
            out.extend_from_slice(&buf);
            return;
        }
        let mut buf = [0u8; 10];
        let mut n = 0usize;
        let mut vv = v;
        loop {
            buf[n] = ((vv & 0x7f) | 0x80) as u8;
            n += 1;
            vv >>= 7;
            if vv == 0 {
                break;
            }
        }
        buf[0] &= 0x7f;
        for j in (0..n).rev() {
            out.push(buf[j]);
        }
    }

    /// Read a SQLite varint from the front of `a`; return `(value, bytes)`.
    /// `None` on truncation. Mirrors `sqlite3GetVarint` (up to 9 bytes).
    fn get_varint(a: &[u8]) -> Option<(u64, usize)> {
        let mut v: u64 = 0;
        for i in 0..9 {
            let b = *a.get(i)?;
            if i == 8 {
                v = (v << 8) | (b as u64);
                return Some((v, 9));
            }
            v = (v << 7) | ((b & 0x7f) as u64);
            if b & 0x80 == 0 {
                return Some((v, i + 1));
            }
        }
        None
    }

    // ---- value (de)serialization ------------------------------------------

    fn ser_val(v: &Val, out: &mut Vec<u8>) {
        match v {
            Val::Undef => out.push(T_UNDEF),
            Val::Null => out.push(T_NULL),
            Val::Int(i) => {
                out.push(T_INT);
                out.extend_from_slice(&i.to_be_bytes());
            }
            Val::Float(bits) => {
                out.push(T_FLOAT);
                out.extend_from_slice(&bits.to_be_bytes());
            }
            Val::Text(b) => {
                out.push(T_TEXT);
                put_varint(b.len() as u64, out);
                out.extend_from_slice(b);
            }
            Val::Blob(b) => {
                out.push(T_BLOB);
                put_varint(b.len() as u64, out);
                out.extend_from_slice(b);
            }
        }
    }

    fn ser_record(r: &[Val], out: &mut Vec<u8>) {
        for v in r {
            ser_val(v, out);
        }
    }

    // ---- bounds-checked input cursor --------------------------------------

    struct Cur<'a> {
        a: &'a [u8],
        i: usize,
    }

    impl<'a> Cur<'a> {
        #[inline]
        fn peek(&self) -> Option<u8> {
            self.a.get(self.i).copied()
        }

        #[inline]
        fn byte(&mut self) -> Result<u8, String> {
            let b = *self.a.get(self.i).ok_or_else(|| trunc())?;
            self.i += 1;
            Ok(b)
        }

        #[inline]
        fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
            let end = self.i.checked_add(n).ok_or_else(|| trunc())?;
            let s = self.a.get(self.i..end).ok_or_else(|| trunc())?;
            self.i = end;
            Ok(s)
        }

        #[inline]
        fn varint(&mut self) -> Result<usize, String> {
            let rest = self.a.get(self.i..).ok_or_else(|| trunc())?;
            let (v, n) = get_varint(rest).ok_or_else(|| trunc())?;
            self.i += n;
            Ok(v as usize)
        }
    }

    fn trunc() -> String {
        String::from("changeset: truncated input")
    }

    fn parse_val(c: &mut Cur) -> Result<Val, String> {
        let t = c.byte()?;
        match t {
            T_UNDEF => Ok(Val::Undef),
            T_NULL => Ok(Val::Null),
            T_INT => {
                let b = c.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                Ok(Val::Int(i64::from_be_bytes(a)))
            }
            T_FLOAT => {
                let b = c.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                Ok(Val::Float(u64::from_be_bytes(a)))
            }
            T_TEXT => {
                let n = c.varint()?;
                Ok(Val::Text(c.take(n)?.to_vec()))
            }
            T_BLOB => {
                let n = c.varint()?;
                Ok(Val::Blob(c.take(n)?.to_vec()))
            }
            other => Err(format!("changeset: bad value type byte {other}")),
        }
    }

    fn parse_record(c: &mut Cur, n_col: usize) -> Result<Vec<Val>, String> {
        let mut r = Vec::with_capacity(n_col);
        for _ in 0..n_col {
            r.push(parse_val(c)?);
        }
        Ok(r)
    }

    /// Parse a changeset blob into its table blocks, preserving stream order.
    pub fn parse(a: &[u8]) -> Result<Vec<Table>, String> {
        let mut c = Cur { a, i: 0 };
        let mut tables: Vec<Table> = Vec::new();
        let mut cur: Option<usize> = None;

        while c.i < a.len() {
            let tag = c.peek().ok_or_else(trunc)?;
            match tag {
                b'T' => {
                    c.i += 1;
                    let n_col = c.varint()?;
                    if n_col == 0 || n_col > 65536 {
                        return Err(format!("changeset: implausible column count {n_col}"));
                    }
                    let pk = c.take(n_col)?.to_vec();
                    // Table name: nul-terminated UTF-8.
                    let start = c.i;
                    while c.peek().map_or(false, |b| b != 0) {
                        c.i += 1;
                    }
                    if c.peek() != Some(0) {
                        return Err(String::from("changeset: unterminated table name"));
                    }
                    let name = a[start..c.i].to_vec();
                    c.i += 1; // consume the nul
                    tables.push(Table {
                        n_col,
                        pk,
                        name,
                        changes: Vec::new(),
                    });
                    cur = Some(tables.len() - 1);
                }
                OP_INSERT | OP_DELETE | OP_UPDATE => {
                    let ti = cur.ok_or_else(|| {
                        String::from("changeset: change record before any table header")
                    })?;
                    let n_col = tables[ti].n_col;
                    c.i += 1; // op byte
                    let indirect = c.byte()?;
                    let (old, new) = match tag {
                        OP_INSERT => (Vec::new(), parse_record(&mut c, n_col)?),
                        OP_DELETE => (parse_record(&mut c, n_col)?, Vec::new()),
                        _ => {
                            let o = parse_record(&mut c, n_col)?;
                            let n = parse_record(&mut c, n_col)?;
                            (o, n)
                        }
                    };
                    tables[ti].changes.push(Change {
                        op: tag,
                        indirect,
                        old,
                        new,
                    });
                }
                other => return Err(format!("changeset: unexpected tag byte {other}")),
            }
        }
        Ok(tables)
    }

    fn append_header(out: &mut Vec<u8>, t: &Table) {
        out.push(b'T');
        put_varint(t.n_col as u64, out);
        out.extend_from_slice(&t.pk);
        out.extend_from_slice(&t.name);
        out.push(0);
    }

    fn append_change(out: &mut Vec<u8>, ch: &Change) {
        out.push(ch.op);
        out.push(ch.indirect);
        match ch.op {
            OP_INSERT => ser_record(&ch.new, out),
            OP_DELETE => ser_record(&ch.old, out),
            _ => {
                ser_record(&ch.old, out);
                ser_record(&ch.new, out);
            }
        }
    }

    /// Serialize table blocks back to a changeset blob. `serialize(parse(x))`
    /// is byte-identical to `x` for any well-formed `x`.
    pub fn serialize(tables: &[Table]) -> Vec<u8> {
        let mut out = Vec::new();
        for t in tables {
            append_header(&mut out, t);
            for ch in &t.changes {
                append_change(&mut out, ch);
            }
        }
        out
    }

    // ---- invert (byte-exact with sessionChangesetInvert) ------------------

    /// Invert a changeset: INSERT↔DELETE, and each UPDATE has old/new swapped
    /// with PK fix-up. Byte-for-byte identical to `sqlite3changeset_invert`.
    pub fn invert(a: &[u8]) -> Result<Vec<u8>, String> {
        let tables = parse(a)?;
        let mut out = Vec::new();
        for t in &tables {
            append_header(&mut out, t);
            for ch in &t.changes {
                match ch.op {
                    OP_INSERT => {
                        out.push(OP_DELETE);
                        out.push(ch.indirect);
                        ser_record(&ch.new, &mut out);
                    }
                    OP_DELETE => {
                        out.push(OP_INSERT);
                        out.push(ch.indirect);
                        ser_record(&ch.old, &mut out);
                    }
                    OP_UPDATE => {
                        out.push(OP_UPDATE);
                        out.push(ch.indirect);
                        // New old.*: PK cols from the original old.*, other
                        // cols from the original new.*.
                        for i in 0..t.n_col {
                            if t.pk[i] != 0 {
                                ser_val(&ch.old[i], &mut out);
                            } else {
                                ser_val(&ch.new[i], &mut out);
                            }
                        }
                        // New new.*: original old.* values, PK cols undefined.
                        for i in 0..t.n_col {
                            if t.pk[i] != 0 {
                                ser_val(&Val::Undef, &mut out);
                            } else {
                                ser_val(&ch.old[i], &mut out);
                            }
                        }
                    }
                    other => return Err(format!("changeset: bad op {other}")),
                }
            }
        }
        Ok(out)
    }

    // ---- count / tables / decode ------------------------------------------

    /// Number of change records in the changeset.
    pub fn count(a: &[u8]) -> Result<i64, String> {
        let tables = parse(a)?;
        Ok(tables.iter().map(|t| t.changes.len() as i64).sum())
    }

    fn json_push_name(out: &mut String, name: &[u8]) {
        // Match the old embed path: only `"` and `\` are escaped in names.
        let s = String::from_utf8_lossy(name);
        for ch in s.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
    }

    /// JSON array of the distinct table names carrying at least one change, in
    /// first-seen order. Matches the old `changeset_tables` output.
    pub fn tables_json(a: &[u8]) -> Result<String, String> {
        let tables = parse(a)?;
        let mut seen: Vec<&[u8]> = Vec::new();
        for t in &tables {
            if !t.changes.is_empty() && !seen.iter().any(|n| *n == t.name.as_slice()) {
                seen.push(&t.name);
            }
        }
        let mut out = String::from("[");
        for (i, n) in seen.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            json_push_name(&mut out, n);
            out.push('"');
        }
        out.push(']');
        Ok(out)
    }

    fn op_name(op: u8) -> &'static str {
        match op {
            OP_INSERT => "INSERT",
            OP_UPDATE => "UPDATE",
            OP_DELETE => "DELETE",
            _ => "UNKNOWN",
        }
    }

    fn val_json(v: &Val, out: &mut String) {
        match v {
            // Both "undefined" and NULL render as JSON null, matching the
            // embed path (sqlite3changeset_old returns NULL for absent cols).
            Val::Undef | Val::Null => out.push_str("null"),
            Val::Int(i) => out.push_str(&format!("{i}")),
            Val::Float(bits) => {
                let f = f64::from_bits(*bits);
                if !f.is_finite() {
                    out.push_str("null");
                } else {
                    out.push_str(&format!("{f}"));
                }
            }
            Val::Text(b) => {
                let s = String::from_utf8_lossy(b);
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Val::Blob(b) => {
                if b.is_empty() {
                    out.push_str("\"\"");
                    return;
                }
                out.push('"');
                for byte in b {
                    out.push_str(&format!("{byte:02x}"));
                }
                out.push('"');
            }
        }
    }

    fn json_record(out: &mut String, key: &str, r: &[Val]) {
        out.push_str(key);
        for (i, v) in r.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            val_json(v, out);
        }
        out.push(']');
    }

    /// JSON array of every change record: `[{"table","op","indirect","old"?,"new"?}]`.
    /// Matches the old `changeset_decode` output.
    pub fn decode_json(a: &[u8]) -> Result<String, String> {
        let tables = parse(a)?;
        let mut out = String::from("[");
        let mut first = true;
        for t in &tables {
            for ch in &t.changes {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str("{\"table\":\"");
                json_push_name(&mut out, &t.name);
                out.push_str("\",\"op\":\"");
                out.push_str(op_name(ch.op));
                out.push_str("\",\"indirect\":");
                out.push_str(if ch.indirect != 0 { "true" } else { "false" });
                if ch.op == OP_DELETE || ch.op == OP_UPDATE {
                    json_record(&mut out, ",\"old\":[", &ch.old);
                }
                if ch.op == OP_INSERT || ch.op == OP_UPDATE {
                    json_record(&mut out, ",\"new\":[", &ch.new);
                }
                out.push('}');
            }
        }
        out.push(']');
        Ok(out)
    }

    // ---- concat (semantic-equivalent change-group merge) ------------------

    /// `mergeValue`: pick the second record's value if present and *defined*
    /// (non-`Undef`), else the first's. `r2 = None` models a missing record.
    #[inline]
    fn mval<'a>(r1: &'a [Val], r2: Option<&'a [Val]>, i: usize) -> &'a Val {
        if let Some(r2) = r2 {
            if r2[i].is_defined() {
                return &r2[i];
            }
        }
        &r1[i]
    }

    /// `sessionMergeRecord`: for each column, the right value if defined, else
    /// the left. Both records are full (`n_col`).
    fn merge_record(left: &[Val], right: &[Val], n_col: usize) -> Vec<Val> {
        let mut out = Vec::with_capacity(n_col);
        for i in 0..n_col {
            if right[i].is_defined() {
                out.push(right[i].clone());
            } else {
                out.push(left[i].clone());
            }
        }
        out
    }

    /// `sessionMergeUpdate`: combine two updates (old1/new1 then old2/new2) on
    /// the same row into a single UPDATE `(old, new)`. Returns `None` when no
    /// non-PK column actually changes (the merged update is a no-op).
    fn merge_update(
        pk: &[u8],
        old1: &[Val],
        old2: Option<&[Val]>,
        new1: &[Val],
        new2: Option<&[Val]>,
    ) -> Option<(Vec<Val>, Vec<Val>)> {
        let n = pk.len();
        let mut out_old = Vec::with_capacity(n);
        let mut required = false;
        for i in 0..n {
            let a_old = mval(old1, old2, i);
            let a_new = mval(new1, new2, i);
            if pk[i] != 0 || a_old != a_new {
                if pk[i] == 0 {
                    required = true;
                }
                out_old.push(a_old.clone());
            } else {
                out_old.push(Val::Undef);
            }
        }
        if !required {
            return None;
        }
        let mut out_new = Vec::with_capacity(n);
        for i in 0..n {
            let a_old = mval(old1, old2, i);
            let a_new = mval(new1, new2, i);
            if pk[i] != 0 || a_old == a_new {
                out_new.push(Val::Undef);
            } else {
                out_new.push(a_new.clone());
            }
        }
        Some((out_old, out_new))
    }

    /// Combine an existing change with a later one on the same row, per the
    /// `sessionChangeMerge` op1×op2 table. `None` => the row nets out to no
    /// change (drop it).
    fn merge_change(
        existing: &Change,
        next: &Change,
        pk: &[u8],
        n_col: usize,
    ) -> Result<Option<Change>, String> {
        let op1 = existing.op;
        let op2 = next.op;
        // pNew->bIndirect = (bIndirect && pExist->bIndirect) for merged rows.
        let indirect = if existing.indirect != 0 && next.indirect != 0 { 1 } else { 0 };

        // Unsupported sequences: keep the existing change, discard the new one.
        if (op1 == OP_INSERT && op2 == OP_INSERT)
            || (op1 == OP_UPDATE && op2 == OP_INSERT)
            || (op1 == OP_DELETE && op2 == OP_UPDATE)
            || (op1 == OP_DELETE && op2 == OP_DELETE)
        {
            return Ok(Some(existing.clone()));
        }

        // INSERT then DELETE -> the row never existed: no change.
        if op1 == OP_INSERT && op2 == OP_DELETE {
            return Ok(None);
        }

        if op1 == OP_INSERT {
            // INSERT + UPDATE -> INSERT with the update folded in.
            let merged = merge_record(&existing.new, &next.new, n_col);
            return Ok(Some(Change {
                op: OP_INSERT,
                indirect,
                old: alloc::vec::Vec::new(),
                new: merged,
            }));
        }

        if op1 == OP_DELETE {
            // DELETE + INSERT -> UPDATE (delete.old -> insert.new).
            return Ok(
                merge_update(pk, &existing.old, None, &next.new, None).map(|(old, new)| Change {
                    op: OP_UPDATE,
                    indirect,
                    old,
                    new,
                }),
            );
        }

        if op2 == OP_UPDATE {
            // UPDATE + UPDATE. mergeValue precedence (see sessionMergeUpdate
            // call site): old1=next.old, old2=existing.old (earliest old wins),
            // new1=existing.new, new2=next.new (latest new wins).
            return Ok(merge_update(
                pk,
                &next.old,
                Some(&existing.old),
                &existing.new,
                Some(&next.new),
            )
            .map(|(old, new)| Change {
                op: OP_UPDATE,
                indirect,
                old,
                new,
            }));
        }

        // UPDATE + DELETE -> DELETE. old = mergeRecord(delete.old, update.old).
        let merged = merge_record(&next.old, &existing.old, n_col);
        Ok(Some(Change {
            op: OP_DELETE,
            indirect,
            old: merged,
            new: alloc::vec::Vec::new(),
        }))
    }

    /// The PK-value key identifying the row a change touches (the concatenated
    /// serialized PK column values). INSERT reads its PK from the new record;
    /// DELETE/UPDATE from the old record.
    fn pk_key(ch: &Change, pk: &[u8]) -> Vec<u8> {
        let rec = if ch.op == OP_INSERT { &ch.new } else { &ch.old };
        let mut k = Vec::new();
        for (i, is_pk) in pk.iter().enumerate() {
            if *is_pk != 0 {
                if let Some(v) = rec.get(i) {
                    ser_val(v, &mut k);
                }
            }
        }
        k
    }

    struct GTable {
        n_col: usize,
        pk: Vec<u8>,
        name: Vec<u8>,
        order: Vec<Vec<u8>>,          // PK keys in first-seen order
        by_key: BTreeMap<Vec<u8>, usize>,
        slots: Vec<Option<Change>>,   // parallel to `order`; None = removed
    }

    /// Concatenate two changesets: a valid changeset that, applied to a
    /// database, has the same effect as applying `a` then `b`. Byte layout is
    /// deterministic (first-seen table, PK-insertion order) but not guaranteed
    /// identical to `sqlite3changeset_concat`.
    pub fn concat(a: &[u8], b: &[u8]) -> Result<Vec<u8>, String> {
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut tabs: BTreeMap<Vec<u8>, GTable> = BTreeMap::new();

        for src in [a, b] {
            for t in parse(src)? {
                let gt = match tabs.get_mut(&t.name) {
                    Some(gt) => {
                        if gt.n_col != t.n_col || gt.pk != t.pk {
                            return Err(String::from(
                                "changeset_concat: incompatible table definitions",
                            ));
                        }
                        gt
                    }
                    None => {
                        names.push(t.name.clone());
                        tabs.entry(t.name.clone()).or_insert(GTable {
                            n_col: t.n_col,
                            pk: t.pk.clone(),
                            name: t.name.clone(),
                            order: Vec::new(),
                            by_key: BTreeMap::new(),
                            slots: Vec::new(),
                        })
                    }
                };

                for ch in t.changes {
                    let key = pk_key(&ch, &gt.pk);
                    match gt.by_key.get(&key).copied() {
                        Some(pos) if gt.slots[pos].is_some() => {
                            let existing = gt.slots[pos].take().unwrap();
                            let merged = merge_change(&existing, &ch, &gt.pk, gt.n_col)?;
                            gt.slots[pos] = merged;
                        }
                        Some(pos) => {
                            // Slot exists but was emptied (a prior INSERT+DELETE).
                            // Treat as fresh.
                            gt.slots[pos] = Some(ch);
                        }
                        None => {
                            let pos = gt.order.len();
                            gt.order.push(key.clone());
                            gt.by_key.insert(key, pos);
                            gt.slots.push(Some(ch));
                        }
                    }
                }
            }
        }

        let mut out = Vec::new();
        for name in &names {
            let gt = tabs.get(name).unwrap();
            let live: Vec<&Change> = gt.slots.iter().flatten().collect();
            if live.is_empty() {
                continue;
            }
            append_header(
                &mut out,
                &Table {
                    n_col: gt.n_col,
                    pk: gt.pk.clone(),
                    name: gt.name.clone(),
                    changes: Vec::new(),
                },
            );
            for ch in live {
                append_change(&mut out, ch);
            }
        }
        Ok(out)
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "changeset";
    version = env!("CARGO_PKG_VERSION");

    scalar changeset_invert(blob) -> blob [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "changeset_invert")?;
        Ok(NeutralValue::Blob(codec::invert(&b)?))
    };

    scalar changeset_concat(blob, blob) -> blob [propagate, deterministic] = |args| {
        let a = args.arg_blob(0, "changeset_concat")?;
        let b = args.arg_blob(1, "changeset_concat")?;
        Ok(NeutralValue::Blob(codec::concat(&a, &b)?))
    };

    scalar changeset_count(blob) -> int64 [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "changeset_count")?;
        Ok(NeutralValue::Int64(codec::count(&b)?))
    };

    scalar changeset_tables(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "changeset_tables")?;
        Ok(NeutralValue::Text(codec::tables_json(&b)?))
    };

    scalar changeset_decode(blob) -> text [propagate, deterministic] = |args| {
        let b = args.arg_blob(0, "changeset_decode")?;
        Ok(NeutralValue::Text(codec::decode_json(&b)?))
    };
}

#[cfg(test)]
mod tests;
