//! What Linux reads from `/proc/net` and `/sys/class/net`, read from Darwin's
//! kernel: the TCP ports something listens on, and which interfaces are
//! hardware.

use std::{collections::BTreeSet, ffi::CStr, io};

/// `IFT_ETHER` from `<net/if_types.h>`: wired and Wi-Fi interfaces alike.
const IFT_ETHER: u8 = 0x6;

/// The record kinds of `<netinet/in_pcb.h>` and `<sys/socketvar.h>` that
/// `net.inet.tcp.pcblist_n` gives for each connection, which ends once all
/// have been seen.
const XSO_SOCKET: u32 = 0x1;
const XSO_RCVBUF: u32 = 0x2;
const XSO_SNDBUF: u32 = 0x4;
const XSO_STATS: u32 = 0x8;
const XSO_INPCB: u32 = 0x10;
const XSO_TCPCB: u32 = 0x20;
const ALL_KINDS: u32 = XSO_SOCKET | XSO_RCVBUF | XSO_SNDBUF | XSO_STATS | XSO_INPCB | XSO_TCPCB;

/// `sizeof(struct xinpgen)`, which opens and closes the list.
const GENERATION_LENGTH: usize = 24;
/// `inp_lport` in `struct xinpcb_n`, in network order.
const LOCAL_PORT_OFFSET: usize = 18;
/// `t_state` in `struct xtcpcb_n`.
const STATE_OFFSET: usize = 36;
/// `TCPS_LISTEN` from `<netinet/tcp_fsm.h>`.
const TCPS_LISTEN: i32 = 1;

/// TCP ports something listens on, over IPv4 and IPv6 alike; none when the
/// kernel's table cannot be read.
pub(super) fn listening_ports() -> Vec<u16> {
    connection_table()
        .map(|table| parse_listening_ports(&table))
        .unwrap_or_default()
}

/// The raw `net.inet.tcp.pcblist_n` table.
fn connection_table() -> io::Result<Vec<u8>> {
    let name = c"net.inet.tcp.pcblist_n";
    // Connections come and go between sizing and reading; retry a few times
    // with room to spare before giving up.
    for _ in 0..4 {
        let mut length = 0;
        // SAFETY: a null buffer asks only for the size, written to `length`.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        length += length / 4;
        let mut table = vec![0_u8; length];
        // SAFETY: the buffer is writable for `length` bytes, and the kernel
        // writes no more than that, storing what it wrote in `length`.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                table.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            table.truncate(length);
            return Ok(table);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOMEM) {
            return Err(error);
        }
    }
    Err(io::Error::from_raw_os_error(libc::ENOMEM))
}

/// The local ports of the listening connections in a `pcblist_n` table.
///
/// The table is a `struct xinpgen`, a run of records each opening with its
/// length and kind and padded to eight bytes, and a closing `xinpgen`. Each
/// connection is one record of every kind, in no promised order.
fn parse_listening_ports(table: &[u8]) -> Vec<u16> {
    let u32_at = |offset: usize| -> Option<u32> {
        Some(u32::from_ne_bytes(
            table.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let mut ports = BTreeSet::new();
    let (mut seen, mut port, mut listening) = (0, None, false);
    let Some(mut offset) = u32_at(0).map(|length| round_up(length as usize)) else {
        return Vec::new();
    };
    while let (Some(length), Some(kind)) = (u32_at(offset), u32_at(offset + 4)) {
        let length = length as usize;
        let Some(record) = table.get(offset..offset + length) else {
            break;
        };
        // The closing xinpgen, or a malformed length.
        if length <= GENERATION_LENGTH {
            break;
        }
        match kind {
            XSO_INPCB => {
                port = record
                    .get(LOCAL_PORT_OFFSET..LOCAL_PORT_OFFSET + 2)
                    .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]));
            }
            XSO_TCPCB => {
                listening = record
                    .get(STATE_OFFSET..STATE_OFFSET + 4)
                    .is_some_and(|bytes| {
                        i32::from_ne_bytes(bytes.try_into().unwrap()) == TCPS_LISTEN
                    });
            }
            _ => {}
        }
        seen |= kind;
        if seen & ALL_KINDS == ALL_KINDS {
            if listening && let Some(port) = port {
                ports.insert(port);
            }
            (seen, port, listening) = (0, None, false);
        }
        offset += round_up(length);
    }
    ports.into_iter().collect()
}

