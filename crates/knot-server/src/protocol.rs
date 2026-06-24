//! y-sync v1 wire format helpers.
//!
//! Frame structure:
//!   <msg_type:u8> [<sync_subtype:u8>] <varuint length> <payload bytes>
//!
//! Awareness frames carry their own length-prefixed payload directly
//! after the type byte (no subtype).

pub const MSG_SYNC: u8 = 0;
pub const MSG_AWARENESS: u8 = 1;
/// Server→client push: comments on this doc changed; client should refetch.
/// Payload: <varuint len><JSON bytes> where JSON is `{ "doc_id": "<uuid>" }`.
pub const MSG_COMMENTS: u8 = 5;
pub const SYNC_STEP_1: u8 = 0;
pub const SYNC_STEP_2: u8 = 1;
pub const SYNC_UPDATE: u8 = 2;

#[derive(Debug)]
pub enum YSyncMessage {
    SyncStep1(Vec<u8>),
    SyncStep2(Vec<u8>),
    Update(Vec<u8>),
    Awareness, // payload not parsed; broker re-broadcasts verbatim
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("frame truncated")]
    Truncated,
    #[error("unknown message type {0}")]
    UnknownType(u8),
    #[error("unknown sync subtype {0}")]
    UnknownSubtype(u8),
}

pub fn decode(buf: &[u8]) -> Result<YSyncMessage, DecodeError> {
    if buf.is_empty() {
        return Err(DecodeError::Truncated);
    }
    match buf[0] {
        MSG_SYNC => {
            if buf.len() < 2 {
                return Err(DecodeError::Truncated);
            }
            let subtype = buf[1];
            let (payload, _) = read_var_bytes(&buf[2..])?;
            let payload = payload.to_vec();
            match subtype {
                SYNC_STEP_1 => Ok(YSyncMessage::SyncStep1(payload)),
                SYNC_STEP_2 => Ok(YSyncMessage::SyncStep2(payload)),
                SYNC_UPDATE => Ok(YSyncMessage::Update(payload)),
                other => Err(DecodeError::UnknownSubtype(other)),
            }
        }
        MSG_AWARENESS => Ok(YSyncMessage::Awareness),
        other => Err(DecodeError::UnknownType(other)),
    }
}

pub fn encode_sync_step2(payload: &[u8]) -> Vec<u8> {
    encode_sync(SYNC_STEP_2, payload)
}

pub fn encode_sync_update(payload: &[u8]) -> Vec<u8> {
    encode_sync(SYNC_UPDATE, payload)
}

fn encode_sync(subtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.push(MSG_SYNC);
    out.push(subtype);
    append_var_uint(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out
}

pub fn append_var_uint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_var_bytes(buf: &[u8]) -> Result<(&[u8], usize), DecodeError> {
    let (len, consumed) = read_var_uint(buf)?;
    let len = usize::try_from(len).map_err(|_| DecodeError::Truncated)?;
    let total = consumed.checked_add(len).ok_or(DecodeError::Truncated)?;
    if buf.len() < total {
        return Err(DecodeError::Truncated);
    }
    Ok((&buf[consumed..total], total))
}

fn read_var_uint(buf: &[u8]) -> Result<(u64, usize), DecodeError> {
    let mut v: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &b) in buf.iter().enumerate() {
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return Err(DecodeError::Truncated);
        }
    }
    Err(DecodeError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_var_bytes_rejects_oversize_length_without_panicking() {
        let mut buf = Vec::new();
        append_var_uint(&mut buf, u64::MAX);
        assert!(matches!(read_var_bytes(&buf), Err(DecodeError::Truncated)));
    }

    #[test]
    fn read_var_bytes_reads_a_valid_payload() {
        let mut buf = Vec::new();
        append_var_uint(&mut buf, 3);
        buf.extend_from_slice(b"abc");
        let (payload, total) = read_var_bytes(&buf).unwrap();
        assert_eq!(payload, b"abc");
        assert_eq!(total, buf.len());
    }

    #[test]
    fn roundtrip_varuint() {
        for v in [0u64, 1, 127, 128, 16383, 16384, 1 << 20, u64::MAX] {
            let mut buf = Vec::new();
            append_var_uint(&mut buf, v);
            let (got, n) = read_var_uint(&buf).expect("decode");
            assert_eq!(got, v);
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn roundtrip_sync_step2() {
        let payload = vec![0xde, 0xad, 0xbe, 0xef];
        let encoded = encode_sync_step2(&payload);
        let decoded = decode(&encoded).expect("decode");
        match decoded {
            YSyncMessage::SyncStep2(p) => assert_eq!(p, payload),
            other => panic!("expected SyncStep2, got {other:?}"),
        }
    }
}
