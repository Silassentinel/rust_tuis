//! Hand-rolled JSON writer.
//!
//! No `serde`. Same reasoning as `rustlogger`'s hand-rolled timestamp
//! formatting (see `CLAUDE.md` rule 4): the output is a small fixed schema we
//! control entirely, so a serialisation framework would be a dependency bought
//! for convenience rather than capability.
//!
//! # Escaping is the security-relevant part
//!
//! Sensor labels, device names and interface names come from the kernel, and on
//! some systems originate in device firmware — a USB device supplies its own
//! name string. This output gets piped into other programs. Every string
//! therefore goes through [`escape_json_string`], which implements RFC 8259
//! §7 properly: `"` and `\` escaped, and **every** control character below
//! `0x20` emitted as `\u00XX` rather than passed through.
//!
//! Getting this wrong doesn't produce a cosmetic bug; it produces a JSON
//! injection into whatever consumes the output.
//!
//! Floats need care too: `f64::NAN` and infinities have no JSON
//! representation, and printing them literally emits `NaN`, which is invalid
//! JSON that most parsers reject. They're emitted as `null`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::delta::Rates;
use crate::error::{Error, Result};
use crate::sample::Snapshot;

/// Schema version, emitted as `"schema": N`.
///
/// Bumped whenever a field changes meaning or is removed, so a consumer can
/// tell. Adding a field does not bump it.
pub const SCHEMA_VERSION: u32 = 1;

/// Cap on a kernel-supplied string (a chip name, a mount path) before it
/// reaches the output. Strings are already sanitised for control characters
/// at the collector boundary — this is a second, independent length bound,
/// not a substitute for that.
const MAX_STRING_LEN: usize = 4096;

