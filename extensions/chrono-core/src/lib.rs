//! Neutral core for the `chrono` extension — the cross-dialect datetime
//! scalars, written ONCE. The per-DB shim is generated from the
//! [`declare!`](datalink_extcore::declare) table.
//!
//! # Scope: only the dialect spellings DuckDB does NOT already provide
//!
//! `chrono` originated in sqlink because SQLite ships almost no powered
//! datetime functions. DuckDB is the opposite: it ships the whole
//! calendar surface as BUILTINS under its own names —
//! `year`/`month`/`day`/`dayofmonth`/`dayofyear`/`dayofweek`/`week`/
//! `weekofyear`/`quarter`/`hour`/`minute`/`second`/`monthname`/`dayname`/
//! `last_day`/`date_part`/`date_diff`/`date_sub`/`date_trunc`/`epoch`/
//! `epoch_ms`/`epoch_us`/`make_date`/`make_time`/`make_timestamp`/
//! `to_days`/`to_seconds`/`to_timestamp`/`age`/`now`/`current_date`/
//! `time_bucket`. Re-registering any of those (same name + arity) would
//! collide with the builtin (DuckDB rejects the overlap), so they are
//! deliberately NOT declared here — they are the DB's own builtins.
//! `date_add`/`datediff` are also skipped: the bare NAME is a DuckDB
//! builtin (at a different arity) and there is no proven safe overload
//! path. The reserved-keyword spellings (`extract`, `timestamp`, `now`,
//! `localtime`, `localtimestamp`, `current_time`, `current_timestamp`)
//! are DB syntax, not registrable function names.
//!
//! What remains — and is what ducklink GAINS — are the MySQL / BigQuery /
//! Snowflake dialect spellings + the tz-convert / duration / business-day
//! surface that DuckDB has no builtin for. The carrier is canonical
//! RFC 3339 ISO 8601 UTC TEXT (`YYYY-MM-DDTHH:MM:SSZ`), byte-identical to
//! sqlink's `chrono`, so a future `sqlite_shim!` over this core
//! reproduces sqlink's behaviour for these names.

extern crate alloc;

use datalink_extcore::NeutralValue;

/// DB-agnostic datetime implementations, lifted byte-for-byte from
/// sqlink's `chrono` extension. Native-testable; the generated shim is a
/// thin dispatch wrapper.
pub mod logic {
    use alloc::format;
    use alloc::string::{String, ToString};
    use chrono::{
        DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc, Weekday,
    };
    use chrono_tz::Tz;
    use core::str::FromStr;

