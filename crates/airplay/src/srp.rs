//! SRP-6a client for HAP transient pairing (workstream: control plane).
//!
//! RFC 5054 3072-bit group, g = 5, SHA-512, I = "Pair-Setup", P = "3939", computed
//! exactly like srptools 1.0.1 as pyatv uses it (`pyatv/auth/hap_srp.py`):
//! salt hashed as the raw received bytes; A, B, S, H(I) and H(N)^H(g) hashed as
//! minimal-length big-endian integers; PAD(...) to 384 bytes only for g in k and
//! A/B in u; K = SHA-512(S) (64 bytes). Deliberate deviation: the private key `a`
//! is 32 random bytes regenerated until A is exactly 384 bytes long.
//! Fixture: `tests/fixtures/srp.json` (overrides any prose).
//!
//! Formulas (all integers big-endian; "min" = no leading zero bytes):
//!
//! * `k = H(N | PAD(g))`, `x = H(s | H("Pair-Setup:3939"))`, `v = g^x`
//! * `A = g^a`, `u = H(PAD(A) | PAD(B))`, `S = (B - k·v)^(a + u·x) mod N`
//! * `K = H(min(S))`, `M1 = H(min(H(N)^H(g)) | min(H(I)) | s | min(A) | min(B) | K)`
//! * `M2 = H(min(A) | M1 | K)` (the server's proof, which we only log)

use std::sync::LazyLock;

use num_bigint::BigUint;
use num_traits::Zero;
use sha2::{Digest, Sha512};

/// SRP user name for HAP pair-setup.
pub const USERNAME: &str = "Pair-Setup";
/// PIN used by transient pairing.
pub const TRANSIENT_PIN: &str = "3939";
/// Byte length of the group modulus (3072 bits).
pub const MODULUS_LEN: usize = 384;
/// Group generator.
pub const GENERATOR: u32 = 5;
/// Length of our random private exponent `a`.
pub const PRIVATE_KEY_LEN: usize = 32;

/// The RFC 5054 3072-bit group prime, big-endian.
pub const PRIME_3072: [u8; MODULUS_LEN] = hex_array(concat!(
    "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74",
    "020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437",
    "4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed",
    "ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf05",
    "98da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb",
    "9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3b",
    "e39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf695581718",
    "3995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33",
    "a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7",
    "abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864",
    "d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e2",
    "08e24fa074e5ab3143db5bfce0fd108e4b82d120a93ad2caffffffffffffffff",
));