/// Write a full snapshot as a single JSON object.
///
/// The schema is documented in `rustmon/README.md` and is a stability
/// commitment — scripts and the MCP-server path depend on it.
pub fn write(out: &mut dyn Write, snapshot: &Snapshot, rates: Option<&Rates>, verbose: bool) -> Result<()> {
    let mut w = JsonWriter::new(out);

    w.begin_object()?;
    w.key("schema")?;
    w.u64_value(u64::from(SCHEMA_VERSION))?;

    w.key("taken_at")?;
    w.str_value(&format_rfc3339(snapshot.taken_at)?)?;

    if let Some(interval) = rates.map(|r| r.interval_secs) {
        w.key("interval_secs")?;
        w.f64_value(interval)?;
    }

    if let Some(cpu) = &snapshot.cpu {
        w.key("cpu")?;
        w.begin_object()?;

        if let Some(model) = &cpu.model {
            w.key("model")?;
            w.str_value(model)?;
        }
        w.key("cores")?;
        w.u64_value(cpu.per_core.len() as u64)?;
        if let Some(load) = cpu.load_avg {
            w.key("load_avg")?;
            w.begin_array()?;
            for l in load {
                w.f64_value(l)?;
            }
            w.end_array()?;
        }
        if let Some(busy) = rates.and_then(|r| r.cpu_total) {
            w.key("busy_percent")?;
            w.f64_value(busy.as_f64())?;
        }

        w.key("per_core")?;
        w.begin_array()?;
        for (idx, freq) in cpu.freq_khz.iter().enumerate() {
            w.begin_object()?;
            w.key("core")?;
            w.u64_value(idx as u64)?;
            if let Some(freq) = freq {
                w.key("freq_khz")?;
                w.u64_value(freq.as_khz())?;
            }
            if let Some(busy) = rates.and_then(|r| r.cpu_per_core.get(idx).copied().flatten()) {
                w.key("busy_percent")?;
                w.f64_value(busy.as_f64())?;
            }
            w.end_object()?;
        }
        w.end_array()?;

        w.end_object()?;
    }

    if let Some(mem) = &snapshot.memory {
        w.key("memory")?;
        w.begin_object()?;
        w.key("total_bytes")?;
        w.u64_value(mem.total.as_u64())?;
        w.key("available_bytes")?;
        w.u64_value(mem.available.as_u64())?;
        w.key("used_bytes")?;
        w.u64_value(mem.used().as_u64())?;
        w.key("swap_total_bytes")?;
        w.u64_value(mem.swap_total.as_u64())?;
        w.key("swap_used_bytes")?;
        w.u64_value(mem.swap_used().as_u64())?;
        w.end_object()?;
    }

    if let Some(thermal) = &snapshot.thermal {
        w.key("thermal")?;
        w.begin_array()?;
        for chip in &thermal.chips {
            w.begin_object()?;
            w.key("name")?;
            w.str_value(&chip.name)?;

            w.key("temps")?;
            w.begin_array()?;
            for t in &chip.temps {
                w.begin_object()?;
                w.key("label")?;
                w.str_value(&t.label)?;
                w.key("celsius")?;
                w.f64_value(t.value.as_celsius())?;
                if let Some(max) = t.max {
                    w.key("max_celsius")?;
                    w.f64_value(max.as_celsius())?;
                }
                if let Some(crit) = t.crit {
                    w.key("crit_celsius")?;
                    w.f64_value(crit.as_celsius())?;
                }
                w.end_object()?;
            }
            w.end_array()?;

            w.key("fans")?;
            w.begin_array()?;
            for f in &chip.fans {
                w.begin_object()?;
                w.key("label")?;
                w.str_value(&f.label)?;
                w.key("rpm")?;
                w.u64_value(f.rpm.as_u64())?;
                w.end_object()?;
            }
            w.end_array()?;

            w.end_object()?;
        }
        w.end_array()?;
    }

    if let Some(disk) = &snapshot.disks {
        w.key("disk")?;
        w.begin_array()?;
        for d in &disk.devices {
            w.begin_object()?;
            w.key("name")?;
            w.str_value(&d.name)?;
            if let Some(r) = rates.and_then(|r| r.disk.get(&d.name)) {
                w.key("read_bytes_per_sec")?;
                w.f64_value(r.read.as_f64())?;
                w.key("write_bytes_per_sec")?;
                w.f64_value(r.write.as_f64())?;
                w.key("read_iops")?;
                w.f64_value(r.read_iops)?;
                w.key("write_iops")?;
                w.f64_value(r.write_iops)?;
                if let Some(util) = r.utilisation {
                    w.key("utilisation_percent")?;
                    w.f64_value(util.as_f64())?;
                }
            }
            w.end_object()?;
        }
        w.end_array()?;

        w.key("mounts")?;
        w.begin_array()?;
        for m in &disk.mounts {
            w.begin_object()?;
            w.key("source")?;
            w.str_value(&m.source)?;
            w.key("mount_point")?;
            w.str_value(&m.mount_point)?;
            w.key("fs_type")?;
            w.str_value(&m.fs_type)?;
            if let Some(total) = m.total {
                w.key("total_bytes")?;
                w.u64_value(total.as_u64())?;
            }
            if let Some(available) = m.available {
                w.key("available_bytes")?;
                w.u64_value(available.as_u64())?;
            }
            w.end_object()?;
        }
        w.end_array()?;
    }

    if let Some(net) = &snapshot.net {
        w.key("net")?;
        w.begin_array()?;
        for i in &net.interfaces {
            w.begin_object()?;
            w.key("name")?;
            w.str_value(&i.name)?;
            if let Some(state) = &i.operstate {
                w.key("operstate")?;
                w.str_value(state)?;
            }
            if let Some(mtu) = i.mtu {
                w.key("mtu")?;
                w.u64_value(mtu)?;
            }
            if let Some(r) = rates.and_then(|r| r.net.get(&i.name)) {
                w.key("rx_bytes_per_sec")?;
                w.f64_value(r.rx.as_f64())?;
                w.key("tx_bytes_per_sec")?;
                w.f64_value(r.tx.as_f64())?;
                w.key("rx_packets_per_sec")?;
                w.f64_value(r.rx_packets_per_sec)?;
                w.key("tx_packets_per_sec")?;
                w.f64_value(r.tx_packets_per_sec)?;
            }
            w.end_object()?;
        }
        w.end_array()?;
    }

    if let Some(gpu) = &snapshot.gpus {
        w.key("gpu")?;
        w.begin_array()?;
        for g in &gpu.gpus {
            w.begin_object()?;
            w.key("name")?;
            w.str_value(&g.name)?;
            w.key("vendor")?;
            w.str_value(match g.vendor {
                crate::sample::GpuVendor::Unknown => "unknown",
                crate::sample::GpuVendor::Amd => "amd",
                crate::sample::GpuVendor::Intel => "intel",
                crate::sample::GpuVendor::Nvidia => "nvidia",
            })?;
            if let Some(busy) = g.busy {
                w.key("busy_percent")?;
                w.f64_value(busy.as_f64())?;
            }
            if let Some(freq) = g.freq_khz {
                w.key("freq_khz")?;
                w.u64_value(freq.as_khz())?;
            }
            if let Some(total) = g.vram_total {
                w.key("vram_total_bytes")?;
                w.u64_value(total.as_u64())?;
            }
            if let Some(used) = g.vram_used {
                w.key("vram_used_bytes")?;
                w.u64_value(used.as_u64())?;
            }
            if let Some(temp) = g.temp {
                w.key("celsius")?;
                w.f64_value(temp.as_celsius())?;
            }
            if let Some(power) = g.power {
                w.key("watts")?;
                w.f64_value(power.as_watts())?;
            }
            if let Some(fan) = g.fan_rpm {
                w.key("fan_rpm")?;
                w.u64_value(fan.as_u64())?;
            }
            w.end_object()?;
        }
        w.end_array()?;
    }

    if let Some(connections) = &snapshot.connections {
        w.key("connections")?;
        w.begin_array()?;
        for c in &connections.connections {
            w.begin_object()?;
            w.key("protocol")?;
            w.str_value(match c.protocol {
                crate::sample::ConnProtocol::Tcp => "tcp",
                crate::sample::ConnProtocol::Udp => "udp",
            })?;
            w.key("local_addr")?;
            w.str_value(&c.local_addr.to_string())?;
            w.key("local_port")?;
            w.u64_value(u64::from(c.local_port))?;
            w.key("remote_addr")?;
            w.str_value(&c.remote_addr.to_string())?;
            w.key("remote_port")?;
            w.u64_value(u64::from(c.remote_port))?;
            if let Some(state) = c.state {
                w.key("state")?;
                w.str_value(state.as_str())?;
            }
            w.key("uid")?;
            w.u64_value(u64::from(c.uid))?;
            if let Some(pid) = c.pid {
                w.key("pid")?;
                w.u64_value(u64::from(pid))?;
            }
            if let Some(program) = &c.program {
                w.key("program")?;
                w.str_value(program)?;
            }
            w.end_object()?;
        }
        w.end_array()?;
    }

    if verbose {
        w.key("errors")?;
        w.begin_array()?;
        for e in &snapshot.errors {
            w.begin_object()?;
            w.key("collector")?;
            w.str_value(e.collector)?;
            w.key("message")?;
            w.str_value(&e.message)?;
            w.end_object()?;
        }
        w.end_array()?;
    }

    w.end_object()?;

    // A single JSON document per invocation, newline-terminated so it's
    // still pleasant piped into a file or a terminal, same as `--format
    // text`.
    out.write_all(b"\n").map_err(io_err)?;

    Ok(())
}

