//! Reverse DNS (PTR) resolution.
//!
//! A minimal, hand-rolled DNS client speaking exactly one query shape (a
//! PTR lookup, over UDP) to exactly one nameserver: whatever
//! `/etc/resolv.conf` names first. This is deliberately not a general
//! resolver — no retry, no fallback nameservers, no caching (the caller,
//! `ui::app`'s enrichment cache, already owns caching), no A/AAAA/CNAME
//! handling. Zero new dependencies: `std::net::UdpSocket` is enough for
//! one query/response round trip.
//!
//! **This transparently benefits from whatever's actually running as the
//! configured resolver** — including a local validating/caching resolver
//! like `unbound`, if that's what `/etc/resolv.conf` points at. Nothing
//! here is resolver-specific; it just speaks the wire protocol any
//! RFC 1035-compliant server understands.
//!
//! # Bounded, like every other I/O in this crate
//!
//! A DNS round trip over a real network is far more likely to stall than
//! any local syscall this crate makes — [`QUERY_TIMEOUT`] bounds it the
//! same way chunk 8's `statvfs` timeout bounds a hanging filesystem.
//! Response parsing is bounded too: [`MAX_LABEL_JUMPS`] caps how many
//! compression pointers a single name may follow (RFC 1035 §4.1.4 allows
//! pointers specifically so a hostile or corrupted response can't be built
//! that decodes into gigabytes of labels), and every pointer must point
//! strictly backward in the message, which — combined with the jump cap —
//! makes an infinite loop structurally impossible, not just unlikely.
//!
//! # Untrusted input
//!
//! A DNS response is network-supplied data from a resolver that is, in
//! turn, often relaying an answer from a third party (recursion). A
//! resolved name is sanitised through
//! [`crate::sysfs::sanitize_kernel_string`] before this module ever
//! returns it — the same treatment every kernel-supplied string in this
//! crate gets, extended here to a network-supplied one, since both reach
//! the same TUI and the same JSON output.

use std::net::{IpAddr, UdpSocket};
use std::path::Path;
use std::time::Duration;

use crate::sysfs::{sanitize_kernel_string, SysfsReader};

const DNS_PORT: u16 = 53;

/// How long to wait for a response before giving up on this one lookup.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Generous upper bound on a DNS response over UDP — real PTR responses
/// are a few hundred bytes; this leaves headroom without inviting an
/// unbounded read.
const MAX_RESPONSE_BYTES: usize = 4096;

/// RFC 1035 §3.1's domain-name length limit, reused here as the sanitised
/// output length cap too.
const MAX_NAME_LEN: usize = 253;

/// Cap on compression-pointer hops per name — see this module's own doc
/// for why this, combined with the backward-only rule in [`decode_name`],
/// makes an infinite loop structurally impossible rather than just rare.
const MAX_LABEL_JUMPS: usize = 16;

const QTYPE_PTR: u16 = 12;
const QCLASS_IN: u16 = 1;

/// Resolve `addr` to its PTR record(s), or `None` on any failure —
/// unconfigured/unreachable nameserver, a timeout, or a malformed
/// response. `Some(vec![])` (as opposed to `None`) means the query
/// completed and the server affirmatively reported no PTR record (e.g.
/// `NXDOMAIN`), which is a different, more informative outcome than
/// "couldn't even ask" and worth keeping distinct for the caller.
pub fn resolve_ptr(reader: &SysfsReader, addr: IpAddr) -> Option<Vec<String>> {
    let nameserver = read_nameserver(reader)?;
    let id = query_id();
    let query = build_query(id, &ptr_qname(addr));
    let response = send_and_receive(nameserver, &query).ok()?;
    parse_ptr_response(&response, id)
}

/// The first `nameserver` line in `/etc/resolv.conf`. Read through the same
/// [`SysfsReader`] every collector uses — `/etc` isn't under `/proc`/`/sys`,
/// but with `sysfs_root` defaulting to `/` in production, `etc/resolv.conf`
/// is just one more path under the same confined root `proc/net/dev`
/// already reads from.
fn read_nameserver(reader: &SysfsReader) -> Option<IpAddr> {
    let contents = reader.read_to_string(Path::new("etc/resolv.conf")).ok()?.ok()?;
    contents.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? == "nameserver" {
            parts.next()?.parse::<IpAddr>().ok()
        } else {
            None
        }
    })
}

