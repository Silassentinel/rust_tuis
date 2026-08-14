//! Process ↔ port ↔ remote-endpoint mapping.
//!
//! Sources:
//! - `proc/net/tcp`, `proc/net/tcp6`, `proc/net/udp`, `proc/net/udp6` — every
//!   socket on the machine, hex-encoded local/remote address:port, TCP
//!   state, owning `uid`, and socket `inode`. Standard kernel ABI, one
//!   independently-optional file per protocol/address-family (a kernel
//!   built without IPv6 has no `tcp6`/`udp6` at all — that's not an error).
//! - `proc/<pid>/fd/*` symlinks (target `socket:[inode]`) and `proc/<pid>/comm`
//!   — correlates a socket's inode to the process that owns it, via the new
//!   [`crate::sysfs::SysfsReader::read_link`] primitive.
//!
//! # Scope: "connecting to and from the internet"
//!
//! Only connections whose remote address is public are kept — private
//! (RFC1918), loopback, link-local, unspecified, multicast, and broadcast
//! addresses are filtered by [`is_public_ip`], and bare listening sockets
//! (no peer yet: remote port `0`) are excluded entirely. This is a smaller
//! set than "every socket on the machine" on purpose: LAN chatter and
//! listening sockets are not what "talking to the internet" means, and
//! cutting them here keeps the `/proc/<pid>` walk below proportional to
//! what's actually being asked for, not to the whole process table.
//!
//! # Why the process walk doesn't touch every PID's every fd
//!
//! Naively walking all of `/proc/<pid>/fd` for every process, every
//! refresh, is a real cost on a busy machine — easily thousands of
//! `readlink` calls a second at a 1s interval. [`resolve_owners`] avoids
//! that by construction: it first collects the exact set of socket inodes
//! this refresh actually needs (from the filtered connection list, not
//! from every socket on the machine), then stops walking further PIDs the
//! moment every one of them has been matched. A PID owned by another user
//! fails `list_dir`/`read_link` with `EACCES`, which the fail-soft path
//! already turns into "this connection's owner is unknown" rather than an
//! error — the connection's `uid` (read directly from `proc/net/*`, no
//! `/proc/<pid>` access needed) is still shown either way, so it's never
//! fully anonymous.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use crate::collector::Collector;
use crate::error::{Error, Result};
use crate::sample::{ConnProtocol, Connection, ConnectionSample, Snapshot, TcpState};
use crate::sysfs::{sanitize_kernel_string, SysfsReader, DEFAULT_MAX_LINES};

pub const NAME: &str = "connections";

const PROC_TCP: &str = "proc/net/tcp";
const PROC_TCP6: &str = "proc/net/tcp6";
const PROC_UDP: &str = "proc/net/udp";
const PROC_UDP6: &str = "proc/net/udp6";

/// Cap on `proc/<pid>/comm`, same order of magnitude as the label caps
/// elsewhere in this crate — real program names (`firefox`, `sshd`,
/// `node`) are nowhere close to this.
const MAX_PROGRAM_NAME_LEN: usize = 64;

#[derive(Debug, Default)]
pub struct ConnectionsCollector;

impl ConnectionsCollector {
    pub fn new() -> Self {
        ConnectionsCollector
    }
}

