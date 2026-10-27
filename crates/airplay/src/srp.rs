//! SRP-6a client for HAP transient pairing (workstream: control plane).
//!
//! RFC 5054 3072-bit group, g = 5, SHA-512, I = "Pair-Setup", P = "3939", computed
//! exactly like srptools 1.0.1 as pyatv uses it (`pyatv/auth/hap_srp.py`):
//! salt hashed as the raw received bytes; A, B, S, H(I) and H(N)^H(g) hashed as
//! minimal-length big-endian integers; PAD(...) to 384 bytes only for g in k and
//! A/B in u; K = SHA-512(S) (64 bytes). Deliberate deviation: the private key `a`
//! is 32 random bytes regenerated until A is exactly 384 bytes long.
//! Fixture: `tests/fixtures/srp.json` (overrides any prose).
