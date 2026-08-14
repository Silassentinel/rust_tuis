//! On-demand route tracing (traceroute) for checked connections.
//!
//! Raw ICMP over a socket needing `CAP_NET_RAW` — see crate-checklist
//! proposal "rustmon D" (`docs/crate-checklist.md`) for why this is the
//! only real option on Linux: unprivileged "ping sockets" are disabled by
//! default on this project's own target machine
//! (`net.ipv4.ping_group_range` is `1 0`), and whether they even see
//! intermediate-hop replies the way a raw socket does was never confirmed.
//! IPv4 only — ICMPv6 traceroute would be a second, separate
//! implementation, and every connection this hits already has an IPv4
//! remote address available or it wouldn't be worth building yet.
//!
//! # Fail-soft, not fail-hard
//!
//! [`trace`] returns `None` when the raw socket can't even be created
//! (`EPERM`/`EACCES` — no `CAP_NET_RAW`), the same "some capability isn't
//! present, report absence" rule every other permission-gated read in this
//! crate follows. It never suggests running as root; the caller (`ui::app`)
//! surfaces this as "unavailable," not an error.
//!
//! # Bounded
//!
//! [`PROBE_TIMEOUT`] bounds each hop; [`TRACE_BUDGET`] bounds the whole
//! trace regardless of hop count, so a route to an unreachable destination
//! can't run indefinitely. A hop with no reply inside its own timeout is
//! recorded as `None` — a real, meaningful gap in the route (a firewall
//! dropping ICMP, a router that doesn't decrement-and-reply), not something
//! to silently omit.
//!
//! # Untrusted input
//!
//! An ICMP reply is network-supplied data, parsed with bounds-checked
//! `slice::get` throughout — no panics on a truncated, malformed, or
//! unrelated packet arriving on the same raw socket (which sees *all*
//! ICMP traffic to this host, not just replies to our own probes).

use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use nix::sys::socket::{
    recvfrom, sendto, setsockopt, socket, sockopt, AddressFamily, MsgFlags, SockFlag, SockProtocol,
    SockType, SockaddrIn,
};
use nix::sys::time::{TimeVal, TimeValLike};

/// RFC 792's conventional traceroute ceiling.
const MAX_HOPS: u8 = 30;
/// Per-hop wait for a reply before recording a silent hop.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Overall cap on the whole trace, regardless of hop count.
pub const TRACE_BUDGET: Duration = Duration::from_secs(18);

/// One hop's result. `None` means no reply arrived within
/// [`PROBE_TIMEOUT`] — shown by the UI as `*`, the standard traceroute
/// convention, not omitted.
pub type Hop = Option<Ipv4Addr>;

/// Trace the route to `dest`.
///
/// `None` means the raw socket itself couldn't be created — no
/// `CAP_NET_RAW`, the fail-soft case this whole module exists to handle
/// gracefully. `Some(hops)` is the route as far as it got: it stops early
/// (rather than padding to [`MAX_HOPS`]) once a hop's reply matches `dest`
/// itself, so the caller can tell "reached the destination" from "ran out
/// of time/hops" by checking whether the last entry is `Some(dest)`.
pub fn trace(dest: Ipv4Addr) -> Option<Vec<Hop>> {
    let sock = socket(AddressFamily::Inet, SockType::Raw, SockFlag::empty(), SockProtocol::Icmp).ok()?;

    // Bounds each individual `recvfrom` below, not the whole trace — a hop
    // that never replies must not hang this thread forever.
    let _ = setsockopt(
        &sock,
        sockopt::ReceiveTimeout,
        &TimeVal::milliseconds(PROBE_TIMEOUT.as_millis() as i64),
    );

    // The process id doubles as the ICMP identifier: good enough to keep
    // this trace's probes distinguishable from another process's on the
    // same host, which is all it needs to do — there is no adversarial
    // party to defend against on a raw socket only this process reads.
    let ident = std::process::id() as u16;
    let deadline = Instant::now() + TRACE_BUDGET;
    let mut hops = Vec::new();

    for ttl in 1..=MAX_HOPS {
        if Instant::now() >= deadline {
            break;
        }

        let _ = setsockopt(&sock, sockopt::Ipv4Ttl, &(i32::from(ttl)));

        let seq = u16::from(ttl);
        let packet = build_echo_request(ident, seq);
        let target = SockaddrIn::new(
            dest.octets()[0],
            dest.octets()[1],
            dest.octets()[2],
            dest.octets()[3],
            0,
        );

        if sendto(sock.as_raw_fd(), &packet, &target, MsgFlags::empty()).is_err() {
            hops.push(None);
            continue;
        }

        let hop_deadline = Instant::now() + PROBE_TIMEOUT;
        let mut hop_result = None;
        while Instant::now() < hop_deadline {
            let mut buf = [0u8; 512];
            match recvfrom::<SockaddrIn>(sock.as_raw_fd(), &mut buf) {
                Ok((n, _)) => {
                    if let Some(reply_addr) = parse_icmp_reply(&buf[..n], ident, seq) {
                        hop_result = Some(reply_addr);
                        break;
                    }
                    // A real packet, just not a match for this probe (a
                    // stray unrelated ICMP message on the same raw
                    // socket, or a delayed reply to an earlier hop) —
                    // keep waiting out this hop's own deadline.
                }
                Err(_) => break, // SO_RCVTIMEO fired, or a hard I/O error.
            }
        }

        let reached_dest = hop_result == Some(dest);
        hops.push(hop_result);
        if reached_dest {
            break;
        }
    }

    Some(hops)
}

