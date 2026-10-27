//! Signed self-update from GitHub releases. CONTRACT.
//!
//! * One ureq Agent: https_only, connect 10 s, per-read 30 s, global 20 s for API
//!   calls (downloads use per-read + a generous body timeout), User-Agent
//!   `WinPods/<version>`.
//! * `check`: GET `https://api.github.com/repos/<repo>/releases/latest` (Accept
//!   `application/vnd.github+json`, X-GitHub-Api-Version). Newer semver tag than the
//!   running version and not skipped → `Some(Release)`. Missing assets → `None`.
//! * `stage`: download `SHA256SUMS.txt` and `SHA256SUMS.txt.minisig` (≤ 1 MB), verify
//!   with one of `update_key::UPDATE_PUBLIC_KEYS`; the trusted comment must be
//!   `WinPods <tag> windows-x64` and the tag newer than the running version (blocks
//!   downgrades). Then stream `WinPods.exe` (and `winpods-cli.exe` if it sits next to
//!   the exe) to `<name>.new`, hashing while downloading; size must equal the API size
//!   and SHA-256 must match the SHA256SUMS line. No signature → error (UI then only
//!   offers "open release page").
//! * `install`: rename swap while still running: `exe → exe.old`, `exe.new → exe`
//!   (retry ~10 s on sharing violations); on failure rename back and keep running.
//!   The caller then hands off (`WinPods.exe --post-update <pid>`) and exits.
//! * `finish_post_update`: delete leftover `.old`/`.new` files.
//! * `WINPODS_UPDATE_REPO` overrides the repository (testing).

use std::path::PathBuf;

/// Default repository.
pub const DEFAULT_REPO: &str = "sheepy928/WinPods";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: semver::Version,
    pub html_url: String,
    pub notes: String,
    /// Download URLs and sizes from the release assets.
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// Verified files waiting next to the running exe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    pub release: Release,
    /// (staged `.new` file, final path) pairs.
    pub files: Vec<(PathBuf, PathBuf)>,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Network(String),
    #[error("release is not signed with a trusted key: {0}")]
    Signature(String),
    #[error("downloaded file failed verification: {0}")]
    Integrity(String),
    #[error("cannot update in place: {0}")]
    NotWritable(String),
    #[error("could not replace the executable: {0}")]
    Swap(String),
    #[error("{0}")]
    Other(String),
}

pub struct Updater {
    _private: (),
}

impl Updater {
    /// `repo` like "owner/name" (use [`Updater::repo_from_env`] for the default).
    pub fn new(repo: &str, current: semver::Version) -> Updater {
        let _ = (repo, current);
        todo!("updater: new")
    }

    /// `WINPODS_UPDATE_REPO` or [`DEFAULT_REPO`].
    pub fn repo_from_env() -> String {
        todo!("updater: repo_from_env")
    }

    /// Newest release if it is newer than the running version and not `skipped`.
    pub fn check(&self, skipped: Option<&str>) -> Result<Option<Release>, UpdateError> {
        let _ = skipped;
        todo!("updater: check")
    }

    /// Download and verify (signature, size, hash) next to the running exe.
    pub fn stage(
        &self,
        release: &Release,
        progress: &dyn Fn(u64, u64),
    ) -> Result<StagedUpdate, UpdateError> {
        let _ = (release, progress);
        todo!("updater: stage")
    }

    /// Swap the staged files in (with rollback on failure).
    pub fn install(&self, staged: &StagedUpdate) -> Result<(), UpdateError> {
        let _ = staged;
        todo!("updater: install")
    }

    /// True when the exe folder is writable and the exe is not a temp copy.
    pub fn can_install_in_place() -> bool {
        todo!("updater: can_install_in_place")
    }

    /// Delete leftover `.old` / `.new` files from a previous update.
    pub fn finish_post_update() {
        todo!("updater: finish_post_update")
    }
}

/// The running version (from Cargo).
pub fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("valid crate version")
}