    /// Parse an input string into a UTC DateTime (auto-detect or explicit
    /// format). Naive inputs (no tz) are interpreted as UTC.
    pub fn parse_to_utc(input: &str, fmt: Option<&str>) -> Result<DateTime<Utc>, String> {
        if let Some(f) = fmt {
            if let Ok(dt) = DateTime::parse_from_str(input, f) {
                return Ok(dt.with_timezone(&Utc));
            }
            if let Ok(ndt) = NaiveDateTime::parse_from_str(input, f) {
                return Ok(Utc.from_utc_datetime(&ndt));
            }
            if let Ok(d) = NaiveDate::parse_from_str(input, f) {
                return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()));
            }
            return Err(format!("date: parse {input:?} with format {f:?} failed"));
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
            return Ok(dt.with_timezone(&Utc));
        }
        if let Ok(dt) = DateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S%z") {
            return Ok(dt.with_timezone(&Utc));
        }
        if let Ok(ndt) = NaiveDateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S") {
            return Ok(Utc.from_utc_datetime(&ndt));
        }
        if let Ok(ndt) = NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M:%S") {
            return Ok(Utc.from_utc_datetime(&ndt));
        }
        if let Ok(d) = NaiveDate::parse_from_str(input, "%Y-%m-%d") {
            return Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()));
        }
        Err(format!("date: unrecognized input {input:?}"))
    }

    /// Canonical "ISO 8601 UTC, second precision, trailing Z".
    pub fn fmt_utc(dt: DateTime<Utc>) -> String {
        dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// ISO 8601 second precision with the tz's UTC offset.
    pub fn fmt_tz(dt: DateTime<Tz>) -> String {
        dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string()
    }

    pub fn parse_tz(name: &str) -> Result<Tz, String> {
        Tz::from_str(name).map_err(|e| format!("unknown timezone {name:?}: {e}"))
    }

    pub fn date_parse(s: &str, format: Option<&str>) -> Result<String, String> {
        Ok(fmt_utc(parse_to_utc(s, format)?))
    }

    pub fn date_format(s: &str, format: &str) -> Result<String, String> {
        let dt = parse_to_utc(s, None)?;
        Ok(dt.format(format).to_string())
    }

    pub fn date_add(s: &str, amount: i64, unit: &str) -> Result<String, String> {
        let dt = parse_to_utc(s, None)?;
        let unit_lc = unit.to_ascii_lowercase();
        let out = match unit_lc.as_str() {
            "year" | "years" => add_months(dt, amount.checked_mul(12).ok_or("date_add: overflow")?)?,
            "month" | "months" => add_months(dt, amount)?,
            "week" | "weeks" => dt
                .checked_add_signed(Duration::weeks(amount))
                .ok_or_else(|| "date_add: overflow".to_string())?,
            "day" | "days" => dt
                .checked_add_signed(Duration::days(amount))
                .ok_or_else(|| "date_add: overflow".to_string())?,
            "hour" | "hours" => dt
                .checked_add_signed(Duration::hours(amount))
                .ok_or_else(|| "date_add: overflow".to_string())?,
            "min" | "mins" | "minute" | "minutes" => dt
                .checked_add_signed(Duration::minutes(amount))
                .ok_or_else(|| "date_add: overflow".to_string())?,
            "sec" | "secs" | "second" | "seconds" => dt
                .checked_add_signed(Duration::seconds(amount))
                .ok_or_else(|| "date_add: overflow".to_string())?,
            other => return Err(format!("date_add: unknown unit {other:?}")),
        };
        Ok(fmt_utc(out))
    }

    fn add_months(dt: DateTime<Utc>, months: i64) -> Result<DateTime<Utc>, String> {
        let total = dt.year() as i64 * 12 + dt.month0() as i64 + months;
        if total < 0 {
            return Err("date_add: month delta crosses year zero".to_string());
        }
        let new_year = (total / 12) as i32;
        let new_month0 = (total % 12) as u32;
        let new_month = new_month0 + 1;
        let max_day = days_in_month(new_year, new_month);
        let new_day = dt.day().min(max_day);
        let d = NaiveDate::from_ymd_opt(new_year, new_month, new_day)
            .ok_or_else(|| "date_add: invalid ymd".to_string())?;
        let ndt = d
            .and_hms_opt(dt.hour(), dt.minute(), dt.second())
            .ok_or_else(|| "date_add: invalid hms".to_string())?;
        Ok(Utc.from_utc_datetime(&ndt))
    }

    fn days_in_month(year: i32, month: u32) -> u32 {
        let (next_y, next_m) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        let first_next = NaiveDate::from_ymd_opt(next_y, next_m, 1).unwrap();
        let last_this = first_next.pred_opt().unwrap();
        last_this.day()
    }

    pub fn date_diff(a: &str, b: &str, unit: &str) -> Result<i64, String> {
        let da = parse_to_utc(a, None)?;
        let db = parse_to_utc(b, None)?;
        let delta = da.signed_duration_since(db);
        let unit_lc = unit.to_ascii_lowercase();
        let n = match unit_lc.as_str() {
            "year" | "years" => calendar_months_between(db, da) / 12,
            "month" | "months" => calendar_months_between(db, da),
            "week" | "weeks" => delta.num_weeks(),
            "day" | "days" => delta.num_days(),
            "hour" | "hours" => delta.num_hours(),
            "min" | "mins" | "minute" | "minutes" => delta.num_minutes(),
            "sec" | "secs" | "second" | "seconds" => delta.num_seconds(),
            other => return Err(format!("date_diff: unknown unit {other:?}")),
        };
        Ok(n)
    }

    fn calendar_months_between(from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
        let mut months = (to.year() as i64 - from.year() as i64) * 12
            + (to.month() as i64 - from.month() as i64);
        if to.day() < from.day() && months > 0 {
            months -= 1;
        } else if to.day() > from.day() && months < 0 {
            months += 1;
        }
        months
    }

    pub fn date_tz_convert(s: &str, from_tz: &str, to_tz: &str) -> Result<String, String> {
        let from = parse_tz(from_tz)?;
        let to = parse_tz(to_tz)?;
        let dt_in_to: DateTime<Tz> = if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            dt.with_timezone(&to)
        } else if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
            from.from_local_datetime(&ndt)
                .single()
                .ok_or_else(|| format!("date_tz_convert: ambiguous/invalid {s:?} in {from_tz}"))?
                .with_timezone(&to)
        } else if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
            from.from_local_datetime(&ndt)
                .single()
                .ok_or_else(|| format!("date_tz_convert: ambiguous/invalid {s:?} in {from_tz}"))?
                .with_timezone(&to)
        } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            let ndt = d.and_hms_opt(0, 0, 0).unwrap();
            from.from_local_datetime(&ndt)
                .single()
                .ok_or_else(|| format!("date_tz_convert: ambiguous/invalid {s:?} in {from_tz}"))?
                .with_timezone(&to)
        } else {
            return Err(format!("date_tz_convert: unrecognized input {s:?}"));
        };
        Ok(fmt_tz(dt_in_to))
    }

    pub fn date_now_tz(tz: &str) -> Result<String, String> {
        let zone = parse_tz(tz)?;
        let now = Utc::now().with_timezone(&zone);
        Ok(fmt_tz(now))
    }

    pub fn date_is_business_day(s: &str) -> Result<i64, String> {
        let dt = parse_to_utc(s, None)?;
        Ok(matches!(
            dt.weekday(),
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
        ) as i64)
    }

    pub fn date_business_days_between(a: &str, b: &str) -> Result<i64, String> {
        let da = parse_to_utc(a, None)?.date_naive();
        let db = parse_to_utc(b, None)?.date_naive();
        let (start, end, sign) = if da <= db { (da, db, 1) } else { (db, da, -1) };
        let mut count: i64 = 0;
        let mut cur = start;
        while cur < end {
            if matches!(
                cur.weekday(),
                Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
            ) {
                count += 1;
            }
            cur = cur.succ_opt().unwrap();
        }
        Ok(count * sign)
    }

    pub fn date_iso_week(s: &str) -> Result<i64, String> {
        Ok(parse_to_utc(s, None)?.iso_week().week() as i64)
    }

    pub fn date_iso_year(s: &str) -> Result<i64, String> {
        Ok(parse_to_utc(s, None)?.iso_week().year() as i64)
    }

    pub fn duration_parse(s: &str) -> Result<i64, String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("duration_parse: empty input".to_string());
        }
        if let Ok(n) = trimmed.parse::<i64>() {
            return Ok(n);
        }
        if let Some(stripped) = trimmed.strip_prefix('P') {
            return parse_iso_duration(stripped);
        }
        parse_compact_duration(trimmed)
    }

    fn parse_iso_duration(rest: &str) -> Result<i64, String> {
        let (date_part, time_part) = match rest.find('T') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let mut secs: i64 = 0;
        secs += parse_iso_segment(date_part, &[('Y', 365 * 86400), ('M', 30 * 86400), ('D', 86400)])?;
        secs += parse_iso_segment(time_part, &[('H', 3600), ('M', 60), ('S', 1)])?;
        Ok(secs)
    }

    fn parse_iso_segment(seg: &str, units: &[(char, i64)]) -> Result<i64, String> {
        let mut total: i64 = 0;
        let mut buf = String::new();
        for c in seg.chars() {
            if c.is_ascii_digit() || c == '.' || c == '-' {
                buf.push(c);
                continue;
            }
            let multiplier = units
                .iter()
                .find(|(u, _)| *u == c)
                .map(|(_, m)| *m)
                .ok_or_else(|| format!("duration_parse: unexpected ISO unit {c:?}"))?;
            let n: f64 = buf
                .parse()
                .map_err(|e| format!("duration_parse: bad number {buf:?}: {e}"))?;
            total += (n * multiplier as f64) as i64;
            buf.clear();
        }
        if !buf.is_empty() {
            return Err("duration_parse: ISO segment trailing digits without unit".to_string());
        }
        Ok(total)
    }

    fn parse_compact_duration(s: &str) -> Result<i64, String> {
        let mut total: i64 = 0;
        let mut num = String::new();
        let mut unit = String::new();
        let mut chars = s.chars().peekable();
        loop {
            while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
                chars.next();
            }
            num.clear();
            let mut saw_digit = false;
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() || c == '.' || (c == '-' && num.is_empty()) {
                    num.push(c);
                    chars.next();
                    if c.is_ascii_digit() {
                        saw_digit = true;
                    }
                } else {
                    break;
                }
            }
            if !saw_digit {
                if chars.peek().is_none() {
                    break;
                }
                return Err(format!(
                    "duration_parse: expected number at {:?}",
                    chars.collect::<String>()
                ));
            }
            unit.clear();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphabetic() {
                    unit.push(c.to_ascii_lowercase());
                    chars.next();
                } else {
                    break;
                }
            }
            let mult = match unit.as_str() {
                "y" | "year" | "years" => 365 * 86400,
                "mo" | "month" | "months" => 30 * 86400,
                "w" | "week" | "weeks" => 7 * 86400,
                "d" | "day" | "days" => 86400,
                "h" | "hour" | "hours" => 3600,
                "m" | "min" | "mins" | "minute" | "minutes" => 60,
                "s" | "sec" | "secs" | "second" | "seconds" => 1,
                "" => return Err(format!("duration_parse: missing unit after {num:?}")),
                other => return Err(format!("duration_parse: unknown unit {other:?}")),
            };
            let n: f64 = num
                .parse()
                .map_err(|e| format!("duration_parse: bad number {num:?}: {e}"))?;
            total += (n * mult as f64) as i64;
        }
        Ok(total)
    }

    pub fn duration_format(seconds: i64, precision: Option<u32>) -> String {
        if seconds == 0 {
            return "0s".to_string();
        }
        let p = precision.unwrap_or(4).max(1);
        let sign = if seconds < 0 { "-" } else { "" };
        let mut n = seconds.unsigned_abs();
        let days = n / 86_400;
        n %= 86_400;
        let hours = n / 3_600;
        n %= 3_600;
        let minutes = n / 60;
        let secs = n % 60;
        let parts: [(u64, &str); 4] = [(days, "d"), (hours, "h"), (minutes, "m"), (secs, "s")];
        let mut out = String::new();
        out.push_str(sign);
        let mut emitted: u32 = 0;
        for (n, suf) in parts {
            if n == 0 {
                continue;
            }
            if !out.is_empty() && out != sign {
                out.push(' ');
            }
            out.push_str(&format!("{n}{suf}"));
            emitted += 1;
            if emitted >= p {
                break;
            }
        }
        if out == sign {
            out.push_str("0s");
        }
        out
    }

    // ── Cross-DB portability scalars ──

    pub fn now_utc() -> String {
        fmt_utc(Utc::now())
    }

    pub fn from_unixtime(epoch: i64) -> Result<String, String> {
        let dt = DateTime::<Utc>::from_timestamp(epoch, 0)
            .ok_or_else(|| format!("from_unixtime: out of range {epoch}"))?;
        Ok(fmt_utc(dt))
    }

    pub fn datediff(d1: &str, d2: &str) -> Result<i64, String> {
        date_diff(d1, d2, "day")
    }

    pub fn timestampdiff(unit: &str, t1: &str, t2: &str) -> Result<i64, String> {
        date_diff(t2, t1, unit)
    }

    pub fn timestampadd(unit: &str, n: i64, t: &str) -> Result<String, String> {
        date_add(t, n, unit)
    }

    pub fn adddate(s: &str, n: i64) -> Result<String, String> {
        date_add(s, n, "day")
    }
    pub fn subdate(s: &str, n: i64) -> Result<String, String> {
        date_add(s, -n, "day")
    }

    pub fn date_sub(s: &str, n: i64, unit: &str) -> Result<String, String> {
        date_add(s, -n, unit)
    }

    pub fn age(t1: &str, t2: &str) -> Result<String, String> {
        let a = parse_to_utc(t1, None)?;
        let b = parse_to_utc(t2, None)?;
        let secs = a.signed_duration_since(b).num_seconds();
        Ok(duration_format(secs, None))
    }

    pub fn from_days(n: i64) -> Result<String, String> {
        let base = NaiveDate::from_ymd_opt(0, 1, 1).ok_or("from_days: bad epoch")?;
        let d = base
            .checked_add_signed(Duration::days(n))
            .ok_or_else(|| format!("from_days: overflow at {n}"))?;
        Ok(Utc
            .from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string())
    }

    pub fn make_date(year: i64, month: i64, day: i64) -> Result<String, String> {
        let d = NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
            .ok_or_else(|| format!("makedate: invalid {year}-{month}-{day}"))?;
        Ok(Utc
            .from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string())
    }

    pub fn make_time(h: i64, m: i64, s: i64) -> Result<String, String> {
        if !(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0..=59).contains(&s) {
            return Err(format!("maketime: out of range {h}:{m}:{s}"));
        }
        Ok(format!("{h:02}:{m:02}:{s:02}"))
    }

    pub fn epoch(s: &str) -> Result<f64, String> {
        Ok(parse_to_utc(s, None)?.timestamp() as f64)
    }
    pub fn epoch_ms(s: &str) -> Result<i64, String> {
        Ok(parse_to_utc(s, None)?.timestamp_millis())
    }
    pub fn epoch_us(s: &str) -> Result<i64, String> {
        Ok(parse_to_utc(s, None)?.timestamp_micros())
    }

    pub fn date_trunc(unit: &str, s: &str) -> Result<String, String> {
        let dt = parse_to_utc(s, None)?;
        let u = unit.to_ascii_lowercase();
        let (y, mo, d, h, mi, se) = (
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
        );
        let nd = match u.as_str() {
            "year" | "years" => NaiveDate::from_ymd_opt(y, 1, 1),
            "quarter" => NaiveDate::from_ymd_opt(y, ((mo - 1) / 3) * 3 + 1, 1),
            "month" | "months" => NaiveDate::from_ymd_opt(y, mo, 1),
            "week" | "weeks" => {
                let weekday = dt.weekday().num_days_from_monday() as i64;
                NaiveDate::from_ymd_opt(y, mo, d)
                    .and_then(|d| d.checked_sub_signed(Duration::days(weekday)))
            }
            "day" | "days" => NaiveDate::from_ymd_opt(y, mo, d),
            _ => None,
        };
        if let Some(d0) = nd {
            let (hh, mm, ss) = match u.as_str() {
                "hour" | "hours" => (h, 0, 0),
                "minute" | "minutes" => (h, mi, 0),
                "second" | "seconds" => (h, mi, se),
                _ => (0, 0, 0),
            };
            let ndt = d0
                .and_hms_opt(hh, mm, ss)
                .unwrap_or(d0.and_hms_opt(0, 0, 0).unwrap());
            return Ok(Utc
                .from_utc_datetime(&ndt)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string());
        }
        let (hh, mm, ss) = match u.as_str() {
            "hour" | "hours" => (h, 0, 0),
            "minute" | "minutes" => (h, mi, 0),
            "second" | "seconds" => (h, mi, se),
            _ => return Err(format!("date_trunc: unknown unit {unit:?}")),
        };
        Ok(NaiveDate::from_ymd_opt(y, mo, d)
            .and_then(|d| d.and_hms_opt(hh, mm, ss))
            .map(|ndt| {
                Utc.from_utc_datetime(&ndt)
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string()
            })
            .unwrap_or_default())
    }

    pub fn time_bucket(seconds: i64, s: &str) -> Result<String, String> {
        if seconds <= 0 {
            return Err("time_bucket: positive interval required".to_string());
        }
        let dt = parse_to_utc(s, None)?;
        let epoch = dt.timestamp();
        let bucket = (epoch / seconds) * seconds;
        Ok(DateTime::<Utc>::from_timestamp(bucket, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default())
    }
}

