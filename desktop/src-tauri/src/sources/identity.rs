//! A source proves it holds its token BEFORE the token is sent to it.
//!
//! The app hands a launched source its loopback address and a fresh 32-byte
//! token through its environment. Another local program can bind that port
//! first (or the one an attached source is configured at), and a client
//! that simply sent `Authorization: Bearer <token>` would hand the token to
//! whoever holds the port. So the first request on every connection is a
//! tokenless challenge:
//!
//! ```text
//! GET /chappa/v1/identity?challenge=<32 random bytes, lowercase hex>
//! → 200 {"proof": "<hex>"}
//!
//! key   = HMAC-SHA256(key = token bytes, message = "chappa source identity v1")
//! proof = HMAC-SHA256(key, challenge_hex || 0x00 || addr)   (lowercase hex)
//! ```
//!
//! `token bytes` are the 32 bytes the hex token decodes to, `challenge_hex`
//! is the challenge as sent (its hex text, not the decoded bytes) and `addr`
//! is the literal address string the source was given (`127.0.0.1:5000`).
//! Only after the proof verifies does the token go out, on the SAME
//! connection, so a program that takes the port between two connections
//! never sees it either.
//!
//! The source answers this route for anyone: the proof is keyed with a key
//! DERIVED from the token, so signing chosen challenges reveals nothing
//! usable as the bearer credential. HMAC-SHA256 rather than the control
//! surface's BLAKE3 because a source is written in any language, and
//! HMAC-SHA256 is in every standard library.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The tokenless identity route.
pub const IDENTITY_PATH: &str = "/chappa/v1/identity";
/// The manifest route (needs the token).
pub const MANIFEST_PATH: &str = "/chappa/v1/manifest";
/// The environment variables a launched source reads.
pub const ADDR_ENV: &str = "CHAPPA_SOURCE_ADDR";
pub const TOKEN_ENV: &str = "CHAPPA_SOURCE_TOKEN";
/// Token and challenge sizes in bytes (both travel as lowercase hex).
pub const TOKEN_BYTES: usize = 32;
pub const CHALLENGE_BYTES: usize = 32;
/// The key-derivation message: binds the derived key to this one use.
const KEY_CONTEXT: &[u8] = b"chappa source identity v1";

type HmacSha256 = Hmac<Sha256>;

/// The proof a source must answer for `challenge_hex`, given its token
/// (raw bytes) and the literal address it was given.
pub fn source_proof(token: &[u8], challenge_hex: &str, addr: &str) -> String {
    let mut derive = HmacSha256::new_from_slice(token).expect("HMAC takes any key length");
    derive.update(KEY_CONTEXT);
    let key = derive.finalize().into_bytes();
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC takes any key length");
    mac.update(challenge_hex.as_bytes());
    mac.update(&[0]);
    mac.update(addr.as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// Exactly [`TOKEN_BYTES`] as lowercase hex, decoded; `None` for anything
/// else (an uppercase or short token is a typo, not a different token).
pub fn decode_token(token_hex: &str) -> Option<Vec<u8>> {
    if token_hex.len() != TOKEN_BYTES * 2 || !is_lower_hex(token_hex) {
        return None;
    }
    (0..token_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&token_hex[i..i + 2], 16).ok())
        .collect()
}

/// A fresh token from the OS RNG, as the hex the source receives.
pub fn new_token_hex() -> Result<String, String> {
    random_hex(TOKEN_BYTES)
}

/// A fresh challenge from the OS RNG. An RNG failure is an error, never a
/// predictable fallback: a predictable challenge would let a program collect
/// answers from the real source ahead of time and replay them.
pub fn new_challenge() -> Result<String, String> {
    random_hex(CHALLENGE_BYTES)
}

/// Whether `s` is a well-formed challenge (what a source must check too).
pub fn is_valid_challenge(s: &str) -> bool {
    s.len() == CHALLENGE_BYTES * 2 && is_lower_hex(s)
}

/// Compare two proofs without an early exit on the first differing byte.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_hex(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness available: {e}"))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vectors every source implementation is checked against, shipped
    /// next to the example source. The Rust side must agree with them to the
    /// byte, or no source written from the documentation would ever connect.
    const VECTORS: &str = include_str!("../../../sources/fixture/test/identity-vectors.json");

    #[test]
    fn the_proof_matches_every_published_vector() {
        let vectors: Vec<serde_json::Value> = serde_json::from_str(VECTORS).unwrap();
        assert!(vectors.len() >= 5, "the vector file lost entries");
        for v in &vectors {
            let token = decode_token(v["token"].as_str().unwrap()).expect("vector token decodes");
            let proof = source_proof(
                &token,
                v["challenge"].as_str().unwrap(),
                v["addr"].as_str().unwrap(),
            );
            assert_eq!(proof, v["proof"].as_str().unwrap(), "vector {v}");
        }
    }

    #[test]
    fn the_proof_depends_on_token_challenge_and_address() {
        let token = decode_token(&"ab".repeat(32)).unwrap();
        let base = source_proof(&token, &"01".repeat(32), "127.0.0.1:1");
        assert_eq!(base.len(), 64);
        assert_ne!(
            base,
            source_proof(
                &decode_token(&"ac".repeat(32)).unwrap(),
                &"01".repeat(32),
                "127.0.0.1:1"
            )
        );
        assert_ne!(base, source_proof(&token, &"02".repeat(32), "127.0.0.1:1"));
        assert_ne!(base, source_proof(&token, &"01".repeat(32), "127.0.0.1:2"));
        // The separator keeps the two fields apart.
        assert_ne!(
            source_proof(&token, "ab", "c"),
            source_proof(&token, "a", "bc")
        );
    }

    #[test]
    fn tokens_and_challenges_are_exact_lowercase_hex() {
        assert!(decode_token(&"0f".repeat(32)).is_some());
        assert!(
            decode_token(&"0F".repeat(32)).is_none(),
            "uppercase is refused"
        );
        assert!(decode_token(&"0f".repeat(31)).is_none(), "short is refused");
        assert!(decode_token(&format!("{}zz", "0f".repeat(31))).is_none());
        let token = new_token_hex().unwrap();
        assert_eq!(decode_token(&token).unwrap().len(), TOKEN_BYTES);
        assert_ne!(token, new_token_hex().unwrap());
        let challenge = new_challenge().unwrap();
        assert!(is_valid_challenge(&challenge));
        assert!(!is_valid_challenge(&challenge.to_uppercase()));
        assert!(!is_valid_challenge(&challenge[1..]));
    }

    #[test]
    fn comparison_is_length_and_content_sensitive() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
