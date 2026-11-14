// Identity proof for a chappa source. Dependency-free: Node's built-in
// crypto only. The same construction is a few lines in any language with
// HMAC-SHA256 (Python hmac, Go crypto/hmac, .NET HMACSHA256).
//
// chappa sends GET /chappa/v1/identity?challenge=<64 lowercase hex chars>
// BEFORE it sends the token, and the source answers {proof}:
//
//   key   = HMAC-SHA256(key = token bytes, msg = "chappa source identity v1")
//   proof = lowercase hex HMAC-SHA256(key, challenge_hex || 0x00 || addr)
//
// token bytes = the 32 raw bytes that the hex CHAPPA_SOURCE_TOKEN decodes to;
// challenge_hex = the challenge as received (the hex string's bytes, not
// decoded); addr = the literal CHAPPA_SOURCE_ADDR string, e.g. 127.0.0.1:5000.
import { createHmac } from "node:crypto";

export function proof(tokenHex, challengeHex, addr) {
  const key = createHmac("sha256", Buffer.from(tokenHex, "hex")).update("chappa source identity v1").digest();
  return createHmac("sha256", key).update(challengeHex).update(Buffer.from([0])).update(addr).digest("hex");
}

/** A challenge is exactly 64 lowercase hex characters (32 random bytes). */
export function isValidChallenge(hex) {
  return typeof hex === "string" && /^[0-9a-f]{64}$/.test(hex);
}