/// Escape a string per RFC 8259 §7, including the surrounding quotes.
///
/// Handles `"`, `\`, `\n`, `\r`, `\t`, `\u{8}`, `\u{c}`, and emits every other
/// character below `0x20` as `\u00XX`. Also escapes `U+007F` and the C1 range,
/// which are legal JSON but hostile to a terminal that later displays the
/// output.
pub fn escape_json_string(s: &str) -> String {
    // Truncated to a character boundary (never a byte boundary) before
    // escaping — `s` is already sanitised at the collector boundary in the
    // common case, but this writer must not assume every caller remembered
    // to, hence `MAX_STRING_LEN` here too.
    let s: String = s.chars().take(MAX_STRING_LEN).collect();

    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');

    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // U+007F (DEL) and the C1 control range (U+0080..=U+009F) are
            // legal JSON as literal characters, but hostile to a terminal
            // that later renders this output — escaped for the same reason
            // `sysfs::sanitize_kernel_string` strips them at the collector
            // boundary, applied here too in case a caller ever bypasses it.
            '\u{7f}' => out.push_str("\\u007f"),
            c if (0x80..=0x9f).contains(&(c as u32)) => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }

    out.push('"');
    out
}

/// Format an `f64` as a JSON number, or `null` if it isn't finite.
pub fn json_number(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".to_string()
    }
}

/// Days-since-epoch to `(year, month, day)`, proleptic Gregorian.
///
/// Howard Hinnant's `civil_from_days` algorithm — the same one
/// `rustlogger/src/timestamp.rs` uses for its own hand-rolled UTC formatting.
/// See that module's doc for the reference and the reasoning for hand-rolling
/// this instead of adding a time-formatting crate (`CLAUDE.md` rule 4).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

