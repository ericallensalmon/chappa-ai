//! The source side of the identity handshake, and the bearer-token check.
//!
//! The app sends `GET /chappa/v1/identity?challenge=<64 hex>` before it ever
//! sends the token, and expects
//!
//! ```text
//! key   = HMAC-SHA256(key = token bytes, msg = "chappa source identity v1")
//! proof = lowercase hex HMAC-SHA256(key, challenge_hex || 0x00 || addr)
//! ```
//!
//! where the token bytes are the 32 raw bytes its hex form decodes to, the
//! challenge is the hex string as received, and `addr` is the literal
//! address string the app connected to. A process that grabbed the port
//! first cannot answer, so it never sees the token.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const IDENTITY_PATH: &str = "/chappa/v1/identity";
pub const MANIFEST_PATH: &str = "/chappa/v1/manifest";
/// The loopback address the app picked, set when it launches a source.
pub const ADDR_ENV: &str = "CHAPPA_SOURCE_ADDR";
/// The 64-hex-char token, set when the app launches a source.
pub const TOKEN_ENV: &str = "CHAPPA_SOURCE_TOKEN";

const KEY_LABEL: &[u8] = b"chappa source identity v1";

/// The 32-byte token a source is handed, kept as its hex form (what the
/// bearer header carries) and its raw bytes (what the proof is keyed by).
#[derive(Clone)]
pub struct Token {
    hex: String,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// Exactly 64 lowercase hex characters (surrounding whitespace, as a
    /// token file's trailing newline, is ignored).
    pub fn parse(text: &str) -> Result<Token, String> {
        let hex = text.trim();
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("the token must be exactly 64 lowercase hex characters".into());
        }
        let bytes = (0..32)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("checked hex"))
            .collect();
        Ok(Token {
            hex: hex.to_string(),
            bytes,
        })
    }

    /// A fresh random token (standalone runs and tests).
    pub fn random() -> Result<Token, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| format!("cannot generate a token: {e}"))?;
        Token::parse(&to_hex(&bytes))
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// Whether an `Authorization` header value carries this token. Both
    /// sides are hashed first and the digests compared in constant time, so
    /// neither the length nor the content of a wrong token leaks by timing.
    pub fn authorizes(&self, header: Option<&str>) -> bool {
        let presented = header
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim)
            .unwrap_or("");
        constant_time_eq(
            &Sha256::digest(presented.as_bytes()),
            &Sha256::digest(self.hex.as_bytes()),
        )
    }

    /// The proof for `challenge_hex` on `addr`.
    pub fn proof(&self, challenge_hex: &str, addr: &str) -> String {
        proof(&self.bytes, challenge_hex, addr)
    }
}

/// `proof = hex HMAC-SHA256(HMAC-SHA256(token, label), challenge || 0 || addr)`.
pub fn proof(token_bytes: &[u8], challenge_hex: &str, addr: &str) -> String {
    let mut outer = <Hmac<Sha256> as Mac>::new_from_slice(token_bytes).expect("any key length");
    outer.update(KEY_LABEL);
    let key = outer.finalize().into_bytes();
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).expect("any key length");
    mac.update(challenge_hex.as_bytes());
    mac.update(&[0]);
    mac.update(addr.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// A challenge is exactly 64 lowercase hex characters (32 random bytes).
pub fn is_valid_challenge(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_exactly_64_lowercase_hex() {
        assert!(Token::parse(&"ab".repeat(32)).is_ok());
        assert!(Token::parse(&format!("{}\n", "ab".repeat(32))).is_ok());
        assert!(Token::parse(&"AB".repeat(32)).is_err());
        assert!(Token::parse(&"ab".repeat(31)).is_err());
        assert!(Token::parse(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn the_bearer_check_needs_the_exact_token() {
        let t = Token::parse(&"01".repeat(32)).unwrap();
        assert!(t.authorizes(Some(&format!("Bearer {}", "01".repeat(32)))));
        assert!(!t.authorizes(Some(&format!("Bearer {}", "02".repeat(32)))));
        assert!(!t.authorizes(Some(&"01".repeat(32))));
        assert!(!t.authorizes(Some("Bearer ")));
        assert!(!t.authorizes(None));
    }

    #[test]
    fn challenges_are_64_lowercase_hex() {
        assert!(is_valid_challenge(&"0f".repeat(32)));
        assert!(!is_valid_challenge(&"0F".repeat(32)));
        assert!(!is_valid_challenge("00"));
    }
}