impl Collector for ConnectionsCollector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self, reader: &SysfsReader) -> bool {
        reader.exists(Path::new(PROC_TCP))
    }

    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
        let mut raw: Vec<(ConnProtocol, RawSocket)> = Vec::new();

        for (path, protocol) in [
            (PROC_TCP, ConnProtocol::Tcp),
            (PROC_TCP6, ConnProtocol::Tcp),
            (PROC_UDP, ConnProtocol::Udp),
            (PROC_UDP6, ConnProtocol::Udp),
        ] {
            // Absent is routine here (no tcp6/udp6 on an IPv4-only kernel)
            // — the other three files still get read.
            if let Ok(contents) = reader.read_to_string(Path::new(path))? {
                for entry in parse_proc_net(Path::new(path), &contents)? {
                    if is_interesting(&entry) {
                        raw.push((protocol, entry));
                    }
                }
            }
        }

        let needed_inodes: HashSet<u64> = raw.iter().map(|(_, e)| e.inode).collect();
        let owners = resolve_owners(reader, &needed_inodes);

        let connections = raw
            .into_iter()
            .map(|(protocol, entry)| {
                let (pid, program, ppid) = owners
                    .get(&entry.inode)
                    .cloned()
                    .unwrap_or((None, None, None));
                Connection {
                    protocol,
                    local_addr: entry.local_addr,
                    local_port: entry.local_port,
                    remote_addr: entry.remote_addr,
                    remote_port: entry.remote_port,
                    state: match protocol {
                        ConnProtocol::Tcp => Some(TcpState::from_byte(entry.state_byte)),
                        ConnProtocol::Udp => None,
                    },
                    uid: entry.uid,
                    pid,
                    program,
                    ppid,
                }
            })
            .collect();

        snapshot.connections = Some(ConnectionSample { connections });
        Ok(())
    }
}

/// One line of `proc/net/{tcp,tcp6,udp,udp6}`, before filtering.
#[derive(Debug)]
struct RawSocket {
    local_addr: IpAddr,
    local_port: u16,
    remote_addr: IpAddr,
    remote_port: u16,
    state_byte: u8,
    uid: u32,
    inode: u64,
}

/// Parse a whole `proc/net/*` file.
///
/// One coherent kernel-formatted table — same reasoning as
/// `collectors::disk`'s `/proc/diskstats` — so a malformed line is a hard
/// `Error::Parse` that fails the whole file for this refresh, not a
/// per-line skip. The header line is always present and always discarded
/// unconditionally, matching the fixed format the kernel has used for this
/// file for decades.
fn parse_proc_net(path: &Path, contents: &str) -> Result<Vec<RawSocket>> {
    let mut out = Vec::new();

    for (idx, line) in contents.lines().enumerate().skip(1).take(DEFAULT_MAX_LINES) {
        if line.trim().is_empty() {
            continue;
        }
        out.push(parse_proc_net_line(path, idx + 1, line)?);
    }

    Ok(out)
}

fn parse_proc_net_line(path: &Path, line_no: usize, line: &str) -> Result<RawSocket> {
    let bad_field = |field: &str| Error::parse(path, Some(line_no), format!("line has no {field} field"));

    let mut fields = line.split_whitespace();
    let _sl = fields.next().ok_or_else(|| bad_field("sl"))?;
    let local = fields.next().ok_or_else(|| bad_field("local_address"))?;
    let remote = fields.next().ok_or_else(|| bad_field("rem_address"))?;
    let state_hex = fields.next().ok_or_else(|| bad_field("st"))?;
    let _tx_rx_queue = fields.next().ok_or_else(|| bad_field("tx_queue:rx_queue"))?;
    let _tr_tm_when = fields.next().ok_or_else(|| bad_field("tr:tm->when"))?;
    let _retrnsmt = fields.next().ok_or_else(|| bad_field("retrnsmt"))?;
    let uid_str = fields.next().ok_or_else(|| bad_field("uid"))?;
    let _timeout = fields.next().ok_or_else(|| bad_field("timeout"))?;
    let inode_str = fields.next().ok_or_else(|| bad_field("inode"))?;
    // Remaining fields (refcount, socket pointer, retransmit backoff, ...)
    // vary across kernel versions and aren't stored — nothing in
    // `Connection` uses them.

    let (local_addr, local_port) = parse_hex_addr_port(local)
        .ok_or_else(|| Error::parse(path, Some(line_no), format!("unparseable local address {local:?}")))?;
    let (remote_addr, remote_port) = parse_hex_addr_port(remote)
        .ok_or_else(|| Error::parse(path, Some(line_no), format!("unparseable remote address {remote:?}")))?;
    let state_byte = u8::from_str_radix(state_hex, 16)
        .map_err(|_| Error::parse(path, Some(line_no), format!("state {state_hex:?} is not hex")))?;
    let uid = uid_str
        .parse::<u32>()
        .map_err(|_| Error::parse(path, Some(line_no), format!("uid {uid_str:?} is not a number")))?;
    let inode = inode_str
        .parse::<u64>()
        .map_err(|_| Error::parse(path, Some(line_no), format!("inode {inode_str:?} is not a number")))?;

    Ok(RawSocket {
        local_addr,
        local_port,
        remote_addr,
        remote_port,
        state_byte,
        uid,
        inode,
    })
}