datalink_extcore::declare! {
    core = Core;
    extension = "chrono";
    version = env!("CARGO_PKG_VERSION");

    // ── Canonical parse / format (DuckDB uses strptime/strftime) ──
    scalar date_parse(text) -> text [propagate, deterministic] = |a| {
        logic::date_parse(&a.arg_text(0, "date_parse")?, None).map(NeutralValue::Text)
    };
    scalar date_parse(text, text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "date_parse")?;
        let f = a.arg_text(1, "date_parse")?;
        logic::date_parse(&s, Some(&f)).map(NeutralValue::Text)
    };
    scalar date_format(text, text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "date_format")?;
        let f = a.arg_text(1, "date_format")?;
        logic::date_format(&s, &f).map(NeutralValue::Text)
    };

    // ── tz convert + now-in-tz (DuckDB: AT TIME ZONE syntax) ──
    scalar date_tz_convert(text, text, text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "date_tz_convert")?;
        let f = a.arg_text(1, "date_tz_convert")?;
        let t = a.arg_text(2, "date_tz_convert")?;
        logic::date_tz_convert(&s, &f, &t).map(NeutralValue::Text)
    };
    scalar date_now_tz(text) -> text [propagate, nondeterministic] = |a| {
        logic::date_now_tz(&a.arg_text(0, "date_now_tz")?).map(NeutralValue::Text)
    };

    // ── business-day math (no DuckDB builtin) ──
    scalar date_is_business_day(text) -> int64 [propagate, deterministic] = |a| {
        logic::date_is_business_day(&a.arg_text(0, "date_is_business_day")?).map(NeutralValue::Int64)
    };
    scalar date_business_days_between(text, text) -> int64 [propagate, deterministic] = |a| {
        let x = a.arg_text(0, "date_business_days_between")?;
        let y = a.arg_text(1, "date_business_days_between")?;
        logic::date_business_days_between(&x, &y).map(NeutralValue::Int64)
    };

    // ── ISO week/year named spelling (DuckDB has isoyear / week) ──
    scalar date_iso_week(text) -> int64 [propagate, deterministic] = |a| {
        logic::date_iso_week(&a.arg_text(0, "date_iso_week")?).map(NeutralValue::Int64)
    };
    scalar date_iso_year(text) -> int64 [propagate, deterministic] = |a| {
        logic::date_iso_year(&a.arg_text(0, "date_iso_year")?).map(NeutralValue::Int64)
    };

    // ── duration parse / format (no DuckDB builtin) ──
    scalar duration_parse(text) -> int64 [propagate, deterministic] = |a| {
        logic::duration_parse(&a.arg_text(0, "duration_parse")?).map(NeutralValue::Int64)
    };
    scalar duration_format(int64) -> text [propagate, deterministic] = |a| {
        Ok(NeutralValue::Text(logic::duration_format(a.arg_int(0, "duration_format")?, None)))
    };
    scalar duration_format(int64, int64) -> text [propagate, deterministic] = |a| {
        let n = a.arg_int(0, "duration_format")?;
        let p = a.arg_int(1, "duration_format")? as u32;
        Ok(NeutralValue::Text(logic::duration_format(n, Some(p))))
    };

    scalar chrono_version() -> text [propagate, deterministic] = |_a| {
        Ok(NeutralValue::Text(::alloc::string::String::from(env!("CARGO_PKG_VERSION"))))
    };

    // ── MySQL dialect spellings (no DuckDB builtin) ──
    scalar from_unixtime(int64) -> text [propagate, deterministic] = |a| {
        logic::from_unixtime(a.arg_int(0, "from_unixtime")?).map(NeutralValue::Text)
    };
    scalar timestampdiff(text, text, text) -> int64 [propagate, deterministic] = |a| {
        let u = a.arg_text(0, "timestampdiff")?;
        let x = a.arg_text(1, "timestampdiff")?;
        let y = a.arg_text(2, "timestampdiff")?;
        logic::timestampdiff(&u, &x, &y).map(NeutralValue::Int64)
    };
    scalar timestampadd(text, int64, text) -> text [propagate, deterministic] = |a| {
        let u = a.arg_text(0, "timestampadd")?;
        let n = a.arg_int(1, "timestampadd")?;
        let t = a.arg_text(2, "timestampadd")?;
        logic::timestampadd(&u, n, &t).map(NeutralValue::Text)
    };
    scalar adddate(text, int64) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "adddate")?;
        let n = a.arg_int(1, "adddate")?;
        logic::adddate(&s, n).map(NeutralValue::Text)
    };
    scalar subdate(text, int64) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "subdate")?;
        let n = a.arg_int(1, "subdate")?;
        logic::subdate(&s, n).map(NeutralValue::Text)
    };
    scalar makedate(int64, int64, int64) -> text [propagate, deterministic] = |a| {
        let y = a.arg_int(0, "makedate")?;
        let m = a.arg_int(1, "makedate")?;
        let d = a.arg_int(2, "makedate")?;
        logic::make_date(y, m, d).map(NeutralValue::Text)
    };
    scalar maketime(int64, int64, int64) -> text [propagate, deterministic] = |a| {
        let h = a.arg_int(0, "maketime")?;
        let m = a.arg_int(1, "maketime")?;
        let s = a.arg_int(2, "maketime")?;
        logic::make_time(h, m, s).map(NeutralValue::Text)
    };
    scalar to_char(text, text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "to_char")?;
        let f = a.arg_text(1, "to_char")?;
        logic::date_format(&s, &f).map(NeutralValue::Text)
    };
    scalar str_to_date(text, text) -> text [propagate, deterministic] = |a| {
        let s = a.arg_text(0, "str_to_date")?;
        let f = a.arg_text(1, "str_to_date")?;
        logic::date_parse(&s, Some(&f)).map(NeutralValue::Text)
    };
    scalar from_days(int64) -> text [propagate, deterministic] = |a| {
        logic::from_days(a.arg_int(0, "from_days")?).map(NeutralValue::Text)
    };

    // ── now aliases not provided as DuckDB builtins / keywords ──
    scalar utc_timestamp() -> text [propagate, nondeterministic] = |_a| {
        Ok(NeutralValue::Text(logic::now_utc()))
    };
    scalar sysdate() -> text [propagate, nondeterministic] = |_a| {
        Ok(NeutralValue::Text(logic::now_utc()))
    };

    // ── BigQuery / Snowflake timestamp_* / datetime_* family ──
    scalar timestamp_add(text, int64, text) -> text [propagate, deterministic] = |a| {
        let t = a.arg_text(0, "timestamp_add")?;
        let n = a.arg_int(1, "timestamp_add")?;
        let u = a.arg_text(2, "timestamp_add")?;
        logic::date_add(&t, n, &u).map(NeutralValue::Text)
    };
    scalar timestamp_sub(text, int64, text) -> text [propagate, deterministic] = |a| {
        let t = a.arg_text(0, "timestamp_sub")?;
        let n = a.arg_int(1, "timestamp_sub")?;
        let u = a.arg_text(2, "timestamp_sub")?;
        logic::date_sub(&t, n, &u).map(NeutralValue::Text)
    };
    scalar timestamp_diff(text, text, text) -> int64 [propagate, deterministic] = |a| {
        let x = a.arg_text(0, "timestamp_diff")?;
        let y = a.arg_text(1, "timestamp_diff")?;
        let u = a.arg_text(2, "timestamp_diff")?;
        logic::date_diff(&x, &y, &u).map(NeutralValue::Int64)
    };
    scalar timestamp_trunc(text, text) -> text [propagate, deterministic] = |a| {
        let t = a.arg_text(0, "timestamp_trunc")?;
        let u = a.arg_text(1, "timestamp_trunc")?;
        logic::date_trunc(&u, &t).map(NeutralValue::Text)
    };
    scalar timestamp_micros(int64) -> text [propagate, deterministic] = |a| {
        logic::from_unixtime(a.arg_int(0, "timestamp_micros")? / 1_000_000).map(NeutralValue::Text)
    };
    scalar timestamp_millis(int64) -> text [propagate, deterministic] = |a| {
        logic::from_unixtime(a.arg_int(0, "timestamp_millis")? / 1_000).map(NeutralValue::Text)
    };
    scalar timestamp_seconds(int64) -> text [propagate, deterministic] = |a| {
        logic::from_unixtime(a.arg_int(0, "timestamp_seconds")?).map(NeutralValue::Text)
    };
    scalar datetime_add(text, int64, text) -> text [propagate, deterministic] = |a| {
        let t = a.arg_text(0, "datetime_add")?;
        let n = a.arg_int(1, "datetime_add")?;
        let u = a.arg_text(2, "datetime_add")?;
        logic::date_add(&t, n, &u).map(NeutralValue::Text)
    };
    scalar datetime_sub(text, int64, text) -> text [propagate, deterministic] = |a| {
        let t = a.arg_text(0, "datetime_sub")?;
        let n = a.arg_int(1, "datetime_sub")?;
        let u = a.arg_text(2, "datetime_sub")?;
        logic::date_sub(&t, n, &u).map(NeutralValue::Text)
    };
    scalar datetime_diff(text, text, text) -> int64 [propagate, deterministic] = |a| {
        let x = a.arg_text(0, "datetime_diff")?;
        let y = a.arg_text(1, "datetime_diff")?;
        let u = a.arg_text(2, "datetime_diff")?;
        logic::date_diff(&x, &y, &u).map(NeutralValue::Int64)
    };
    scalar datetime_trunc(text, text) -> text [propagate, deterministic] = |a| {
        let t = a.arg_text(0, "datetime_trunc")?;
        let u = a.arg_text(1, "datetime_trunc")?;
        logic::date_trunc(&u, &t).map(NeutralValue::Text)
    };

    // ── BigQuery parse_*/format_* — NOTE (format, value) arg order ──
    scalar parse_date(text, text) -> text [propagate, deterministic] = |a| {
        let f = a.arg_text(0, "parse_date")?;
        let v = a.arg_text(1, "parse_date")?;
        logic::date_parse(&v, Some(&f)).map(NeutralValue::Text)
    };
    scalar parse_datetime(text, text) -> text [propagate, deterministic] = |a| {
        let f = a.arg_text(0, "parse_datetime")?;
        let v = a.arg_text(1, "parse_datetime")?;
        logic::date_parse(&v, Some(&f)).map(NeutralValue::Text)
    };
    scalar parse_timestamp(text, text) -> text [propagate, deterministic] = |a| {
        let f = a.arg_text(0, "parse_timestamp")?;
        let v = a.arg_text(1, "parse_timestamp")?;
        logic::date_parse(&v, Some(&f)).map(NeutralValue::Text)
    };
    scalar format_date(text, text) -> text [propagate, deterministic] = |a| {
        let f = a.arg_text(0, "format_date")?;
        let v = a.arg_text(1, "format_date")?;
        logic::date_format(&v, &f).map(NeutralValue::Text)
    };
    scalar format_datetime(text, text) -> text [propagate, deterministic] = |a| {
        let f = a.arg_text(0, "format_datetime")?;
        let v = a.arg_text(1, "format_datetime")?;
        logic::date_format(&v, &f).map(NeutralValue::Text)
    };
    scalar format_timestamp(text, text) -> text [propagate, deterministic] = |a| {
        let f = a.arg_text(0, "format_timestamp")?;
        let v = a.arg_text(1, "format_timestamp")?;
        logic::date_format(&v, &f).map(NeutralValue::Text)
    };

    // ── BigQuery unix_* + date_from_unix_date ──
    scalar unix_micros(text) -> int64 [propagate, deterministic] = |a| {
        logic::epoch_us(&a.arg_text(0, "unix_micros")?).map(NeutralValue::Int64)
    };
    scalar unix_millis(text) -> int64 [propagate, deterministic] = |a| {
        logic::epoch_ms(&a.arg_text(0, "unix_millis")?).map(NeutralValue::Int64)
    };
    scalar unix_seconds(text) -> float64 [propagate, deterministic] = |a| {
        logic::epoch(&a.arg_text(0, "unix_seconds")?).map(NeutralValue::Float64)
    };
    scalar date_from_unix_date(int64) -> text [propagate, deterministic] = |a| {
        logic::from_unixtime(a.arg_int(0, "date_from_unix_date")? * 86400).map(NeutralValue::Text)
    };

    // ── date_bucket: time_bucket's non-colliding alias (time_bucket/2 is
    //    a DuckDB builtin; date_bucket is not) ──
    scalar date_bucket(int64, text) -> text [propagate, deterministic] = |a| {
        let n = a.arg_int(0, "date_bucket")?;
        let s = a.arg_text(1, "date_bucket")?;
        logic::time_bucket(n, &s).map(NeutralValue::Text)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalink_extcore::ExtCore;

    fn idx(n: &str, arity: usize) -> usize {
        Core::DECLS
            .iter()
            .position(|d| d.name == n && d.args.len() == arity)
            .unwrap_or_else(|| panic!("no decl {n}/{arity}"))
    }

    fn call(n: &str, arity: usize, args: &[NeutralValue]) -> NeutralValue {
        Core::dispatch(idx(n, arity), args).unwrap()
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(
            call("date_parse", 1, &[NeutralValue::Text("2025-06-20T15:30:00Z".into())]),
            NeutralValue::Text("2025-06-20T15:30:00Z".into())
        );
        assert_eq!(
            call(
                "to_char",
                2,
                &[
                    NeutralValue::Text("2025-06-20T15:30:00Z".into()),
                    NeutralValue::Text("%Y/%m/%d".into())
                ]
            ),
            NeutralValue::Text("2025/06/20".into())
        );
    }

    #[test]
    fn mysql_spellings() {
        assert_eq!(
            call("from_unixtime", 1, &[NeutralValue::Int64(0)]),
            NeutralValue::Text("1970-01-01T00:00:00Z".into())
        );
        assert_eq!(
            call(
                "timestampdiff",
                3,
                &[
                    NeutralValue::Text("day".into()),
                    NeutralValue::Text("2024-01-01".into()),
                    NeutralValue::Text("2024-01-08".into())
                ]
            ),
            NeutralValue::Int64(7)
        );
        assert_eq!(
            call(
                "makedate",
                3,
                &[NeutralValue::Int64(2024), NeutralValue::Int64(2), NeutralValue::Int64(29)]
            ),
            NeutralValue::Text("2024-02-29T00:00:00Z".into())
        );
    }

    #[test]
    fn bigquery_arg_order() {
        // parse_date(format, value)
        assert_eq!(
            call(
                "parse_date",
                2,
                &[
                    NeutralValue::Text("%Y/%m/%d".into()),
                    NeutralValue::Text("2025/06/20".into())
                ]
            ),
            NeutralValue::Text("2025-06-20T00:00:00Z".into())
        );
        assert_eq!(
            call("timestamp_seconds", 1, &[NeutralValue::Int64(0)]),
            NeutralValue::Text("1970-01-01T00:00:00Z".into())
        );
        assert_eq!(
            call("date_from_unix_date", 1, &[NeutralValue::Int64(1)]),
            NeutralValue::Text("1970-01-02T00:00:00Z".into())
        );
    }

    #[test]
    fn duration_roundtrip() {
        assert_eq!(
            call("duration_parse", 1, &[NeutralValue::Text("1d 3h".into())]),
            NeutralValue::Int64(97200)
        );
        assert_eq!(
            call("duration_format", 2, &[NeutralValue::Int64(90061), NeutralValue::Int64(2)]),
            NeutralValue::Text("1d 1h".into())
        );
    }
}
