//! Tiny helpers for the y-protocol wire framing shared by `room` (doc
//! collab) and `board_room` (board collab). Both actors broadcast yrs
//! update bytes wrapped as `[MSG_SYNC=0, SYNC_UPDATE=2, varuint(len), bytes]`.

/// Wrap raw yrs update bytes in a SYNC_UPDATE frame so clients can decode
/// incoming broadcasts via the standard y-protocol framing.
pub fn wrap_sync_update(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(0u8); // MSG_SYNC
    out.push(2u8); // SYNC_UPDATE
    let mut v = payload.len() as u64;
    loop {
        if v < 0x80 {
            out.push(v as u8);
            break;
        }
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_yields_three_bytes() {
        let out = wrap_sync_update(&[]);
        assert_eq!(out, vec![0u8, 2, 0]);
    }

    #[test]
    fn small_payload_encodes_inline_length() {
        let p = [0xAAu8; 5];
        let out = wrap_sync_update(&p);
        assert_eq!(out, vec![0u8, 2, 5, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]);
    }

    #[test]
    fn varuint_threshold_uses_continuation_byte() {
        // 200 = 0xC8 → varuint bytes 0xC8, 0x01.
        let p = vec![0u8; 200];
        let out = wrap_sync_update(&p);
        assert_eq!(out[..4], [0u8, 2, 0xC8, 0x01]);
        assert_eq!(out.len(), 4 + 200);
    }
}
