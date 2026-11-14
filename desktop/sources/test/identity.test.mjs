// Vectors file: desktop/sources/fixture/test/identity-vectors.json, a JSON
// array of {token, challenge, addr, proof}: token = 64 hex chars (32 bytes),
// challenge = 64 lowercase hex chars, addr = the literal address string,
// proof = lowercase hex HMAC-SHA256 per lib/identity.mjs. The Rust host
// tests read the same file.
import test from "node:test";
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { readFileSync } from "node:fs";
import { isValidChallenge, proof } from "../lib/identity.mjs";

const vectors = JSON.parse(readFileSync(new URL("../fixture/test/identity-vectors.json", import.meta.url), "utf8"));

test("the vectors file has the edge cases", () => {
  assert.ok(vectors.length >= 4);
  assert.ok(vectors.some((v) => v.challenge === "f".repeat(64)));
  assert.ok(vectors.some((v) => !v.addr.endsWith(":5000") && !v.addr.endsWith(":80")));
});

test("proof matches every vector", () => {
  for (const v of vectors) assert.equal(proof(v.token, v.challenge, v.addr), v.proof, v.addr);
});

test("proof follows the written construction", () => {
  const v = vectors[0];
  const key = createHmac("sha256", Buffer.from(v.token, "hex")).update("chappa source identity v1").digest();
  const msg = Buffer.concat([Buffer.from(v.challenge, "utf8"), Buffer.from([0]), Buffer.from(v.addr, "utf8")]);
  assert.equal(createHmac("sha256", key).update(msg).digest("hex"), v.proof);
});

test("the proof depends on token, challenge and address", () => {
  const v = vectors[0];
  assert.notEqual(proof(vectors[1].token, v.challenge, v.addr), v.proof);
  assert.notEqual(proof(v.token, vectors[1].challenge, v.addr), v.proof);
  assert.notEqual(proof(v.token, v.challenge, "127.0.0.1:5001"), v.proof);
});

test("challenge validation edges", () => {
  assert.equal(isValidChallenge("a".repeat(64)), true);
  assert.equal(isValidChallenge("0123456789abcdef".repeat(4)), true);
  assert.equal(isValidChallenge("a".repeat(63)), false);
  assert.equal(isValidChallenge("a".repeat(65)), false);
  assert.equal(isValidChallenge("A".repeat(64)), false);
  assert.equal(isValidChallenge("g".repeat(64)), false);
  assert.equal(isValidChallenge("a".repeat(63) + "\n"), false);
  assert.equal(isValidChallenge(""), false);
  assert.equal(isValidChallenge(null), false);
  assert.equal(isValidChallenge(undefined), false);
  assert.equal(isValidChallenge(["a".repeat(64)]), false);
});