/// Decode one `hex_address:hex_port` field.
///
/// IPv4 is 8 hex chars (one 32-bit word); IPv6 is 32 (four 32-bit words).
/// Each word is stored byte-reversed (the kernel writes it as a native
/// `u32`, and this machine's byte order is little-endian) — `"0100007F"`
/// reversed byte-by-byte is `7F 00 00 01`, i.e. `127.0.0.1`. The port is
/// **not** reversed: `"0050"` is `80` directly.
fn parse_hex_addr_port(field: &str) -> Option<(IpAddr, u16)> {
    let (addr_hex, port_hex) = field.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    let addr = parse_hex_addr(addr_hex)?;
    Some((addr, port))
}

fn parse_hex_addr(hex: &str) -> Option<IpAddr> {
    let bytes = decode_hex_bytes(hex)?;
    match bytes.len() {
        4 => {
            let mut word = [0u8; 4];
            word.copy_from_slice(&bytes);
            word.reverse();
            Some(IpAddr::V4(Ipv4Addr::from(word)))
        }
        16 => {
            let mut out = [0u8; 16];
            for (chunk_idx, chunk) in bytes.chunks_exact(4).enumerate() {
                let mut word = [0u8; 4];
                word.copy_from_slice(chunk);
                word.reverse();
                out[chunk_idx * 4..chunk_idx * 4 + 4].copy_from_slice(&word);
            }
            Some(IpAddr::V6(Ipv6Addr::from(out)))
        }
        _ => None,
    }
}

fn decode_hex_bytes(hex: &str) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Should this socket be kept at all?
///
/// A remote port of `0` means "no peer" — a listening or otherwise
/// unconnected socket, not a connection to anywhere. Combined with
/// [`is_public_ip`], this is the whole "connecting to and from the
/// internet" filter this module's own doc describes.
fn is_interesting(entry: &RawSocket) -> bool {
    entry.remote_port != 0 && is_public_ip(&entry.remote_addr)
}

/// Excludes private (RFC1918), loopback, link-local, unspecified,
/// multicast, and (IPv4) broadcast/documentation addresses.
///
/// Hand-rolled octet checks for IPv6 rather than relying on `std`'s
/// still-unstable `Ipv6Addr` range predicates (`is_unique_local` and
/// friends were nightly-only for a long time) — `fe80::/10` (link-local)
/// and `fc00::/7` (unique local, the IPv6 analogue of RFC1918) are simple
/// enough to check directly and keep this fully on stable `std`.
pub fn is_public_ip(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation())
        }
        IpAddr::V6(v6) => {
            let octets = v6.octets();
            let link_local = octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80;
            let unique_local = (octets[0] & 0xfe) == 0xfc;
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || link_local || unique_local)
        }
    }
}

/// Correlate socket inodes to (pid, program name, parent pid), stopping as
/// soon as every inode in `needed` has been matched. See this module's own
/// doc for why this doesn't just walk every process's every fd
/// unconditionally.
type Owner = (Option<u32>, Option<String>, Option<u32>);

