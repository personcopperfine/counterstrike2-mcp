//! The session token: 32 bytes from the OS RNG, written as 64 lowercase hex characters.
//!
//! `start_listener_once` (`bridge/mcp_bridge.c:6734-6742`) fills `g_token` from
//! `BCryptGenRandom(0, raw, 32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)` and **fails closed** when the
//! call fails: a log line, no threads, no discovery file. [`from_bytes`] is the hex half
//! (`:6739-6741`); [`random`] is the OS half, `getrandom(2)` on Linux instead of
//! `BCryptGenRandom` (the same kernel CSPRNG `BCryptGenRandom` uses).
//!
//! The comparison in the hello path is the C's `ct_eq` (`:158-163`): length first, then every
//! byte XORed into one accumulator, so a wrong token cannot be found byte by byte.

/// How many random bytes the token carries (`mcp_bridge.c:6735`, `u8 raw[32]`).
pub const RAW_BYTES: usize = 32;

/// The token length in characters (`mcp_bridge.c:1013`, `static char g_token[65]`).
pub const TOKEN_LEN: usize = RAW_BYTES * 2;

/// Renders 32 random bytes as the C's 64 lowercase hex characters (`mcp_bridge.c:6739-6741`).
pub(crate) fn from_bytes(raw: &[u8; RAW_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut token = String::with_capacity(TOKEN_LEN);
    for byte in raw {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 15)]));
    }
    token
}

/// The C's constant-time token comparison (`mcp_bridge.c:158-163`): `false` on a length
/// mismatch, otherwise the XOR of every byte pair accumulated and tested once.
pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Fills `buf` from the OS RNG; `false` on failure (the C's nonzero `BCryptGenRandom` status).
///
/// Linux: `getrandom(2)` with no flags, which blocks only until the pool is initialised. The
/// libc call is bounded to the buffer it is given, so an interruption returns `EINTR` and the
/// answer is "failure", exactly the C's fail-closed path.
#[cfg(unix)]
pub(crate) fn random(buf: &mut [u8; RAW_BYTES]) -> bool {
    // SAFETY: `buf` is a valid writable pointer to exactly `RAW_BYTES` bytes for the duration
    // of the call; `getrandom` writes at most that many and reads nothing else.
    let written = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), RAW_BYTES, 0) };
    written == RAW_BYTES as isize
}

/// Fills `buf` with `BCryptGenRandom` (`mcp_bridge.c:6736`).
#[cfg(windows)]
pub(crate) fn random(buf: &mut [u8; RAW_BYTES]) -> bool {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    // SAFETY: `buf` is a valid writable pointer to `RAW_BYTES` bytes; a null algorithm handle
    // with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` is the documented system-RNG call, and the status
    // is checked below.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            RAW_BYTES as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    status == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-to-text rule of `mcp_bridge.c:6739-6741`: high nibble first, lowercase, 64
    /// characters, nothing else.
    #[test]
    fn bytes_become_lowercase_hex() {
        let mut raw = [0u8; RAW_BYTES];
        raw[0] = 0x00;
        raw[1] = 0x0f;
        raw[2] = 0xff;
        raw[31] = 0xa5;
        let token = from_bytes(&raw);
        assert_eq!(token.len(), TOKEN_LEN);
        assert_eq!(&token[..6], "000fff");
        assert_eq!(&token[60..], "00a5");
        assert!(token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    /// `ct_eq` (`:158-163`): equal strings match, a difference in any position is caught, and a
    /// length mismatch is a mismatch even when one string is a prefix of the other.
    #[test]
    fn token_comparison_is_exact() {
        assert!(ct_eq("", ""));
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(!ct_eq("ab", "abc"));
        assert!(!ct_eq("Abc", "abc"));
    }

    /// The OS source fills the whole buffer, and two calls do not repeat (the CSPRNG contract
    /// the fail-closed path relies on).
    #[test]
    fn os_rng_fills_the_buffer() {
        let mut a = [0u8; RAW_BYTES];
        assert!(random(&mut a), "the OS RNG must answer");
        let mut b = [0u8; RAW_BYTES];
        assert!(random(&mut b), "the OS RNG must answer twice");
        assert_ne!(a, b, "two 32-byte draws are not equal");
    }
}