/// Build an ICMP Echo Request (RFC 792): 8-byte header
/// (type/code/checksum/identifier/sequence), no payload.
fn build_echo_request(ident: u16, seq: u16) -> Vec<u8> {
    let mut packet = vec![8, 0, 0, 0]; // type=8 (echo request), code=0, checksum placeholder
    packet.extend_from_slice(&ident.to_be_bytes());
    packet.extend_from_slice(&seq.to_be_bytes());

    let checksum = icmp_checksum(&packet);
    packet[2..4].copy_from_slice(&checksum.to_be_bytes());
    packet
}

/// RFC 1071 one's-complement checksum: sum all 16-bit big-endian words
/// (an odd trailing byte is treated as a word with a zero low byte), fold
/// carries back in, complement.
fn icmp_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for chunk in chunks.by_ref() {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if let [last] = *chunks.remainder() {
        sum += u32::from(u16::from_be_bytes([last, 0]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Does `buf` (a raw-ICMP-socket read, IP header included per `man 7 raw`)
/// contain a reply to our probe (`ident`/`seq`)? Handles both an Echo
/// Reply from the destination itself and a Time Exceeded from an
/// intermediate router (which embeds our original request's header, 8
/// bytes of it, inside its own payload — RFC 792).
///
/// Returns the replying router/host's address either way; the caller
/// distinguishes "reached the destination" from "an intermediate hop
/// replied" by comparing that address to `dest`.
fn parse_icmp_reply(buf: &[u8], ident: u16, seq: u16) -> Option<Ipv4Addr> {
    let ihl = usize::from(*buf.first()? & 0x0f) * 4;
    let src = buf.get(12..16)?;
    let source_addr = Ipv4Addr::new(src[0], src[1], src[2], src[3]);

    let icmp = buf.get(ihl..)?;
    let icmp_type = *icmp.first()?;
    let icmp_code = *icmp.get(1)?;

    let matched = match icmp_type {
        0 => icmp_ident_seq_at(icmp, 4) == Some((ident, seq)),
        11 if icmp_code == 0 => {
            let embedded = icmp.get(8..)?;
            let embedded_ihl = usize::from(*embedded.first()? & 0x0f) * 4;
            let embedded_icmp = embedded.get(embedded_ihl..)?;
            icmp_ident_seq_at(embedded_icmp, 4) == Some((ident, seq))
        }
        _ => false,
    };

    matched.then_some(source_addr)
}

/// Read a big-endian `(identifier, sequence)` pair starting at `offset` —
/// the shape both an Echo header and an embedded original-request header
/// share, just at a different offset within their respective buffers.
fn icmp_ident_seq_at(buf: &[u8], offset: usize) -> Option<(u16, u16)> {
    let ident = u16::from_be_bytes([*buf.get(offset)?, *buf.get(offset + 1)?]);
    let seq = u16::from_be_bytes([*buf.get(offset + 2)?, *buf.get(offset + 3)?]);
    Some((ident, seq))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- icmp_checksum ---------------------------------------------------------

    /// The defining property of a one's-complement Internet checksum: once
    /// the checksum field itself is filled in correctly, re-summing the
    /// *whole* buffer (checksum field included) always yields exactly
    /// zero. This is a self-verifying property rather than a hand-picked
    /// reference value, and it's what every real implementation of this
    /// algorithm relies on to validate a received packet.
    #[test]
    fn a_correctly_checksummed_buffer_sums_to_zero() {
        let packet = build_echo_request(0x1234, 0x0001);
        assert_eq!(icmp_checksum(&packet), 0);
    }

    #[test]
    fn checksum_of_an_odd_length_buffer_does_not_panic() {
        // Exercises the "trailing single byte" branch.
        let _ = icmp_checksum(&[1, 2, 3]);
    }

    #[test]
    fn checksum_of_an_empty_buffer_is_all_ones() {
        assert_eq!(icmp_checksum(&[]), 0xffff);
    }

    // ---- build_echo_request -----------------------------------------------------

    #[test]
    fn echo_request_has_the_right_shape() {
        let packet = build_echo_request(0xabcd, 0x0007);
        assert_eq!(packet[0], 8, "type must be Echo Request");
        assert_eq!(packet[1], 0, "code must be 0");
        assert_eq!(&packet[4..6], &0xabcdu16.to_be_bytes());
        assert_eq!(&packet[6..8], &0x0007u16.to_be_bytes());
        assert_eq!(packet.len(), 8);
    }

    // ---- parse_icmp_reply --------------------------------------------------------

    /// A minimal, syntactically real IPv4 header (20 bytes, no options)
    /// with the given source address, for building fake received packets
    /// without needing a real socket.
    fn fake_ip_header(src: Ipv4Addr) -> Vec<u8> {
        let mut header = vec![0u8; 20];
        header[0] = 0x45; // version 4, IHL 5 (20 bytes, no options)
        header[9] = 1; // protocol: ICMP
        header[12..16].copy_from_slice(&src.octets());
        header
    }

    #[test]
    fn recognises_a_matching_echo_reply() {
        let mut buf = fake_ip_header(Ipv4Addr::new(93, 184, 216, 34));
        buf.extend_from_slice(&[0, 0, 0, 0]); // type=0 (echo reply), code, checksum
        buf.extend_from_slice(&0x1234u16.to_be_bytes());
        buf.extend_from_slice(&0x0005u16.to_be_bytes());

        assert_eq!(
            parse_icmp_reply(&buf, 0x1234, 0x0005),
            Some(Ipv4Addr::new(93, 184, 216, 34))
        );
    }

    #[test]
    fn rejects_an_echo_reply_with_a_mismatched_sequence() {
        let mut buf = fake_ip_header(Ipv4Addr::new(1, 1, 1, 1));
        buf.extend_from_slice(&[0, 0, 0, 0]);
        buf.extend_from_slice(&0x1234u16.to_be_bytes());
        buf.extend_from_slice(&0x0005u16.to_be_bytes());

        assert_eq!(parse_icmp_reply(&buf, 0x1234, 0x0099), None);
    }

    /// The intermediate-hop case: a router's Time Exceeded message embeds
    /// our own original request (8 bytes of it) inside its payload, after
    /// its own 8-byte ICMP header and the embedded original IP header.
    #[test]
    fn recognises_a_matching_time_exceeded_from_an_intermediate_hop() {
        let mut buf = fake_ip_header(Ipv4Addr::new(10, 0, 0, 1)); // the router itself
        buf.extend_from_slice(&[11, 0, 0, 0]); // type=11 (time exceeded), code=0
        buf.extend_from_slice(&[0, 0, 0, 0]); // "unused" field

        // Embedded original packet: its own IP header, then our echo
        // request's 8-byte ICMP header.
        buf.extend_from_slice(&fake_ip_header(Ipv4Addr::new(93, 184, 216, 34)));
        buf.extend_from_slice(&build_echo_request(0x1234, 0x0005));

        assert_eq!(
            parse_icmp_reply(&buf, 0x1234, 0x0005),
            Some(Ipv4Addr::new(10, 0, 0, 1)),
            "must report the replying router's address, not the original destination"
        );
    }

    #[test]
    fn an_unrelated_icmp_type_is_not_a_match() {
        let mut buf = fake_ip_header(Ipv4Addr::new(1, 1, 1, 1));
        buf.extend_from_slice(&[3, 0, 0, 0]); // type=3, destination unreachable
        buf.extend_from_slice(&0x1234u16.to_be_bytes());
        buf.extend_from_slice(&0x0005u16.to_be_bytes());

        assert_eq!(parse_icmp_reply(&buf, 0x1234, 0x0005), None);
    }

    #[test]
    fn truncated_or_empty_buffers_are_none_not_a_panic() {
        assert_eq!(parse_icmp_reply(&[], 1, 1), None);
        assert_eq!(parse_icmp_reply(&[0x45], 1, 1), None);
        assert_eq!(parse_icmp_reply(&fake_ip_header(Ipv4Addr::UNSPECIFIED), 1, 1), None);
    }

    // ---- trace: fail-soft without CAP_NET_RAW ------------------------------------

    /// This crate's own test suite never runs with `CAP_NET_RAW` (nor
    /// should it need to, to be green) — so `trace` hitting `EPERM` on
    /// socket creation and returning `None` gracefully, rather than
    /// panicking or hanging, is a real exercise of the fail-soft path this
    /// module's whole design is built around, not a mock.
    #[test]
    fn trace_is_none_without_cap_net_raw() {
        // A loopback destination: if this environment unexpectedly *does*
        // have the capability (running as root), the assertion below still
        // holds — `Some` is an equally acceptable outcome, this test only
        // pins down that neither path panics or hangs.
        let result = trace(Ipv4Addr::new(127, 0, 0, 1));
        match result {
            None => {}                    // expected: no CAP_NET_RAW here
            Some(hops) => assert!(hops.len() <= usize::from(MAX_HOPS)),
        }
    }
}
