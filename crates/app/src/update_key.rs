//! Minisign public key(s) that release `SHA256SUMS.txt` files must be signed with.
//! The secret key lives only in the GitHub Actions `release` environment (and the
//! maintainer's offline backup). A second slot allows key rotation.

/// Base64 minisign public keys (the line after "untrusted comment:" in the .pub file).
pub const UPDATE_PUBLIC_KEYS: &[&str] = &[
    // WinPods release key, generated 2026-10-26.
    "RWR4XFKu09ClAU6GCwRPGdTypnI83Q+0G1DI3IfN32fhHMHojYpRHCcC",
];