fn resolve_owners(reader: &SysfsReader, needed: &HashSet<u64>) -> HashMap<u64, Owner> {
    let mut found: HashMap<u64, Owner> = HashMap::new();
    if needed.is_empty() {
        return found;
    }
    let mut remaining: HashSet<u64> = needed.clone();

    let Ok(Ok(pids)) = reader.list_dir(Path::new("proc"), &is_ascii_digits) else {
        return found;
    };

    for pid_str in pids {
        if remaining.is_empty() {
            break;
        }
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };

        let fd_dir = format!("proc/{pid_str}/fd");
        // EACCES here — another user's process — is exactly the fail-soft
        // case this module's doc describes: that PID contributes nothing,
        // not an error.
        let Ok(Ok(fds)) = reader.list_dir(Path::new(&fd_dir), &is_ascii_digits) else {
            continue;
        };

        let mut matched_this_pid: Vec<u64> = Vec::new();
        for fd in &fds {
            let link_path = format!("{fd_dir}/{fd}");
            let Ok(Ok(target)) = reader.read_link(Path::new(&link_path)) else {
                continue;
            };
            if let Some(inode) = parse_socket_inode(&target) {
                if remaining.contains(&inode) {
                    matched_this_pid.push(inode);
                }
            }
        }

        if matched_this_pid.is_empty() {
            continue;
        }

        let program = read_program_name(reader, &pid_str);
        let ppid = read_parent_pid(reader, &pid_str);
        for inode in matched_this_pid {
            remaining.remove(&inode);
            found.insert(inode, (Some(pid), program.clone(), ppid));
        }
    }

    found
}