/// `ROUNDUP64` from `<netinet/in_pcb.h>`.
fn round_up(length: usize) -> usize {
    length.next_multiple_of(8)
}

/// The names of the interfaces backed by hardware, as Ethernet and Wi-Fi
/// are, which leaves out bridges, tunnels and loopback. A virtual machine's
/// Ethernet adapter on this computer also presents as Ethernet, unlike on
/// Linux, where sysfs tells it apart; its private address may be offered.
pub(super) fn hardware_interfaces() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that freeifaddrs
    // releases; each entry is read before that. An AF_LINK address is a
    // sockaddr_dl.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return names;
        }
        let mut entry = list;
        while !entry.is_null() {
            let interface = &*entry;
            entry = interface.ifa_next;
            if interface.ifa_addr.is_null()
                || i32::from((*interface.ifa_addr).sa_family) != libc::AF_LINK
            {
                continue;
            }
            let link = &*(interface.ifa_addr as *const libc::sockaddr_dl);
            if link.sdl_type == IFT_ETHER {
                names.insert(
                    CStr::from_ptr(interface.ifa_name)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        libc::freeifaddrs(list);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One connection's records, with its local port and TCP state.
    fn connection(port: u16, state: i32, kinds: &[u32]) -> Vec<u8> {
        let mut records = Vec::new();
        for &kind in kinds {
            // Unpadded, as a record's length need not be a multiple of 8.
            const LENGTH: u32 = 52;
            let mut record = vec![0_u8; LENGTH as usize];
            record[..4].copy_from_slice(&LENGTH.to_ne_bytes());
            record[4..8].copy_from_slice(&kind.to_ne_bytes());
            if kind == XSO_INPCB {
                record[LOCAL_PORT_OFFSET..LOCAL_PORT_OFFSET + 2]
                    .copy_from_slice(&port.to_be_bytes());
            }
            if kind == XSO_TCPCB {
                record[STATE_OFFSET..STATE_OFFSET + 4].copy_from_slice(&state.to_ne_bytes());
            }
            record.resize(round_up(record.len()), 0);
            records.extend(record);
        }
        records
    }

    fn generation() -> Vec<u8> {
        let mut generation = vec![0_u8; GENERATION_LENGTH];
        generation[..4].copy_from_slice(&(GENERATION_LENGTH as u32).to_ne_bytes());
        generation
    }

    #[test]
    fn only_listening_connections_give_their_ports() {
        let every = [
            XSO_SOCKET, XSO_RCVBUF, XSO_SNDBUF, XSO_STATS, XSO_INPCB, XSO_TCPCB,
        ];
        let reordered = [
            XSO_TCPCB, XSO_STATS, XSO_INPCB, XSO_SOCKET, XSO_SNDBUF, XSO_RCVBUF,
        ];
        let established = 4;
        let table = [
            generation(),
            connection(22, TCPS_LISTEN, &every),
            connection(50_000, established, &every),
            connection(8080, TCPS_LISTEN, &reordered),
            generation(),
        ]
        .concat();
        assert_eq!(parse_listening_ports(&table), [22, 8080]);
    }

    #[test]
    fn a_truncated_table_gives_what_it_holds() {
        let every = [
            XSO_SOCKET, XSO_RCVBUF, XSO_SNDBUF, XSO_STATS, XSO_INPCB, XSO_TCPCB,
        ];
        let table = [
            generation(),
            connection(22, TCPS_LISTEN, &every),
            connection(23, TCPS_LISTEN, &every),
        ]
        .concat();
        assert_eq!(parse_listening_ports(&table[..table.len() - 20]), [22]);
        assert!(parse_listening_ports(&[]).is_empty());
    }

    #[test]
    fn a_bound_listener_is_found_in_the_kernel_table() {
        for address in ["127.0.0.1:0", "[::1]:0"] {
            let listener = std::net::TcpListener::bind(address).unwrap();
            let port = listener.local_addr().unwrap().port();
            assert!(listening_ports().contains(&port), "{address} on {port}");
        }
    }

    #[test]
    fn loopback_is_not_hardware() {
        let hardware = hardware_interfaces();
        assert!(!hardware.contains("lo0"), "{hardware:?}");
    }
}
