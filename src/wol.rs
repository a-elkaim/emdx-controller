//! Wake-on-LAN.

use anyhow::{bail, Context, Result};
use tokio::net::UdpSocket;

/// Parse `aa:bb:cc:dd:ee:ff` (or `-` separated) into bytes.
pub fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = mac.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        bail!("invalid MAC address {mac:?}");
    }
    let mut out = [0u8; 6];
    for (slot, part) in out.iter_mut().zip(parts) {
        if part.is_empty() || part.len() > 2 {
            bail!("invalid MAC address {mac:?}");
        }
        *slot =
            u8::from_str_radix(part, 16).with_context(|| format!("invalid MAC address {mac:?}"))?;
    }
    Ok(out)
}

/// 6 × `FF` followed by the MAC repeated 16 times.
pub fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    packet
}

/// Broadcast a magic packet on UDP port 9.
pub async fn wake(mac: &str) -> Result<()> {
    wake_to(mac, "255.255.255.255:9").await
}

pub async fn wake_to(mac: &str, target: &str) -> Result<()> {
    let packet = magic_packet(parse_mac(mac)?);
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;
    socket
        .send_to(&packet, target)
        .await
        .with_context(|| format!("sending Wake-on-LAN packet to {target}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macs() {
        assert_eq!(
            parse_mac("b0:f2:f6:55:0d:0b").unwrap(),
            [0xB0, 0xF2, 0xF6, 0x55, 0x0D, 0x0B]
        );
        assert_eq!(
            parse_mac("B0-F2-F6-55-D-B").unwrap(),
            [0xB0, 0xF2, 0xF6, 0x55, 0x0D, 0x0B]
        );
        assert!(parse_mac("b0:f2:f6:55:0d").is_err());
        assert!(parse_mac("b0:f2:f6:55:0d:zz").is_err());
        assert!(parse_mac("b0:f2:f6:55:0d:0b0").is_err());
    }

    #[test]
    fn builds_magic_packet() {
        let mac = [1, 2, 3, 4, 5, 6];
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
    }

    #[tokio::test]
    async fn sends_packet_over_udp() {
        let rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = rx.local_addr().unwrap().to_string();
        wake_to("01:02:03:04:05:06", &addr).await.unwrap();
        let mut buf = [0u8; 200];
        let (n, _) = rx.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], magic_packet([1, 2, 3, 4, 5, 6]).as_slice());
    }
}