fn is_ascii_digits(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

/// `/proc/<pid>/fd/N`'s target for a socket fd is exactly `socket:[<inode>]`
/// — any other target (a real file, a pipe, an anonymous inode) means this
/// fd isn't a socket at all.
fn parse_socket_inode(target: &str) -> Option<u64> {
    target.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

fn read_program_name(reader: &SysfsReader, pid: &str) -> Option<String> {
    let path = format!("proc/{pid}/comm");
    match reader.read_first_line(Path::new(&path)) {
        Ok(Ok(name)) => Some(sanitize_kernel_string(&name, MAX_PROGRAM_NAME_LEN)),
        _ => None,
    }
}

/// The owning process's parent pid, from `proc/<pid>/status`'s `PPid:`
/// line — used by the UI to group subprocesses under their parent. Fails
/// soft to `None` (unreadable file, missing `PPid:` line, or a non-numeric
/// value) exactly like `read_program_name` does for `comm`.
fn read_parent_pid(reader: &SysfsReader, pid: &str) -> Option<u32> {
    let path = format!("proc/{pid}/status");
    let contents = match reader.read_to_string(Path::new(&path)) {
        Ok(Ok(s)) => s,
        _ => return None,
    };
    contents
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|rest| rest.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> &'static Path {
        Path::new("proc/net/tcp")
    }

    // ---- address/port decoding -------------------------------------------------

    #[test]
    fn decodes_ipv4_address_and_port() {
        let (addr, port) = parse_hex_addr_port("0100007F:0050").expect("parses");
        assert_eq!(addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(port, 80);
    }

    #[test]
    fn decodes_a_real_public_ipv4_address() {
        // 8.8.8.8 (a real, memorable public IP), reversed byte-by-byte per
        // word: 08 08 08 08 stays 08 08 08 08 either way, so use an
        // asymmetric one to actually prove the byte order: 1.2.3.4.
        let (addr, port) = parse_hex_addr_port("04030201:01BB").expect("parses");
        assert_eq!(addr, IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
        assert_eq!(port, 443);
    }

    /// `2001:4860:4860::8888` (a real, memorable public IPv6 address),
    /// encoded the way `/proc/net/tcp6` actually writes it: split into four
    /// 32-bit words (`2001:4860`, `4860:0000`, `0000:0000`, `0000:8888`),
    /// each word's four bytes individually reversed.
    #[test]
    fn decodes_ipv6_address() {
        let (addr, port) =
            parse_hex_addr_port("60480120000060480000000088880000:0050").expect("parses");
        assert_eq!(addr, "2001:4860:4860::8888".parse::<IpAddr>().unwrap());
        assert_eq!(port, 80);
    }

    #[test]
    fn unparseable_addresses_are_none_not_a_panic() {
        assert_eq!(parse_hex_addr_port("notanaddress"), None);
        assert_eq!(parse_hex_addr_port("0100007F"), None); // no port
        assert_eq!(parse_hex_addr_port("XX:0050"), None); // bad hex, odd length too
        assert_eq!(parse_hex_addr_port("010:0050"), None); // odd-length hex
    }

    // ---- is_public_ip -----------------------------------------------------------

    #[test]
    fn private_and_special_ipv4_ranges_are_excluded() {
        for ip in [
            "10.0.0.1", "172.16.0.1", "192.168.1.1", // RFC1918
            "127.0.0.1",     // loopback
            "169.254.1.1",   // link-local
            "0.0.0.0",       // unspecified
            "224.0.0.1",     // multicast
            "255.255.255.255", // broadcast
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(!is_public_ip(&addr), "{ip} should not be public");
        }
    }

    #[test]
    fn a_real_public_ipv4_address_is_public() {
        let addr: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(is_public_ip(&addr));
    }

    #[test]
    fn private_and_special_ipv6_ranges_are_excluded() {
        for ip in ["::1", "::", "fe80::1", "fc00::1", "ff02::1"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(!is_public_ip(&addr), "{ip} should not be public");
        }
    }

    #[test]
    fn a_real_public_ipv6_address_is_public() {
        let addr: IpAddr = "2001:4860:4860::8888".parse().unwrap();
        assert!(is_public_ip(&addr));
    }

    // ---- is_interesting -----------------------------------------------------------

    fn socket(remote_addr: &str, remote_port: u16) -> RawSocket {
        RawSocket {
            local_addr: "0.0.0.0".parse().unwrap(),
            local_port: 12345,
            remote_addr: remote_addr.parse().unwrap(),
            remote_port,
            state_byte: 0x01,
            uid: 1000,
            inode: 999,
        }
    }

    #[test]
    fn a_listening_socket_with_no_peer_is_not_interesting() {
        assert!(!is_interesting(&socket("0.0.0.0", 0)));
    }

    #[test]
    fn a_connection_to_a_public_address_is_interesting() {
        assert!(is_interesting(&socket("8.8.8.8", 443)));
    }

    #[test]
    fn a_connection_to_a_private_address_is_not_interesting() {
        assert!(!is_interesting(&socket("192.168.1.1", 443)));
    }

    // ---- parse_proc_net -----------------------------------------------------------

    /// A trimmed real capture shape: header line plus a handful of TCP
    /// entries, one of them a connection to a public address.
    const REAL_TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:8AE1 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 55123 1 0000000000000000 20 0 0 10 -1
   1: 0A00A8C0:C71C 08080808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 55124 1 0000000000000000 20 0 0 10 -1
   2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 55125 1 0000000000000000 20 0 0 10 -1
";

    #[test]
    fn parses_a_real_tcp_table() {
        let entries = parse_proc_net(p(), REAL_TCP).expect("real fixture parses");
        assert_eq!(entries.len(), 3);

        // Row 0: 127.0.0.1:35553 -> 127.0.0.1:8080, established.
        assert_eq!(entries[0].local_port, 0x8AE1);
        assert_eq!(entries[0].remote_port, 0x1F90);
        assert_eq!(entries[0].state_byte, 0x01);
        assert_eq!(entries[0].uid, 1000);
        assert_eq!(entries[0].inode, 55123);

        // Row 1: a real public remote address (8.8.8.8:443).
        assert_eq!(entries[1].remote_addr, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
        assert_eq!(entries[1].remote_port, 443);

        // Row 2: a listening socket (remote 0.0.0.0:0, state LISTEN).
        assert_eq!(entries[2].remote_port, 0);
        assert_eq!(entries[2].state_byte, 0x0a);
    }

    #[test]
    fn the_header_line_is_always_skipped() {
        let entries = parse_proc_net(p(), "  sl  local_address rem_address   st ...\n").expect("parses");
        assert!(entries.is_empty());
    }

    #[test]
    fn a_truncated_line_is_a_parse_error_not_a_panic() {
        let stat = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:8AE1 0100007F:1F90 01\n";
        let err = parse_proc_net(p(), stat).expect_err("truncated line must fail");
        assert!(matches!(err, Error::Parse { line: Some(2), .. }), "got {err:?}");
    }

    #[test]
    fn an_unparseable_address_is_a_parse_error() {
        let stat = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: garbage 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 55123\n";
        let err = parse_proc_net(p(), stat).expect_err("garbage address must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn blank_lines_are_skipped() {
        let entries = parse_proc_net(p(), &format!("{REAL_TCP}\n\n")).expect("blank trailing lines are fine");
        assert_eq!(entries.len(), 3);
    }

    // ---- parse_socket_inode -----------------------------------------------------

    #[test]
    fn parses_a_real_socket_fd_target() {
        assert_eq!(parse_socket_inode("socket:[12345]"), Some(12345));
    }

    #[test]
    fn non_socket_fd_targets_are_none() {
        assert_eq!(parse_socket_inode("/dev/null"), None);
        assert_eq!(parse_socket_inode("pipe:[6789]"), None);
        assert_eq!(parse_socket_inode("anon_inode:[eventfd]"), None);
    }

    // ---- read_parent_pid ---------------------------------------------------------

    use crate::sysfs::tests::TempTree;

    #[test]
    fn read_parent_pid_finds_the_ppid_line() {
        let tree = TempTree::new("read-parent-pid-happy");
        tree.file("proc/99/status", "Name:\tsh\nState:\tS\nPPid:\t1\nUid:\t0\t0\t0\t0\n");
        assert_eq!(read_parent_pid(&tree.reader(), "99"), Some(1));
    }

    #[test]
    fn read_parent_pid_is_none_when_status_is_missing() {
        let tree = TempTree::new("read-parent-pid-missing");
        assert_eq!(read_parent_pid(&tree.reader(), "99"), None);
    }

    #[test]
    fn read_parent_pid_is_none_when_the_ppid_line_is_malformed() {
        let tree = TempTree::new("read-parent-pid-malformed");
        tree.file("proc/99/status", "Name:\tsh\nPPid:\tnot-a-number\n");
        assert_eq!(read_parent_pid(&tree.reader(), "99"), None);
    }

    #[test]
    fn read_parent_pid_is_none_when_there_is_no_ppid_line_at_all() {
        let tree = TempTree::new("read-parent-pid-absent-field");
        tree.file("proc/99/status", "Name:\tsh\nState:\tS\n");
        assert_eq!(read_parent_pid(&tree.reader(), "99"), None);
    }

    // ---- end-to-end: collect() against a fixture tree ---------------------------

    fn tcp_header() -> &'static str {
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
    }

    /// The full pipeline: a fixture `/proc` with one real public connection
    /// (owned by pid 42, matched via its `fd/3` symlink) and one owned by a
    /// pid whose `fd` directory isn't readable (simulating another user's
    /// process) — proving both the happy path and the fail-soft path
    /// through `collect()` itself, not just the parsing helpers in
    /// isolation.
    #[test]
    fn collect_correlates_a_connection_to_its_owning_process() {
        let tree = TempTree::new("connections-e2e");
        let tcp = format!(
            "{}   0: 0A00A8C0:C71C 08080808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 999999 1\n",
            tcp_header()
        );
        tree.file("proc/net/tcp", &tcp);

        tree.dir("proc/42/fd");
        tree.symlink("proc/42/fd/3", Path::new("socket:[999999]"));
        tree.file("proc/42/comm", "curl\n");
        tree.file("proc/42/status", "Name:\tcurl\nState:\tS (sleeping)\nPPid:\t7\nUid:\t1000\t1000\t1000\t1000\n");

        let reader = tree.reader();
        let mut collector = ConnectionsCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&reader, &mut snapshot).expect("collect must not fail");

        let sample = snapshot.connections.expect("connections present");
        assert_eq!(sample.connections.len(), 1);
        let conn = &sample.connections[0];
        assert_eq!(conn.remote_addr, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
        assert_eq!(conn.remote_port, 443);
        assert_eq!(conn.pid, Some(42));
        assert_eq!(conn.program.as_deref(), Some("curl"));
        assert_eq!(conn.ppid, Some(7));
        assert_eq!(conn.uid, 1000);
    }

    /// No `proc/<pid>/status` file at all (present `comm`, absent `status`)
    /// — attribution still succeeds via `comm`, `ppid` just comes back
    /// `None`, same fail-soft story `program` already has independently of
    /// `pid`.
    #[test]
    fn collect_leaves_ppid_none_when_status_is_unreadable() {
        let tree = TempTree::new("connections-e2e-no-status");
        let tcp = format!(
            "{}   0: 0A00A8C0:C71C 08080808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 999999 1\n",
            tcp_header()
        );
        tree.file("proc/net/tcp", &tcp);
        tree.dir("proc/42/fd");
        tree.symlink("proc/42/fd/3", Path::new("socket:[999999]"));
        tree.file("proc/42/comm", "curl\n");
        // No `proc/42/status` at all.

        let reader = tree.reader();
        let mut collector = ConnectionsCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&reader, &mut snapshot).expect("collect must not fail");

        let sample = snapshot.connections.expect("connections present");
        assert_eq!(sample.connections[0].pid, Some(42));
        assert_eq!(sample.connections[0].ppid, None);
    }

    /// A connection whose owning process can't be walked (no matching
    /// `/proc/<pid>` at all here, simulating either a permission-denied
    /// walk or a process that exited between the two reads) still shows up
    /// with its `uid` intact — never fully anonymous.
    #[test]
    fn collect_is_fail_soft_when_the_owning_process_cannot_be_attributed() {
        let tree = TempTree::new("connections-e2e-unattributed");
        let tcp = format!(
            "{}   0: 0A00A8C0:C71C 08080808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 999999 1\n",
            tcp_header()
        );
        tree.file("proc/net/tcp", &tcp);
        // No `proc/<pid>` directories at all — nothing to correlate against.

        let reader = tree.reader();
        let mut collector = ConnectionsCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&reader, &mut snapshot).expect("collect must not fail");

        let sample = snapshot.connections.expect("connections present");
        assert_eq!(sample.connections.len(), 1);
        assert_eq!(sample.connections[0].pid, None);
        assert_eq!(sample.connections[0].program, None);
        assert_eq!(sample.connections[0].uid, 1000, "uid still shown even when pid is not");
    }

    /// A private-address connection and a bare listening socket in the same
    /// file are both filtered out — only the one real internet-facing
    /// connection reaches the snapshot.
    #[test]
    fn collect_filters_out_private_and_listening_sockets() {
        let tree = TempTree::new("connections-e2e-filtered");
        let tcp = format!(
            "{header}   0: 0100007F:8AE1 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 1 1\n\
              1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1\n\
              2: 0A00A8C0:C71C 08080808:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 3 1\n",
            header = tcp_header()
        );
        tree.file("proc/net/tcp", &tcp);

        let reader = tree.reader();
        let mut collector = ConnectionsCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&reader, &mut snapshot).expect("collect must not fail");

        let sample = snapshot.connections.expect("connections present");
        assert_eq!(sample.connections.len(), 1, "{:?}", sample.connections);
        assert_eq!(sample.connections[0].remote_addr, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn probe_is_true_when_proc_net_tcp_exists() {
        let tree = TempTree::new("connections-probe");
        tree.file("proc/net/tcp", tcp_header());
        assert!(ConnectionsCollector::new().probe(&tree.reader()));
    }

    #[test]
    fn probe_is_false_when_proc_net_tcp_is_absent() {
        let tree = TempTree::new("connections-probe-absent");
        assert!(!ConnectionsCollector::new().probe(&tree.reader()));
    }
}
