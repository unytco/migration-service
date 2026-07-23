import { describe, expect, it } from "vitest";
import { normalizeDnaHashB64 } from "../src/notary";

// A HoloHash is exactly 39 bytes; the router accepts a DNA hash either as its b64
// string form or as that raw byte array (the notary relays the zome payload verbatim).
describe("normalizeDnaHashB64", () => {
  it("passes a b64 string through unchanged", () => {
    expect(normalizeDnaHashB64("uhC0k_whatever")).toBe("uhC0k_whatever");
  });

  it("encodes a 39-byte HoloHash array as unpadded base64url with the 'u' prefix", () => {
    const bytes = [0x84, 0x2d, 0x24, ...Array(32).fill(0), 0xde, 0xad, 0xbe, 0xef]; // 39
    // Node's base64url is an independent oracle for the same transform.
    const expected = "u" + Buffer.from(bytes).toString("base64url");
    expect(normalizeDnaHashB64(bytes)).toBe(expected);
    expect(normalizeDnaHashB64(bytes)).not.toMatch(/[+/=]/); // url-safe, unpadded
  });

  it("rejects arrays that are not exactly 39 bytes", () => {
    expect(normalizeDnaHashB64([])).toBeUndefined();
    expect(normalizeDnaHashB64(Array(38).fill(0))).toBeUndefined();
    expect(normalizeDnaHashB64(Array(40).fill(0))).toBeUndefined();
  });

  it("rejects a 39-length array carrying a non-byte value (out of range / non-integer / non-number)", () => {
    expect(normalizeDnaHashB64([...Array(38).fill(0), 256])).toBeUndefined();
    expect(normalizeDnaHashB64([...Array(38).fill(0), -1])).toBeUndefined();
    expect(normalizeDnaHashB64([...Array(38).fill(0), 1.5])).toBeUndefined();
    expect(
      normalizeDnaHashB64([...Array(38).fill(0), "0" as unknown as number]),
    ).toBeUndefined();
  });

  it("returns undefined for null / undefined / non-array / object shapes", () => {
    expect(normalizeDnaHashB64(null)).toBeUndefined();
    expect(normalizeDnaHashB64(undefined)).toBeUndefined();
    expect(normalizeDnaHashB64(42)).toBeUndefined();
    expect(normalizeDnaHashB64({})).toBeUndefined();
  });
});