/// Format a [`std::time::SystemTime`] as an RFC 3339 UTC string.
///
/// Hand-rolled against `SystemTime` — the same approach `rustlogger`'s
/// `timestamp.rs` takes, and worth reading that module before rewriting the
/// leap-year and month-length arithmetic here. Unlike that module's
/// infallible `format_utc` (which silently falls back to the epoch), this one
/// returns `Err` on a time before 1970 — every `Snapshot::taken_at` comes
/// from `SystemTime::now()`, so in practice this branch is unreachable, but
/// the `Result` signature exists so a future caller can't be tempted to
/// assume otherwise.
pub fn format_rfc3339(t: SystemTime) -> Result<String> {
    let duration = t.duration_since(SystemTime::UNIX_EPOCH).map_err(|_| {
        Error::parse(Path::new("<timestamp>"), None, "system time predates the Unix epoch")
    })?;

    let secs = duration.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;

    Ok(format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"))
}

/// A minimal writer that tracks nesting so the emitters don't hand-manage
/// commas and braces — that's where hand-rolled JSON usually breaks.
pub struct JsonWriter<'a> {
    out: &'a mut dyn Write,
    /// One flag per open container: has a member been written yet? (`true`
    /// means the *next* one needs a leading comma.)
    needs_comma: Vec<bool>,
    /// Set by [`key`](Self::key), consumed by the very next value write.
    /// This is what stops a comma being inserted between `"key":` and its
    /// value — a member's key and value are one unit, not two, but
    /// `key()` and the value-writing methods are separate calls and the
    /// value methods are *also* used bare, as array elements, where they
    /// must insert a comma. This flag is how the same method knows which
    /// situation it's in.
    after_key: bool,
}

impl<'a> JsonWriter<'a> {
    pub fn new(out: &'a mut dyn Write) -> Self {
        JsonWriter {
            out,
            needs_comma: Vec::new(),
            after_key: false,
        }
    }

    /// Insert a comma if this call is starting a new member/element in the
    /// current container — and record that the *next* one will need one.
    /// Suppressed exactly once by a preceding [`key`](Self::key) call, since
    /// a key and its value are one member, not two.
    fn maybe_comma(&mut self) -> Result<()> {
        if self.after_key {
            self.after_key = false;
            return Ok(());
        }
        if let Some(needs) = self.needs_comma.last_mut() {
            if *needs {
                self.out.write_all(b",").map_err(io_err)?;
            }
            *needs = true;
        }
        Ok(())
    }

    pub fn begin_object(&mut self) -> Result<()> {
        self.maybe_comma()?;
        self.out.write_all(b"{").map_err(io_err)?;
        self.needs_comma.push(false);
        Ok(())
    }

    pub fn end_object(&mut self) -> Result<()> {
        self.needs_comma.pop();
        self.out.write_all(b"}").map_err(io_err)?;
        Ok(())
    }

    pub fn begin_array(&mut self) -> Result<()> {
        self.maybe_comma()?;
        self.out.write_all(b"[").map_err(io_err)?;
        self.needs_comma.push(false);
        Ok(())
    }

    pub fn end_array(&mut self) -> Result<()> {
        self.needs_comma.pop();
        self.out.write_all(b"]").map_err(io_err)?;
        Ok(())
    }

    pub fn key(&mut self, k: &str) -> Result<()> {
        self.maybe_comma()?;
        self.out.write_all(escape_json_string(k).as_bytes()).map_err(io_err)?;
        self.out.write_all(b":").map_err(io_err)?;
        self.after_key = true;
        Ok(())
    }

    pub fn str_value(&mut self, v: &str) -> Result<()> {
        self.maybe_comma()?;
        self.out.write_all(escape_json_string(v).as_bytes()).map_err(io_err)?;
        Ok(())
    }

    pub fn u64_value(&mut self, v: u64) -> Result<()> {
        self.maybe_comma()?;
        write!(self.out, "{v}").map_err(io_err)?;
        Ok(())
    }

    pub fn f64_value(&mut self, v: f64) -> Result<()> {
        self.maybe_comma()?;
        self.out.write_all(json_number(v).as_bytes()).map_err(io_err)?;
        Ok(())
    }