/// Decode a hex string into a fixed-size array at compile time.
const fn hex_array<const LEN: usize>(hex: &str) -> [u8; LEN] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("invalid hex digit"),
        }
    }
    let bytes = hex.as_bytes();
    assert!(bytes.len() == LEN * 2, "hex length mismatch");
    let mut out = [0u8; LEN];
    let mut i = 0;
    while i < LEN {
        out[i] = (nibble(bytes[2 * i]) << 4) | nibble(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

/// Group parameters and the derived constants, computed once.
struct Group {
    n: BigUint,
    g: BigUint,
    /// `k = H(N | PAD(g))`.
    k: BigUint,
    /// `min(H(N) ^ H(g))`, the first M1 input.
    h_n_xor_h_g: Vec<u8>,
    /// `min(H(I))`, the second M1 input.
    h_i: Vec<u8>,
    /// `H(I ":" P)`, the inner password hash of `x`.
    h_identity: [u8; 64],
}

static GROUP: LazyLock<Group> = LazyLock::new(|| {
    let n = BigUint::from_bytes_be(&PRIME_3072);
    let g = BigUint::from(GENERATOR);
    let k = BigUint::from_bytes_be(&hash(&[&minimal(&n), &pad(&g)]));
    let h_n = BigUint::from_bytes_be(&hash(&[&minimal(&n)]));
    let h_g = BigUint::from_bytes_be(&hash(&[&minimal(&g)]));
    let h_n_xor_h_g = minimal(&(h_n ^ h_g));
    let h_i = minimal(&BigUint::from_bytes_be(&hash(&[USERNAME.as_bytes()])));
    let h_identity = hash(&[USERNAME.as_bytes(), b":", TRANSIENT_PIN.as_bytes()]);
    Group {
        n,
        g,
        k,
        h_n_xor_h_g,
        h_i,
        h_identity,
    }
});

/// SHA-512 over the concatenation of `parts`.
fn hash(parts: &[&[u8]]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Minimal big-endian bytes (srptools `int_to_bytes`; zero is one `00` byte).
fn minimal(value: &BigUint) -> Vec<u8> {
    value.to_bytes_be()
}

/// Big-endian bytes left-padded with zeros to the modulus length.
fn pad(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    let mut out = vec![0u8; MODULUS_LEN.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

/// `k = H(N | PAD(g))` as minimal bytes.
pub fn multiplier() -> Vec<u8> {
    minimal(&GROUP.k)
}

/// `H(N) ^ H(g)` as minimal bytes (the form hashed into M1).
pub fn h_n_xor_h_g() -> Vec<u8> {
    GROUP.h_n_xor_h_g.clone()
}

/// `H("Pair-Setup")` as minimal bytes (the form hashed into M1).
pub fn h_i() -> Vec<u8> {
    GROUP.h_i.clone()
}

/// `x = H(s | H(I ":" P))` for a raw salt.
fn password_hash(salt: &[u8]) -> BigUint {
    BigUint::from_bytes_be(&hash(&[salt, &GROUP.h_identity]))
}

/// Password verifier `v = g^x mod N` for `salt`, as minimal bytes. Receivers store
/// this; exposed for tests and mock receivers.
pub fn verifier(salt: &[u8]) -> Vec<u8> {
    let group = &*GROUP;
    minimal(&group.g.modpow(&password_hash(salt), &group.n))
}

/// Errors from the SRP exchange.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SrpError {
    /// B % N == 0 (srptools rejects it; it would make the shared secret predictable).
    #[error("receiver sent an invalid SRP public key")]
    InvalidServerKey,
    /// The OS random generator failed.
    #[error("random number generator failed: {0}")]
    Random(String),
}

/// Client side of one SRP exchange: the private exponent `a` and `A = g^a mod N`.
pub struct SrpClient {
    private: BigUint,
    /// `A` in minimal form (what pyatv sends in M3).
    public: Vec<u8>,
}

impl std::fmt::Debug for SrpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the private exponent.
        f.debug_struct("SrpClient")
            .field("public_len", &self.public.len())
            .finish_non_exhaustive()
    }
}

impl SrpClient {
    /// Client with a given private exponent (big-endian bytes; leading zeros are
    /// irrelevant). Used by tests to reproduce fixtures.
    pub fn new(private_key: &[u8]) -> SrpClient {
        let group = &*GROUP;
        let private = BigUint::from_bytes_be(private_key);
        let public = minimal(&group.g.modpow(&private, &group.n));
        SrpClient { private, public }
    }

    /// Client with 32 random private bytes, regenerated until `A` is exactly 384
    /// bytes long (so M3 always carries a full-length public key).
    pub fn generate() -> Result<SrpClient, SrpError> {
        loop {
            let mut a = [0u8; PRIVATE_KEY_LEN];
            getrandom::fill(&mut a).map_err(|err| SrpError::Random(err.to_string()))?;
            let client = SrpClient::new(&a);
            if client.public.len() == MODULUS_LEN {
                return Ok(client);
            }
        }
    }

    /// `A` in minimal big-endian form.
    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Process the receiver's M2 (`salt` exactly as received, `B` as received) and
    /// compute the session key and both proofs.
    pub fn process(&self, salt: &[u8], server_public: &[u8]) -> Result<SrpProof, SrpError> {
        let group = &*GROUP;
        let b_pub = BigUint::from_bytes_be(server_public);
        if (&b_pub % &group.n).is_zero() {
            return Err(SrpError::InvalidServerKey);
        }
        let a_pub = BigUint::from_bytes_be(&self.public);

        let u = BigUint::from_bytes_be(&hash(&[&pad(&a_pub), &pad(&b_pub)]));
        let x = password_hash(salt);
        let v = group.g.modpow(&x, &group.n);

        // (B - k·v) mod N, kept non-negative like Python's pow() with a modulus.
        let kv = (&group.k * &v) % &group.n;
        let base = ((&b_pub % &group.n) + &group.n - kv) % &group.n;
        let exponent = &self.private + &u * &x;
        let premaster = base.modpow(&exponent, &group.n);

        let a_min = minimal(&a_pub);
        let b_min = minimal(&b_pub);
        let key = hash(&[&minimal(&premaster)]);
        let client_proof = hash(&[&group.h_n_xor_h_g, &group.h_i, salt, &a_min, &b_min, &key]);
        let server_proof = hash(&[&a_min, &client_proof, &key]);
        Ok(SrpProof {
            key,
            client_proof,
            server_proof,
            #[cfg(test)]
            debug: ProofInternals {
                x: minimal(&x),
                u: minimal(&u),
                premaster: minimal(&premaster),
            },
        })
    }
}

/// Intermediate values kept only for fixture tests.
#[cfg(test)]
struct ProofInternals {
    x: Vec<u8>,
    u: Vec<u8>,
    premaster: Vec<u8>,
}

/// Result of [`SrpClient::process`].
pub struct SrpProof {
    key: [u8; 64],
    client_proof: [u8; 64],
    server_proof: [u8; 64],
    #[cfg(test)]
    debug: ProofInternals,
}

impl std::fmt::Debug for SrpProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the session key.
        f.debug_struct("SrpProof").finish_non_exhaustive()
    }
}