/// Not cryptographically random on purpose: this is a one-shot round trip
/// over a socket this process just `connect`ed to a single peer, not a
/// query pattern exposed to third-party spoofing the way an open recursive
/// resolver is. The only thing the id needs to do is avoid confusing this
/// query's response with a stale one from an earlier lookup.
fn query_id() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u16
}

/// `a.b.c.d` → `d.c.b.a.in-addr.arpa`; IPv6 → each nibble reversed, then
/// `.ip6.arpa` — RFC 1035 §3.5 / RFC 3596 §2.5.
fn ptr_qname(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0])
        }
        IpAddr::V6(v6) => {
            let mut name = String::new();
            for byte in v6.octets().iter().rev() {
                name.push_str(&format!("{:x}.{:x}.", byte & 0x0f, byte >> 4));
            }
            name.push_str("ip6.arpa");
            name
        }
    }
}

/// Encode a dotted name into DNS wire format: length-prefixed labels,
/// terminated by a zero-length root label.
fn encode_qname(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.split('.') {
        let bytes = label.as_bytes();
        // 63 bytes is RFC 1035's own per-label limit. Every label this
        // function is ever called with (decimal octets, hex nibbles,
        // "in-addr"/"ip6"/"arpa") is far under it — this is a defensive
        // cap against a future caller, not something real input can hit.
        let len = bytes.len().min(63);
        out.push(len as u8);
        out.extend_from_slice(&bytes[..len]);
    }
    out.push(0);
    out
}

/// Build a one-question PTR query. Flags: `RD` (recursion desired) set,
/// everything else zero — this is a stub resolver asking its configured
/// server to do the work, not attempting recursion itself.
fn build_query(id: u16, qname: &str) -> Vec<u8> {
    let qname = encode_qname(qname);
    let mut out = Vec::with_capacity(12 + qname.len() + 4);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: RD=1
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&qname);
    out.extend_from_slice(&QTYPE_PTR.to_be_bytes());
    out.extend_from_slice(&QCLASS_IN.to_be_bytes());
    out
}