    pub fn null_value(&mut self) -> Result<()> {
        self.maybe_comma()?;
        self.out.write_all(b"null").map_err(io_err)?;
        Ok(())
    }

    pub fn bool_value(&mut self, v: bool) -> Result<()> {
        self.maybe_comma()?;
        self.out.write_all(if v { b"true" } else { b"false" }).map_err(io_err)?;
        Ok(())
    }
}

/// Every write error in this module gets the same synthetic path: `out` is
/// an arbitrary [`Write`] (stdout in production, a `Vec<u8>` in tests), not
/// a `sysfs`-confined file, so there's no real path to report — but
/// [`Error::Io`] always needs one, and `"<output>"` says plainly where the
/// failure was, same idea as `format_rfc3339`'s `"<timestamp>"`.
fn io_err(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<output>"),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{CollectorError, CpuSample, CpuTimes, MemorySample};
    use crate::units::Bytes;
    use std::time::Duration;

    // ---- escape_json_string ---------------------------------------------------

    #[test]
    fn escapes_the_required_rfc8259_sequences() {
        assert_eq!(escape_json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(escape_json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(escape_json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(escape_json_string("a\rb"), "\"a\\rb\"");
        assert_eq!(escape_json_string("a\tb"), "\"a\\tb\"");
        assert_eq!(escape_json_string("plain"), "\"plain\"");
    }

    #[test]
    fn escapes_every_control_character_below_0x20() {
        for code in 0u32..0x20 {
            let c = char::from_u32(code).unwrap();
            let s = String::from(c);
            let escaped = escape_json_string(&s);
            // The named two-char escapes (\n, \r, \t, \b, \f) are also valid
            // RFC 8259 output; everything else must be \u00XX.
            let acceptable_named = matches!(code, 0x08 | 0x09 | 0x0a | 0x0c | 0x0d);
            if !acceptable_named {
                assert_eq!(escaped, format!("\"\\u{code:04x}\""), "code point {code:#x}");
            }
            assert!(!escaped[1..escaped.len() - 1].chars().any(|c| (c as u32) < 0x20 && !matches!(c, '\\')), "{escaped:?} leaked a raw control byte");
        }
    }

    /// Legal JSON as literal bytes, but hostile to a terminal — the module
    /// doc's specific reason these get escaped even though RFC 8259 doesn't
    /// require it.
    #[test]
    fn escapes_del_and_the_c1_control_range() {
        assert_eq!(escape_json_string("\u{7f}"), "\"\\u007f\"");
        assert_eq!(escape_json_string("\u{9b}"), "\"\\u009b\""); // C1 CSI
        assert_eq!(escape_json_string("\u{80}"), "\"\\u0080\"");
        assert_eq!(escape_json_string("\u{9f}"), "\"\\u009f\"");
    }

    /// An unsanitised device name would have to look like this to break
    /// something downstream — the writer's own escaping is the last line of
    /// defence even though collectors are expected to sanitise first.
    #[test]
    fn an_embedded_ansi_escape_survives_only_as_an_escaped_sequence() {
        let hostile = "\u{1b}[2Jdevice";
        let escaped = escape_json_string(hostile);
        assert!(!escaped.contains('\u{1b}'), "{escaped:?}");
        assert!(escaped.contains("\\u001b"));
    }

    #[test]
    fn a_string_over_the_length_cap_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(MAX_STRING_LEN + 10);
        let escaped = escape_json_string(&long);
        // Every char in the truncated body must be a full "é" (2 bytes),
        // never half of one — proven by successfully round-tripping through
        // `char::from_u32`/String at all (a byte-boundary truncation of a
        // 2-byte UTF-8 char would have made `escape_json_string` itself
        // panic on `s.chars()` earlier, since the input wouldn't be valid
        // UTF-8 in the first place — this asserts the *count*, not just
        // that it didn't panic).
        assert_eq!(escaped.chars().filter(|&c| c == 'é').count(), MAX_STRING_LEN);
    }

    // ---- json_number ------------------------------------------------------

    #[test]
    fn finite_numbers_format_normally() {
        assert_eq!(json_number(1.5), "1.5");
        assert_eq!(json_number(0.0), "0");
        assert_eq!(json_number(-3.25), "-3.25");
    }

    /// `NaN`/`inf`/`-inf` have no JSON representation; printing them
    /// literally would emit `NaN`, which most parsers reject outright.
    #[test]
    fn non_finite_numbers_become_null() {
        assert_eq!(json_number(f64::NAN), "null");
        assert_eq!(json_number(f64::INFINITY), "null");
        assert_eq!(json_number(f64::NEG_INFINITY), "null");
    }

    // ---- format_rfc3339 -------------------------------------------------------

    /// Same reference dates `rustlogger/src/timestamp.rs` checks against
    /// `date -u -d @<epoch>` — this module borrows that exact algorithm, so
    /// reusing its check set is a direct parity proof, not just a spot check.
    #[test]
    fn matches_known_reference_dates() {
        let cases: &[(u64, &str)] = &[
            (0, "1970-01-01T00:00:00Z"),
            (1, "1970-01-01T00:00:01Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02T00:00:00Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
            (951_782_400, "2000-02-29T00:00:00Z"), // leap day
            (1_735_689_599, "2024-12-31T23:59:59Z"),
        ];

        for &(secs, expected) in cases {
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(format_rfc3339(t).unwrap(), expected, "mismatch for epoch {secs}");
        }
    }

    #[test]
    fn a_time_before_the_epoch_is_an_error_not_a_silent_fallback() {
        let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
        assert!(format_rfc3339(before_epoch).is_err());
    }

    // ---- JsonWriter comma/nesting placement --------------------------------

    fn written(f: impl FnOnce(&mut JsonWriter) -> Result<()>) -> String {
        let mut buf = Vec::new();
        let mut w = JsonWriter::new(&mut buf);
        f(&mut w).expect("writer calls succeed");
        String::from_utf8(buf).expect("valid utf-8")
    }

    #[test]
    fn empty_object_and_array() {
        assert_eq!(written(|w| { w.begin_object()?; w.end_object() }), "{}");
        assert_eq!(written(|w| { w.begin_array()?; w.end_array() }), "[]");
    }

    #[test]
    fn object_with_two_members_gets_exactly_one_comma() {
        let out = written(|w| {
            w.begin_object()?;
            w.key("a")?;
            w.u64_value(1)?;
            w.key("b")?;
            w.u64_value(2)?;
            w.end_object()
        });
        assert_eq!(out, "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn array_elements_are_comma_separated_with_no_comma_before_the_first() {
        let out = written(|w| {
            w.begin_array()?;
            w.u64_value(1)?;
            w.u64_value(2)?;
            w.u64_value(3)?;
            w.end_array()
        });
        assert_eq!(out, "[1,2,3]");
    }

    /// The specific bug the `after_key` flag exists to prevent: a comma
    /// between `"key":` and its value.
    #[test]
    fn no_comma_appears_between_a_key_and_its_value() {
        let out = written(|w| {
            w.begin_object()?;
            w.key("only")?;
            w.str_value("value")?;
            w.end_object()
        });
        assert_eq!(out, "{\"only\":\"value\"}");
    }

    #[test]
    fn nested_object_in_array_and_array_in_object() {
        let out = written(|w| {
            w.begin_array()?;
            w.begin_object()?;
            w.key("x")?;
            w.u64_value(1)?;
            w.end_object()?;
            w.begin_object()?;
            w.key("y")?;
            w.u64_value(2)?;
            w.end_object()?;
            w.end_array()
        });
        assert_eq!(out, "[{\"x\":1},{\"y\":2}]");

        let out = written(|w| {
            w.begin_object()?;
            w.key("list")?;
            w.begin_array()?;
            w.u64_value(1)?;
            w.u64_value(2)?;
            w.end_array()?;
            w.key("after")?;
            w.bool_value(true)?;
            w.end_object()
        });
        assert_eq!(out, "{\"list\":[1,2],\"after\":true}");
    }

    #[test]
    fn null_and_bool_values() {
        let out = written(|w| {
            w.begin_array()?;
            w.null_value()?;
            w.bool_value(true)?;
            w.bool_value(false)?;
            w.end_array()
        });
        assert_eq!(out, "[null,true,false]");
    }

    #[test]
    fn f64_value_emits_null_for_non_finite() {
        let out = written(|w| {
            w.begin_array()?;
            w.f64_value(1.5)?;
            w.f64_value(f64::NAN)?;
            w.end_array()
        });
        assert_eq!(out, "[1.5,null]");
    }

    // ---- write() / golden snapshot ------------------------------------------

    /// Deliberately minimal, hand-rolled JSON structural check — not a full
    /// parser (no crate for that per `CLAUDE.md` rule 4, and a real DOM
    /// model is more than a test needs). It walks the byte stream checking
    /// brace/bracket balance and that no raw control byte escaped a string
    /// unescaped, which is exactly the failure mode a hand-rolled *writer*
    /// is at risk of (a missed comma, a mismatched container, an
    /// under-escaped string) — the same class of bug `escape_json_string`'s
    /// and `JsonWriter`'s own tests target directly, checked here again at
    /// the whole-document level.
    fn assert_structurally_valid_json(s: &str) {
        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escaped = false;

        for c in s.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else {
                    match c {
                        '\\' => escaped = true,
                        '"' => in_string = false,
                        c if (c as u32) < 0x20 => {
                            panic!("raw control byte {:#x} inside a JSON string in: {s}", c as u32)
                        }
                        _ => {}
                    }
                }
                continue;
            }

            match c {
                '"' => in_string = true,
                '{' | '[' => stack.push(c),
                '}' => assert_eq!(stack.pop(), Some('{'), "unbalanced }} in: {s}"),
                ']' => assert_eq!(stack.pop(), Some('['), "unbalanced ] in: {s}"),
                _ => {}
            }
        }

        assert!(!in_string, "unterminated string in: {s}");
        assert!(stack.is_empty(), "unclosed containers {stack:?} in: {s}");
    }

    fn full_snapshot() -> Snapshot {
        let mut s = Snapshot::now();
        s.cpu = Some(CpuSample {
            total: CpuTimes { user: 100, idle: 900, ..CpuTimes::default() },
            per_core: vec![CpuTimes::default()],
            freq_khz: vec![Some(crate::units::KiloHertz::from_khz(3_600_000))],
            load_avg: Some([1.0, 1.5, 2.0]),
            model: Some("Test CPU".to_string()),
            ctxt: Some(1),
            btime: Some(2),
        });
        s.memory = Some(MemorySample {
            total: Bytes::from_bytes(1000),
            available: Bytes::from_bytes(400),
            free: Bytes::from_bytes(100),
            ..MemorySample::default()
        });
        s.errors.push(CollectorError { collector: "gpu", message: "boom".to_string() });
        s
    }

    #[test]
    fn golden_snapshot_is_structurally_valid_and_contains_expected_fields() {
        let snapshot = full_snapshot();
        let mut buf = Vec::new();
        write(&mut buf, &snapshot, None, true).expect("writes fine");
        let text = String::from_utf8(buf).expect("valid utf-8");

        assert_structurally_valid_json(text.trim());

        assert!(text.contains("\"schema\":1"));
        assert!(text.contains("\"model\":\"Test CPU\""));
        assert!(text.contains("\"cores\":1"));
        assert!(text.contains("\"total_bytes\":1000"));
        // used = total - available = 600.
        assert!(text.contains("\"used_bytes\":600"));
        assert!(text.contains("\"collector\":\"gpu\""));
        assert!(text.contains("\"message\":\"boom\""));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn verbose_false_omits_the_errors_array() {
        let snapshot = full_snapshot();
        let mut buf = Vec::new();
        write(&mut buf, &snapshot, None, false).expect("writes fine");
        let text = String::from_utf8(buf).unwrap();

        assert_structurally_valid_json(text.trim());
        assert!(!text.contains("\"errors\""));
    }

    #[test]
    fn an_empty_snapshot_is_still_valid_json() {
        let snapshot = Snapshot::now();
        let mut buf = Vec::new();
        write(&mut buf, &snapshot, None, false).expect("writes fine");
        let text = String::from_utf8(buf).unwrap();

        assert_structurally_valid_json(text.trim());
        assert!(text.contains("\"schema\":1"));
        assert!(!text.contains("\"cpu\""));
    }

    /// Round-trip in the sense the TODO asks for: what this writer produces,
    /// a structural walk can read back without finding a broken container or
    /// an under-escaped control byte — for every section this crate knows
    /// how to emit, not just the happy path above.
    #[test]
    fn round_trip_every_section_is_structurally_sound() {
        use crate::delta::{DiskRate, NetRate, Rates};
        use std::collections::HashMap;

        let mut snapshot = full_snapshot();
        snapshot.thermal = Some(crate::sample::ThermalSample {
            chips: vec![crate::sample::HwmonChip {
                name: "chip".to_string(),
                temps: vec![crate::sample::TempSensor {
                    label: "temp1".to_string(),
                    value: crate::units::MilliCelsius::from_millidegrees(45_000),
                    max: Some(crate::units::MilliCelsius::from_millidegrees(90_000)),
                    crit: None,
                }],
                fans: vec![crate::sample::FanSensor {
                    label: "fan1".to_string(),
                    rpm: crate::units::Rpm::from_rpm(1200),
                }],
            }],
        });
        snapshot.disks = Some(crate::sample::DiskSample {
            devices: vec![crate::sample::DiskDevice {
                name: "sda".to_string(),
                reads_completed: 1,
                writes_completed: 2,
                sectors_read: 3,
                sectors_written: 4,
                io_ticks_ms: 5,
            }],
            mounts: vec![crate::sample::MountPoint {
                source: "/dev/sda1".to_string(),
                mount_point: "/".to_string(),
                fs_type: "ext4".to_string(),
                total: Some(Bytes::from_bytes(1000)),
                available: Some(Bytes::from_bytes(500)),
            }],
        });
        snapshot.net = Some(crate::sample::NetSample {
            interfaces: vec![crate::sample::NetInterface {
                name: "eth0".to_string(),
                rx_bytes: 1,
                tx_bytes: 2,
                rx_packets: 3,
                tx_packets: 4,
                rx_errors: 0,
                tx_errors: 0,
                rx_dropped: 0,
                tx_dropped: 0,
                operstate: Some("up".to_string()),
                mtu: Some(1500),
            }],
        });
        snapshot.gpus = Some(crate::sample::GpuSample {
            gpus: vec![crate::sample::Gpu {
                vendor: crate::sample::GpuVendor::Amd,
                name: "card0".to_string(),
                busy: Some(crate::units::Percent::new(10.0)),
                vram_total: Some(Bytes::from_bytes(2000)),
                vram_used: Some(Bytes::from_bytes(500)),
                temp: Some(crate::units::MilliCelsius::from_millidegrees(50_000)),
                power: Some(crate::units::MicroWatts::from_microwatts(9_000_000)),
                fan_rpm: Some(crate::units::Rpm::from_rpm(800)),
                freq_khz: None,
            }],
        });
        snapshot.connections = Some(crate::sample::ConnectionSample {
            connections: vec![crate::sample::Connection {
                protocol: crate::sample::ConnProtocol::Tcp,
                local_addr: "192.168.1.1".parse().unwrap(),
                local_port: 51000,
                remote_addr: "8.8.8.8".parse().unwrap(),
                remote_port: 443,
                state: Some(crate::sample::TcpState::Established),
                uid: 1000,
                pid: Some(42),
                program: Some("curl".to_string()),
            }],
        });

        let mut disk_rates = HashMap::new();
        disk_rates.insert(
            "sda".to_string(),
            DiskRate {
                read: crate::units::BytesPerSec::new(100.0),
                write: crate::units::BytesPerSec::new(50.0),
                read_iops: 1.0,
                write_iops: 2.0,
                utilisation: Some(crate::units::Percent::new(5.0)),
            },
        );
        let mut net_rates = HashMap::new();
        net_rates.insert(
            "eth0".to_string(),
            NetRate {
                rx: crate::units::BytesPerSec::new(10.0),
                tx: crate::units::BytesPerSec::new(20.0),
                rx_packets_per_sec: 1.0,
                tx_packets_per_sec: 2.0,
            },
        );
        let rates = Rates {
            interval_secs: 1.0,
            cpu_total: Some(crate::units::Percent::new(12.5)),
            cpu_per_core: vec![Some(crate::units::Percent::new(12.5))],
            disk: disk_rates,
            net: net_rates,
        };

        let mut buf = Vec::new();
        write(&mut buf, &snapshot, Some(&rates), true).expect("writes fine");
        let text = String::from_utf8(buf).expect("valid utf-8");

        assert_structurally_valid_json(text.trim());
        for expected in [
            "\"thermal\"", "\"disk\"", "\"mounts\"", "\"net\"", "\"gpu\"",
            "\"read_bytes_per_sec\":100", "\"rx_bytes_per_sec\":10",
            "\"vendor\":\"amd\"", "\"connections\"", "\"remote_addr\":\"8.8.8.8\"",
            "\"program\":\"curl\"",
        ] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
    }
}