impl SrpProof {
    /// `K = H(S)`, the 64-byte shared secret all HAP keys are derived from.
    pub fn session_key(&self) -> &[u8; 64] {
        &self.key
    }

    /// `M1`, sent to the receiver in M3.
    pub fn client_proof(&self) -> &[u8; 64] {
        &self.client_proof
    }

    /// `M2 = H(A | M1 | K)`, the proof a genuine receiver returns in M4.
    pub fn expected_server_proof(&self) -> &[u8; 64] {
        &self.server_proof
    }

    /// True when `proof` equals the expected `M2`.
    pub fn verify_server_proof(&self, proof: &[u8]) -> bool {
        proof == self.server_proof
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value as Json;

    fn fixture() -> Json {
        serde_json::from_str(include_str!("../tests/fixtures/srp.json")).expect("srp.json")
    }

    fn unhex(value: &Json) -> Vec<u8> {
        hex::decode(value.as_str().expect("hex string")).expect("valid hex")
    }

    #[test]
    fn constants_match_srptools() {
        let fx = fixture();
        let c = &fx["constants"];
        assert_eq!(PRIME_3072.to_vec(), unhex(&c["N"]));
        assert_eq!(u64::from(GENERATOR), c["g"].as_u64().expect("g"));
        assert_eq!(multiplier(), unhex(&c["k"]));
        assert_eq!(h_n_xor_h_g(), unhex(&c["H_N_xor_H_g"]));
        assert_eq!(h_i(), unhex(&c["H_I"]));
        assert_eq!(c["I"].as_str(), Some(USERNAME));
        assert_eq!(c["P"].as_str(), Some(TRANSIENT_PIN));
    }

    #[test]
    fn vectors_match_srptools() {
        let fx = fixture();
        let vectors = fx["vectors"].as_array().expect("vectors");
        assert_eq!(vectors.len(), 6);
        for v in vectors {
            let case = v["case"].as_str().expect("case");
            let client = SrpClient::new(&unhex(&v["a"]));
            assert_eq!(client.public_key(), unhex(&v["A"]).as_slice(), "{case}: A");
            let salt = unhex(&v["salt"]);
            assert_eq!(verifier(&salt), unhex(&v["verifier"]), "{case}: verifier");
            let proof = client.process(&salt, &unhex(&v["B"])).expect(case);
            assert_eq!(proof.debug.x, unhex(&v["x"]), "{case}: x");
            assert_eq!(proof.debug.u, unhex(&v["u"]), "{case}: u");
            assert_eq!(proof.debug.premaster, unhex(&v["S"]), "{case}: S");
            assert_eq!(proof.session_key().to_vec(), unhex(&v["K"]), "{case}: K");
            assert_eq!(proof.client_proof().to_vec(), unhex(&v["M1"]), "{case}: M1");
            assert_eq!(
                proof.expected_server_proof().to_vec(),
                unhex(&v["M2"]),
                "{case}: M2"
            );
            assert!(proof.verify_server_proof(&unhex(&v["M2"])), "{case}");
            assert!(!proof.verify_server_proof(&[0u8; 64]), "{case}");
        }
    }

    #[test]
    fn leading_zero_cases_exist_in_fixture() {
        let fx = fixture();
        let by_case = |name: &str| {
            fx["vectors"]
                .as_array()
                .expect("vectors")
                .iter()
                .find(|v| v["case"] == name)
                .cloned()
                .expect(name)
        };
        assert!(unhex(&by_case("salt_leading_zero")["salt"])[0] == 0);
        assert!(unhex(&by_case("A_leading_zero")["A"]).len() < MODULUS_LEN);
        assert!(unhex(&by_case("B_leading_zero")["B"]).len() < MODULUS_LEN);
        assert!(unhex(&by_case("S_leading_zero")["S"]).len() < MODULUS_LEN);
    }

    #[test]
    fn generated_client_has_full_length_public_key() {
        for _ in 0..4 {
            let client = SrpClient::generate().expect("rng");
            assert_eq!(client.public_key().len(), MODULUS_LEN);
            assert_ne!(client.public_key()[0], 0);
        }
    }

    #[test]
    fn rejects_server_key_that_is_a_multiple_of_n() {
        let client = SrpClient::new(&[7; 32]);
        assert_eq!(
            client.process(b"salt", &PRIME_3072).unwrap_err(),
            SrpError::InvalidServerKey
        );
        assert_eq!(
            client.process(b"salt", &[0]).unwrap_err(),
            SrpError::InvalidServerKey
        );
    }

    #[test]
    fn debug_output_hides_secrets() {
        let client = SrpClient::new(&[1; 32]);
        assert!(!format!("{client:?}").contains("private"));
    }
}