fn send_and_receive(nameserver: IpAddr, query: &[u8]) -> std::io::Result<Vec<u8>> {
    let local: std::net::SocketAddr = match nameserver {
        IpAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        IpAddr::V6(_) => ([0u16; 8], 0).into(),
    };
    let socket = UdpSocket::bind(local)?;
    socket.set_read_timeout(Some(QUERY_TIMEOUT))?;
    // `connect` on a `UdpSocket` doesn't perform a handshake (there is none
    // for UDP) — it just fixes the peer, so `send`/`recv` below can't be
    // handed a reply spoofed from an address other than the configured
    // nameserver's.
    socket.connect((nameserver, DNS_PORT))?;
    socket.send(query)?;

    let mut buf = vec![0u8; MAX_RESPONSE_BYTES];
    let n = socket.recv(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

struct Header {
    id: u16,
    flags: u16,
    qdcount: u16,
    ancount: u16,
}

fn parse_header(msg: &[u8]) -> Option<Header> {
    if msg.len() < 12 {
        return None;
    }
    Some(Header {
        id: u16::from_be_bytes([msg[0], msg[1]]),
        flags: u16::from_be_bytes([msg[2], msg[3]]),
        qdcount: u16::from_be_bytes([msg[4], msg[5]]),
        ancount: u16::from_be_bytes([msg[6], msg[7]]),
    })
}

/// Decode a domain name starting at `msg[start]`, following compression
/// pointers (RFC 1035 §4.1.4). Returns the decoded (dot-joined, not yet
/// sanitised) name and the offset immediately *after* this name in the
/// original, non-pointer-followed stream — which is where the caller
/// should continue reading the next field, and is generally not the same
/// place the pointer chain itself ended up.
///
/// Every pointer must target a strictly earlier offset than the pointer
/// itself; combined with [`MAX_LABEL_JUMPS`], this makes a pointer loop
/// structurally unrepresentable rather than merely guarded against.
fn decode_name(msg: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut pos = start;
    let mut jumps = 0usize;
    let mut end_pos: Option<usize> = None;
    let mut name_len = 0usize;

    loop {
        let len_byte = *msg.get(pos)?;

        if len_byte == 0 {
            if end_pos.is_none() {
                end_pos = Some(pos + 1);
            }
            break;
        }

        if len_byte & 0xc0 == 0xc0 {
            let low = *msg.get(pos + 1)?;
            let target = (((len_byte & 0x3f) as usize) << 8) | low as usize;
            if end_pos.is_none() {
                end_pos = Some(pos + 2);
            }
            jumps += 1;
            if jumps > MAX_LABEL_JUMPS || target >= pos {
                return None;
            }
            pos = target;
            continue;
        }

        if len_byte & 0xc0 != 0 {
            return None; // reserved bit pattern — malformed
        }

        let label_len = len_byte as usize;
        let label_bytes = msg.get(pos + 1..pos + 1 + label_len)?;
        // Lossy, not rejected: a hostile or misconfigured server could
        // send non-ASCII bytes, and this crate's rule is to degrade
        // gracefully on untrusted input, never panic or hard-fail on it —
        // same reasoning as `sysfs::read_to_string`'s lossy UTF-8.
        labels.push(String::from_utf8_lossy(label_bytes).into_owned());
        pos += 1 + label_len;

        name_len += label_len + 1;
        if labels.len() > 128 || name_len > MAX_NAME_LEN {
            return None;
        }
    }

    Some((labels.join("."), end_pos.unwrap_or(pos)))
}

/// Parse a response for `expected_id`, returning every PTR record found.
///
/// `RCODE != 0` (e.g. `NXDOMAIN`) is a valid, complete answer that just
/// happens to say "nothing here" — `Some(vec![])`, not `None`, since the
/// query genuinely succeeded. A mismatched id, a response that isn't
/// actually marked as a response (`QR` bit unset), or any structurally
/// malformed field is `None`: something is wrong enough that this
/// response shouldn't be trusted at all.
fn parse_ptr_response(msg: &[u8], expected_id: u16) -> Option<Vec<String>> {
    let header = parse_header(msg)?;
    if header.id != expected_id {
        return None;
    }
    let qr = (header.flags >> 15) & 1;
    if qr != 1 {
        return None;
    }
    let rcode = header.flags & 0x000f;
    if rcode != 0 {
        return Some(Vec::new());
    }

    let mut pos = 12usize;
    for _ in 0..header.qdcount {
        let (_, next) = decode_name(msg, pos)?;
        pos = next.checked_add(4)?; // QTYPE + QCLASS
    }

    let mut names = Vec::new();
    for _ in 0..header.ancount {
        let (_, next) = decode_name(msg, pos)?; // RR owner name
        pos = next;
        let rtype = u16::from_be_bytes([*msg.get(pos)?, *msg.get(pos + 1)?]);
        pos = pos.checked_add(2)?; // TYPE
        pos = pos.checked_add(2)?; // CLASS
        pos = pos.checked_add(4)?; // TTL
        let rdlength = u16::from_be_bytes([*msg.get(pos)?, *msg.get(pos + 1)?]) as usize;
        pos = pos.checked_add(2)?;

        if rtype == QTYPE_PTR {
            let (name, _) = decode_name(msg, pos)?;
            names.push(sanitize_kernel_string(&name, MAX_NAME_LEN));
        }

        pos = pos.checked_add(rdlength)?;
        if pos > msg.len() {
            return None;
        }
    }

    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::tests::TempTree;

    // ---- name encoding -------------------------------------------------------

    #[test]
    fn ptr_qname_reverses_ipv4_octets() {
        let addr: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(ptr_qname(addr), "8.8.8.8.in-addr.arpa");

        let addr: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(ptr_qname(addr), "4.3.2.1.in-addr.arpa");
    }

    #[test]
    fn ptr_qname_reverses_ipv6_nibbles() {
        let addr: IpAddr = "::1".parse().unwrap();
        let name = ptr_qname(addr);
        assert!(name.ends_with(".ip6.arpa"));
        assert!(name.starts_with("1.0.0.0."), "{name}");
    }

    #[test]
    fn encode_qname_length_prefixes_each_label_and_terminates_with_root() {
        let encoded = encode_qname("a.bc.arpa");
        assert_eq!(
            encoded,
            vec![1, b'a', 2, b'b', b'c', 4, b'a', b'r', b'p', b'a', 0]
        );
    }

    // ---- decode_name ----------------------------------------------------------

    #[test]
    fn decode_name_reads_a_plain_uncompressed_name() {
        let msg = encode_qname("dns.google");
        let (name, end) = decode_name(&msg, 0).expect("decodes");
        assert_eq!(name, "dns.google");
        assert_eq!(end, msg.len());
    }

    #[test]
    fn decode_name_follows_a_compression_pointer() {
        // A name at offset 0, then a second name elsewhere that's just a
        // pointer back to offset 0.
        let mut msg = encode_qname("dns.google");
        let pointer_offset = msg.len();
        msg.extend_from_slice(&[0xc0, 0x00]); // pointer to offset 0

        let (name, end) = decode_name(&msg, pointer_offset).expect("decodes");
        assert_eq!(name, "dns.google");
        // The pointer itself is 2 bytes; `end` must reflect that, not
        // wherever the pointer chain wound up.
        assert_eq!(end, pointer_offset + 2);
    }

    #[test]
    fn decode_name_rejects_a_self_referencing_pointer_loop() {
        // A pointer at offset 0 pointing at itself must not hang or panic.
        let msg = vec![0xc0, 0x00];
        assert_eq!(decode_name(&msg, 0), None);
    }

    #[test]
    fn decode_name_rejects_a_forward_pointer() {
        // Pointer at offset 0 pointing forward to offset 4 — forbidden
        // regardless of what's actually at offset 4.
        let mut msg = vec![0xc0, 0x04];
        msg.extend_from_slice(&encode_qname("x"));
        assert_eq!(decode_name(&msg, 0), None);
    }

    #[test]
    fn decode_name_on_truncated_input_is_none_not_a_panic() {
        assert_eq!(decode_name(&[], 0), None);
        assert_eq!(decode_name(&[5, b'h', b'e'], 0), None); // label longer than remaining bytes
        assert_eq!(decode_name(&[0xc0], 0), None); // pointer with no second byte
    }

    // ---- full response parsing --------------------------------------------------

    /// Build a syntactically real DNS response with one PTR answer, reusing
    /// this module's own encoding functions rather than hand-transcribed
    /// hex — the point is to prove `parse_ptr_response` correctly walks a
    /// well-formed message end to end, compression pointer included, not to
    /// pin an exact byte sequence by hand (error-prone to transcribe and
    /// tells you nothing extra once it's transcribed correctly).
    fn build_fake_response(id: u16, question_name: &str, answer_name: &str) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(&id.to_be_bytes());
        msg.extend_from_slice(&0x8180u16.to_be_bytes()); // QR=1, RD=1, RA=1, RCODE=0
        msg.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        msg.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());

        let question_start = msg.len();
        msg.extend_from_slice(&encode_qname(question_name));
        msg.extend_from_slice(&QTYPE_PTR.to_be_bytes());
        msg.extend_from_slice(&QCLASS_IN.to_be_bytes());

        // Answer: owner name is a compression pointer back to the question.
        msg.extend_from_slice(&[0xc0, question_start as u8]);
        msg.extend_from_slice(&QTYPE_PTR.to_be_bytes());
        msg.extend_from_slice(&QCLASS_IN.to_be_bytes());
        msg.extend_from_slice(&300u32.to_be_bytes()); // TTL
        let rdata = encode_qname(answer_name);
        msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        msg.extend_from_slice(&rdata);

        msg
    }

    #[test]
    fn parses_a_real_shaped_ptr_response() {
        let msg = build_fake_response(1234, "8.8.8.8.in-addr.arpa", "dns.google");
        let names = parse_ptr_response(&msg, 1234).expect("parses");
        assert_eq!(names, vec!["dns.google".to_string()]);
    }

    #[test]
    fn a_mismatched_id_is_rejected() {
        let msg = build_fake_response(1234, "8.8.8.8.in-addr.arpa", "dns.google");
        assert_eq!(parse_ptr_response(&msg, 9999), None);
    }

    #[test]
    fn a_query_message_not_marked_as_a_response_is_rejected() {
        // QR bit unset — this is what build_query itself produces, and
        // parse_ptr_response must never mistake an echoed query for an
        // answer.
        let query = build_query(1234, "8.8.8.8.in-addr.arpa");
        assert_eq!(parse_ptr_response(&query, 1234), None);
    }

    #[test]
    fn an_nxdomain_style_rcode_is_an_empty_result_not_none() {
        let mut msg = Vec::new();
        msg.extend_from_slice(&1234u16.to_be_bytes());
        msg.extend_from_slice(&0x8183u16.to_be_bytes()); // QR=1, RCODE=3 (NXDOMAIN)
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&encode_qname("nope.in-addr.arpa"));
        msg.extend_from_slice(&QTYPE_PTR.to_be_bytes());
        msg.extend_from_slice(&QCLASS_IN.to_be_bytes());

        assert_eq!(parse_ptr_response(&msg, 1234), Some(Vec::new()));
    }

    #[test]
    fn a_hostile_ptr_name_is_sanitised() {
        // A malicious or misconfigured server naming a host with a raw
        // ANSI escape must not carry it through — same rule as every other
        // kernel/network-supplied string in this crate.
        let msg = build_fake_response(1234, "1.0.0.127.in-addr.arpa", "evil\u{1b}[2Jhost");
        let names = parse_ptr_response(&msg, 1234).expect("parses");
        assert_eq!(names.len(), 1);
        assert!(!names[0].contains('\u{1b}'), "{:?}", names[0]);
        assert!(names[0].contains("evilhost") || names[0].contains("evil"), "{:?}", names[0]);
    }

    // ---- read_nameserver --------------------------------------------------------

    #[test]
    fn reads_the_first_nameserver_line() {
        let tree = TempTree::new("dns-resolv-conf");
        tree.file(
            "etc/resolv.conf",
            "# comment\nnameserver 8.8.8.8\nnameserver 1.1.1.1\n",
        );
        let reader = tree.reader();
        assert_eq!(read_nameserver(&reader), Some("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn missing_resolv_conf_is_none_not_a_panic() {
        let tree = TempTree::new("dns-no-resolv-conf");
        let reader = tree.reader();
        assert_eq!(read_nameserver(&reader), None);
    }

    #[test]
    fn a_resolv_conf_with_no_nameserver_line_is_none() {
        let tree = TempTree::new("dns-resolv-conf-empty");
        tree.file("etc/resolv.conf", "search example.com\noptions timeout:2\n");
        let reader = tree.reader();
        assert_eq!(read_nameserver(&reader), None);
    }

    // ---- end-to-end: real network, best-effort -----------------------------------

    /// A genuine round trip against whatever resolver this machine is
    /// actually configured to use, resolving a well-known public IP
    /// (`8.8.8.8`, Google's public DNS) whose PTR record has been stable
    /// for years. Skipped, not failed, if this sandbox has no usable
    /// resolver or no outbound network access — `resolve_ptr`'s whole
    /// contract is "best-effort, `None` on any failure," so a `None` here
    /// is not distinguishable from "network unavailable in this
    /// environment" and asserting on it would make the test flaky for the
    /// wrong reason.
    #[test]
    fn resolve_ptr_against_a_real_resolver_is_plausible_when_reachable() {
        let reader = SysfsReader::new();
        match resolve_ptr(&reader, "8.8.8.8".parse().unwrap()) {
            Some(names) if !names.is_empty() => {
                assert!(
                    names.iter().any(|n| n.contains("dns.google")),
                    "unexpected PTR result for 8.8.8.8: {names:?}"
                );
            }
            _ => {
                // No network, no resolver, or a resolver that didn't
                // answer in time in this sandbox — acceptable, not a
                // test failure.
            }
        }
    }
}
