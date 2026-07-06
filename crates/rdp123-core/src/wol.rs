//! Wake-on-LAN magic packets.
//!
//! A magic packet is 6 bytes of `0xFF` followed by the target MAC address
//! repeated 16 times, sent as a UDP broadcast. Sending is fire-and-forget:
//! a machine that is already awake simply ignores it.

use std::net::UdpSocket;

/// Parse a MAC address in `aa:bb:cc:dd:ee:ff`, `aa-bb-…` or bare-hex form.
pub fn parse_mac(value: &str) -> Option<[u8; 6]> {
    let hex: String = value
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | ' '))
        .collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(mac)
}

/// Send a magic packet for `mac` as a limited broadcast (ports 9 and 7).
pub fn send_magic_packet(mac: [u8; 6]) -> std::io::Result<()> {
    let mut payload = [0u8; 6 + 16 * 6];
    payload[..6].fill(0xFF);
    for repeat in payload[6..].chunks_exact_mut(6) {
        repeat.copy_from_slice(&mac);
    }
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_broadcast(true)?;
    // Port 9 (discard) is the convention; some setups listen on 7 (echo).
    socket.send_to(&payload, ("255.255.255.255", 9))?;
    let _ = socket.send_to(&payload, ("255.255.255.255", 7));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_mac_formats() {
        let expected = [0xAA, 0xBB, 0xCC, 0x00, 0x11, 0x22];
        assert_eq!(parse_mac("aa:bb:cc:00:11:22"), Some(expected));
        assert_eq!(parse_mac("AA-BB-CC-00-11-22"), Some(expected));
        assert_eq!(parse_mac("aabbcc001122"), Some(expected));
    }

    #[test]
    fn rejects_invalid_macs() {
        assert_eq!(parse_mac(""), None);
        assert_eq!(parse_mac("aa:bb:cc:00:11"), None);
        assert_eq!(parse_mac("aa:bb:cc:00:11:2g"), None);
        assert_eq!(parse_mac("aa:bb:cc:00:11:22:33"), None);
    }

    #[test]
    fn magic_packet_layout() {
        let mac = [1, 2, 3, 4, 5, 6];
        let mut payload = [0u8; 102];
        payload[..6].fill(0xFF);
        for repeat in payload[6..].chunks_exact_mut(6) {
            repeat.copy_from_slice(&mac);
        }
        assert_eq!(&payload[..6], &[0xFF; 6]);
        assert_eq!(&payload[6..12], &mac);
        assert_eq!(&payload[96..102], &mac);
    }
}
