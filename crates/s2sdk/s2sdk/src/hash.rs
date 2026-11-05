//! `MurmurHash2` lowercase variant, as upstream hashes string-token keys.
//!
//! Direct port of `MurmurHash2<true>` from the pinned upstream header
//! `public/tier0/generichash.h` (`GenericHash::detail::to_lower` included): keys are hashed
//! ASCII-lowercased, four bytes at a time, little-endian packed, then the standard Murmur2
//! tail mix. The seed the engine uses for `CUtlStringToken` / `CKV3MemberName` is
//! `STRINGTOKEN_MURMURHASH_SEED` (`public/tier0/utlstringtoken.h`) = `0x31415926`.
//!
//! Pinned values (same algorithm, verified against the header):
//! `"userid"` -> `0x3e7b804b`, `"attacker"` -> `0xc819dd8a`, `"headshot"` -> `0x533754f8`,
//! `"weapon"` -> `0xebde2b13`, `"player_death"` -> `0xb6c81b7a`.
//!
//! The empty string has no special case here (`""` hashes to `0xb5d89f2f`); the empty-name
//! special case (hash code 0) lives where upstream has it: in
//! `CKV3MemberName`'s `const char *` constructor, which leaves `m_nHashCode = 0` for a null or
//! empty string — mirrored by [`crate::events::KeySymbol::new`].

/// ASCII lowercase, branchlessly: `c + (((c >= 'A') && (c <= 'Z')) << 5)`, byte-wise.
fn to_lower(c: u8) -> u8 {
    let offset = u8::from(c.is_ascii_uppercase());
    c + (offset << 5)
}

/// `MurmurHash2` with the `CASELESS` template argument `true` (upstream's
/// `MurmurHash2LowerCase`): lowercase each byte before the mix, process the buffer as
/// little-endian 32-bit words, seed first.
///
/// The input is treated as raw bytes (callers pass the bytes of a NUL-terminated C string
/// without the terminator, matching upstream's `strlen`-driven runtime overload).
#[must_use]
pub fn murmur_hash2_lowercase(bytes: &[u8], seed: u32) -> u32 {
    // The mixing constants from the upstream header ('m' and 'r').
    const M: u32 = 0x5bd1e995;
    const R: u32 = 24;

    let mut h = seed ^ (bytes.len() as u32);

    let chunks = bytes.as_chunks::<4>();
    for chunk in chunks.0 {
        let mut k = u32::from(to_lower(chunk[0]))
            | (u32::from(to_lower(chunk[1])) << 8)
            | (u32::from(to_lower(chunk[2])) << 16)
            | (u32::from(to_lower(chunk[3])) << 24);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M);
        h ^= k;
    }

    // The tail switch falls through 3 -> 2 -> 1 (upstream keeps the case labels empty so the
    // C fallthrough accumulates), and only the last byte multiplies.
    let rem = chunks.1;
    if rem.len() >= 3 {
        h ^= u32::from(to_lower(rem[2])) << 16;
    }
    if rem.len() >= 2 {
        h ^= u32::from(to_lower(rem[1])) << 8;
    }
    if !rem.is_empty() {
        h ^= u32::from(to_lower(rem[0]));
        h = h.wrapping_mul(M);
    }

    // Final mix.
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h
}

/// The seed upstream uses for every string token: `STRINGTOKEN_MURMURHASH_SEED` from
/// `public/tier0/utlstringtoken.h`.
pub const STRINGTOKEN_MURMURHASH_SEED: u32 = 0x3141_5926;

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned expected values, computed with the upstream header
    /// (`MurmurHash2LowerCase(name, STRINGTOKEN_MURMURHASH_SEED)`).
    #[test]
    fn pinned_string_token_values() {
        let seed = STRINGTOKEN_MURMURHASH_SEED;
        assert_eq!(murmur_hash2_lowercase(b"userid", seed), 0x3e7b_804b);
        assert_eq!(murmur_hash2_lowercase(b"attacker", seed), 0xc819_dd8a);
        assert_eq!(murmur_hash2_lowercase(b"headshot", seed), 0x5337_54f8);
        assert_eq!(murmur_hash2_lowercase(b"weapon", seed), 0xebde_2b13);
        assert_eq!(murmur_hash2_lowercase(b"player_death", seed), 0xb6c8_1b7a);
    }

    /// The port against the compiler's answer: the `TOKEN_*` consts in the generated
    /// bindings are clang's evaluation of upstream's own
    /// `MurmurHash2LowerCase(name, STRINGTOKEN_MURMURHASH_SEED)`
    /// (gen/allowlist.toml's `[consts]`), so comparing them here ties this file to
    /// the SDK headers instead of to itself.
    #[test]
    fn matches_the_clang_measured_string_token_consts() {
        use s2sdk_sys::generated::{
            STRINGTOKEN_MURMURHASH_SEED as CLANG_SEED, TOKEN_OUTPUT_COLOR, TOKEN_OUTPUT_DEPTH,
            TOKEN_TOOLS,
        };
        assert_eq!(CLANG_SEED, STRINGTOKEN_MURMURHASH_SEED);
        assert_eq!(murmur_hash2_lowercase(b"Tools", CLANG_SEED), TOKEN_TOOLS);
        assert_eq!(
            murmur_hash2_lowercase(b"OutputColor", CLANG_SEED),
            TOKEN_OUTPUT_COLOR
        );
        assert_eq!(
            murmur_hash2_lowercase(b"OutputDepth", CLANG_SEED),
            TOKEN_OUTPUT_DEPTH
        );
    }

    /// Case-insensitivity: the hash is lowercase-blind, and lengths hit every tail case.
    #[test]
    fn lowercase_insensitive_and_tail_lengths() {
        let seed = STRINGTOKEN_MURMURHASH_SEED;
        assert_eq!(
            murmur_hash2_lowercase(b"UserID", seed),
            murmur_hash2_lowercase(b"userid", seed)
        );
        assert_eq!(
            murmur_hash2_lowercase(b"PLAYER_DEATH", seed),
            murmur_hash2_lowercase(b"player_death", seed)
        );
        // 1, 2, 3 and 5-byte inputs cover every tail branch (len % 4 == 1, 2, 3 and a
        // full chunk plus a 1-byte tail).
        for s in [b"a".as_slice(), b"ab", b"abc", b"abcde"] {
            assert_ne!(murmur_hash2_lowercase(s, seed), 0);
        }
    }
}
