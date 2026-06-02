//! Awareness frames — opaque bytes from the wire; we never decode them.
//! Size cap = 4 KB on emit. On disconnect the room synthesises a clearing
//! frame so other clients drop the departed cursor.

pub const PRESENCE_MAX_BYTES: usize = 4 * 1024;

pub fn is_oversize(payload: &[u8]) -> bool {
    payload.len() > PRESENCE_MAX_BYTES
}
