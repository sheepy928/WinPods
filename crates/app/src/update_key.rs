//! Minisign public key(s) that release `SHA256SUMS.txt` files must be signed with.
//! Filled in by the lead when the release key pair is generated; the secret key lives
//! only in the GitHub Actions `release` environment. A second slot allows rotation.

/// Base64 minisign public keys (the line after "untrusted comment:" in the .pub file).
pub const UPDATE_PUBLIC_KEYS: &[&str] = &[];
