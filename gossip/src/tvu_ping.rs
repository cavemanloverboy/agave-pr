//! Echo packets used to measure RTT to a TVU socket.
//!
//! A ping is not a shred. Receivers echo the datagram and drop it before
//! shred verification.

pub const TVU_PING_LEN: usize = 12;
const MAGIC: [u8; 4] = *b"TVUP";

pub fn encode(token: u64, dst: &mut [u8; TVU_PING_LEN]) {
    dst[..4].copy_from_slice(&MAGIC);
    dst[4..].copy_from_slice(&token.to_le_bytes());
}

#[must_use]
pub fn decode(buf: &[u8]) -> Option<u64> {
    if buf.len() != TVU_PING_LEN || buf[..4] != MAGIC {
        return None;
    }
    let mut token = [0u8; 8];
    token.copy_from_slice(&buf[4..]);
    Some(u64::from_le_bytes(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_reject_shred_sized_packets() {
        let mut buf = [0u8; TVU_PING_LEN];
        encode(0x1122_3344_5566_7788, &mut buf);
        assert_eq!(decode(&buf), Some(0x1122_3344_5566_7788));
        assert!(decode(&[0u8; 64]).is_none());
        assert!(decode(&buf[..11]).is_none());
    }
}
