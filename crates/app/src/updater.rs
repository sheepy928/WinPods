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
//!
//! Security notes: TLS (rustls, webpki roots) authenticates GitHub, but the
//! minisign signature over `SHA256SUMS.txt` is what makes an update trusted. The
//! signed trusted comment binds the checksums to one tag, and that tag must be newer
//! than the running version, so an old (validly signed) release can never be
//! replayed as a downgrade. Executables are only ever written as `.new` files next to
//! the exe and swapped in after both size and SHA-256 matched.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use minisign_verify::{PublicKey, Signature};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{paths, update_key, win};

/// Default repository.
pub const DEFAULT_REPO: &str = "sheepy928/WinPods";

/// Environment variable overriding [`DEFAULT_REPO`] (testing).
pub const REPO_ENV: &str = "WINPODS_UPDATE_REPO";
/// GitHub REST API base URL.
pub const GITHUB_API: &str = "https://api.github.com";
/// Release asset: the GUI executable.
pub const GUI_ASSET: &str = "WinPods.exe";
/// Release asset: the diagnostics CLI (updated only when it sits next to the exe).
pub const CLI_ASSET: &str = "winpods-cli.exe";
/// Release asset: SHA-256 checksums of the other assets.
pub const SUMS_ASSET: &str = "SHA256SUMS.txt";
/// Release asset: minisign signature of [`SUMS_ASSET`].
pub const SIG_ASSET: &str = "SHA256SUMS.txt.minisig";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const API_TIMEOUT: Duration = Duration::from_secs(20);
/// Whole-body budget for one executable download (per-read timeout still applies).
const DOWNLOAD_BODY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SWAP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `finish_post_update` waits for the previous process to let go of `.old`.
const POST_UPDATE_WAIT: Duration = Duration::from_secs(5);
const MAX_API_BYTES: u64 = 4 << 20;
const MAX_SMALL_ASSET_BYTES: u64 = 1 << 20;
const MAX_EXE_BYTES: u64 = 512 << 20;

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

/// Injectable settings behind [`Updater::with_options`]. Production code uses
/// [`Updater::new`]; tests point `api_base` at a loopback HTTP server.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct UpdaterOptions {
    /// API base URL (default [`GITHUB_API`]).
    pub api_base: String,
    /// Allow plain `http://` URLs, but only to loopback hosts (local test servers).
    /// Also disables proxies so loopback requests stay local.
    pub allow_http_loopback: bool,
    /// Trusted minisign public keys (base64; default [`update_key::UPDATE_PUBLIC_KEYS`]).
    pub public_keys: Vec<String>,
    /// Folder whose `WinPods.exe` (and `winpods-cli.exe`) are updated; `None` = the
    /// running executable and its folder.
    pub exe_dir: Option<PathBuf>,
    /// How long `install` retries a rename that fails with a sharing violation.
    pub swap_timeout: Duration,
}

impl Default for UpdaterOptions {
    fn default() -> Self {
        Self {
            api_base: GITHUB_API.to_string(),
            allow_http_loopback: false,
            public_keys: update_key::UPDATE_PUBLIC_KEYS
                .iter()
                .map(|k| k.to_string())
                .collect(),
            exe_dir: None,
            swap_timeout: SWAP_TIMEOUT,
        }
    }
}

pub struct Updater {
    agent: ureq::Agent,
    repo: String,
    current: Version,
    opts: UpdaterOptions,
}

impl Updater {
    /// `repo` like "owner/name" (use [`Updater::repo_from_env`] for the default).
    pub fn new(repo: &str, current: semver::Version) -> Updater {
        Self::with_options(repo, current, UpdaterOptions::default())
    }

    /// [`Updater::new`] with injectable endpoints, keys and folders (tests).
    #[doc(hidden)]
    pub fn with_options(repo: &str, current: Version, opts: UpdaterOptions) -> Updater {
        let mut config = ureq::Agent::config_builder()
            .https_only(!opts.allow_http_loopback)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_per_read(Some(READ_TIMEOUT))
            .user_agent(format!("WinPods/{current}"));
        if opts.allow_http_loopback {
            config = config.proxy(None);
        }
        Updater {
            agent: config.build().new_agent(),
            repo: repo.trim().to_string(),
            current,
            opts,
        }
    }

    /// `WINPODS_UPDATE_REPO` or [`DEFAULT_REPO`].
    pub fn repo_from_env() -> String {
        repo_from(std::env::var(REPO_ENV).ok())
    }

    /// Newest release if it is newer than the running version and not `skipped`.
    pub fn check(&self, skipped: Option<&str>) -> Result<Option<Release>, UpdateError> {
        if !is_valid_repo(&self.repo) {
            return Err(UpdateError::Other(format!(
                "invalid repository {:?} (expected owner/name)",
                self.repo
            )));
        }
        let url = format!(
            "{}/repos/{}/releases/latest",
            self.opts.api_base.trim_end_matches('/'),
            self.repo
        );
        self.ensure_allowed_url(&url)?;
        let response = self
            .agent
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .config()
            .timeout_global(Some(API_TIMEOUT))
            .build()
            .call();
        let mut response = match response {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => {
                tracing::info!("update check: {} has no published release", self.repo);
                return Ok(None);
            }
            Err(e) => return Err(network(e)),
        };
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_API_BYTES)
            .read_to_vec()
            .map_err(network)?;
        let parsed = parse_release(&body, &self.repo)?;
        Ok(evaluate(parsed, &self.current, skipped))
    }

    /// Download and verify (signature, size, hash) next to the running exe.
    pub fn stage(
        &self,
        release: &Release,
        progress: &dyn Fn(u64, u64),
    ) -> Result<StagedUpdate, UpdateError> {
        let (main_exe, dir) = self.targets()?;
        if self.opts.exe_dir.is_none() && paths::is_in_temp_dir(&main_exe) {
            return Err(UpdateError::NotWritable(
                "WinPods runs from a temporary folder (extract the zip first)".into(),
            ));
        }
        if !win::dir_writable(&dir) {
            return Err(UpdateError::NotWritable(format!(
                "{} is not writable",
                dir.display()
            )));
        }

        // 1. Signed checksums.
        let sums_asset = find_asset(release, SUMS_ASSET)
            .ok_or_else(|| UpdateError::Integrity(format!("the release has no {SUMS_ASSET}")))?;
        let sig_asset = find_asset(release, SIG_ASSET).ok_or_else(|| {
            UpdateError::Signature(format!("the release has no {SIG_ASSET} (unsigned)"))
        })?;
        let sums = self.fetch_small(&sums_asset.url)?;
        let sig = self.fetch_small(&sig_asset.url)?;
        let sig = String::from_utf8(sig)
            .map_err(|_| UpdateError::Signature(format!("{SIG_ASSET} is not text")))?;
        verify_signature(
            &sums,
            &sig,
            &self.opts.public_keys,
            &release.tag,
            &self.current,
        )?;
        let sums = std::str::from_utf8(&sums)
            .map_err(|_| UpdateError::Integrity(format!("{SUMS_ASSET} is not text")))?;
        let table = parse_sha256sums(sums)?;
        tracing::info!("update {}: checksums verified", release.tag);

        // 2. What to download.
        let mut plan = vec![(GUI_ASSET, main_exe)];
        let cli = dir.join(CLI_ASSET);
        if cli.is_file() {
            if find_asset(release, CLI_ASSET).is_some() {
                plan.push((CLI_ASSET, cli));
            } else {
                tracing::warn!("update {}: no {CLI_ASSET} asset, keeping it", release.tag);
            }
        }
        let mut items = Vec::with_capacity(plan.len());
        for (name, target) in plan {
            let asset = find_asset(release, name).ok_or_else(|| {
                UpdateError::Integrity(format!("the release has no {name} asset"))
            })?;
            let sha256 = *table.get(name).ok_or_else(|| {
                UpdateError::Integrity(format!("{SUMS_ASSET} has no entry for {name}"))
            })?;
            if asset.size == 0 || asset.size > MAX_EXE_BYTES {
                return Err(UpdateError::Integrity(format!(
                    "{name} has an implausible size ({} bytes)",
                    asset.size
                )));
            }
            items.push(DownloadItem {
                asset,
                sha256,
                staged: win::with_suffix(&target, ".new"),
                target,
            });
        }

        // 3. Download + verify each file; on any failure remove what was staged.
        let total: u64 = items.iter().map(|i| i.asset.size).sum();
        let mut done = 0u64;
        progress(0, total);
        let mut files = Vec::with_capacity(items.len());
        for item in &items {
            if let Err(e) = self.download(item, &mut done, total, progress) {
                for staged in items.iter().map(|i| &i.staged) {
                    let _ = fs::remove_file(staged);
                }
                return Err(e);
            }
            files.push((item.staged.clone(), item.target.clone()));
        }
        tracing::info!("update {}: {} file(s) staged", release.tag, files.len());
        Ok(StagedUpdate {
            release: release.clone(),
            files,
        })
    }

    /// Swap the staged files in (with rollback on failure).
    pub fn install(&self, staged: &StagedUpdate) -> Result<(), UpdateError> {
        swap_files(&staged.files, self.opts.swap_timeout)?;
        tracing::info!("update {}: installed", staged.release.tag);
        Ok(())
    }

    /// True when the exe folder is writable and the exe is not a temp copy.
    pub fn can_install_in_place() -> bool {
        let Ok(exe) = paths::exe_path() else {
            return false;
        };
        !paths::is_in_temp_dir(&exe) && exe.parent().is_some_and(win::dir_writable)
    }

    /// Delete leftover `.old` / `.new` files from a previous update.
    ///
    /// Right after a hand-off the previous process may still be exiting, so this waits
    /// up to ~5 s for its `.old` image to become deletable; call it off the UI thread.
    pub fn finish_post_update() {
        let Ok(exe) = paths::exe_path() else {
            return;
        };
        let Some(dir) = exe.parent() else {
            return;
        };
        let mut targets = vec![exe.clone()];
        let cli = dir.join(CLI_ASSET);
        if !win::same_path(&cli, &exe) {
            targets.push(cli);
        }
        cleanup_leftovers(&targets, POST_UPDATE_WAIT);
    }

    /// (file to replace as `WinPods.exe`, its folder).
    fn targets(&self) -> Result<(PathBuf, PathBuf), UpdateError> {
        if let Some(dir) = &self.opts.exe_dir {
            return Ok((dir.join(GUI_ASSET), dir.clone()));
        }
        let exe = paths::exe_path().map_err(|e| UpdateError::Other(e.to_string()))?;
        let dir = exe
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| UpdateError::Other("the executable has no parent folder".into()))?;
        Ok((exe, dir))
    }

    /// Only `https://` URLs, or `http://` to a loopback host in test mode.
    fn ensure_allowed_url(&self, url: &str) -> Result<(), UpdateError> {
        if url.starts_with("https://") {
            return Ok(());
        }
        if self.opts.allow_http_loopback
            && let Some(rest) = url.strip_prefix("http://")
            && is_loopback_authority(rest.split('/').next().unwrap_or(""))
        {
            return Ok(());
        }
        Err(UpdateError::Network(format!(
            "refusing a non-HTTPS URL: {url}"
        )))
    }

    /// GET a small asset (checksums, signature) into memory.
    fn fetch_small(&self, url: &str) -> Result<Vec<u8>, UpdateError> {
        self.ensure_allowed_url(url)?;
        let mut response = self
            .agent
            .get(url)
            .header("Accept", "application/octet-stream")
            .config()
            .timeout_global(Some(API_TIMEOUT))
            .build()
            .call()
            .map_err(network)?;
        response
            .body_mut()
            .with_config()
            .limit(MAX_SMALL_ASSET_BYTES)
            .read_to_vec()
            .map_err(network)
    }

    /// Stream one executable to its `.new` file, checking size and SHA-256.
    fn download(
        &self,
        item: &DownloadItem<'_>,
        done: &mut u64,
        total: u64,
        progress: &dyn Fn(u64, u64),
    ) -> Result<(), UpdateError> {
        let name = &item.asset.name;
        let expected = item.asset.size;
        self.ensure_allowed_url(&item.asset.url)?;
        let response = self
            .agent
            .get(&item.asset.url)
            .header("Accept", "application/octet-stream")
            .config()
            .timeout_recv_body(Some(DOWNLOAD_BODY_TIMEOUT))
            .build()
            .call()
            .map_err(network)?;
        if let Some(len) = response.body().content_length()
            && len != expected
        {
            return Err(UpdateError::Integrity(format!(
                "{name}: the server sends {len} bytes but the release lists {expected}"
            )));
        }
        let mut reader = response.into_body().into_reader();
        let mut file = File::create(&item.staged)
            .map_err(|e| UpdateError::NotWritable(format!("{}: {e}", item.staged.display())))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut received = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(UpdateError::Network(format!("{name}: {e}"))),
            };
            received += n as u64;
            if received > expected {
                return Err(UpdateError::Integrity(format!(
                    "{name}: more data than the {expected} bytes the release lists"
                )));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])
                .map_err(|e| UpdateError::Other(format!("{}: {e}", item.staged.display())))?;
            *done += n as u64;
            progress(*done, total);
        }
        if received != expected {
            return Err(UpdateError::Integrity(format!(
                "{name}: received {received} bytes, expected {expected}"
            )));
        }
        file.sync_all()
            .map_err(|e| UpdateError::Other(format!("{}: {e}", item.staged.display())))?;
        if hasher.finalize().as_slice() != item.sha256 {
            return Err(UpdateError::Integrity(format!(
                "{name}: SHA-256 does not match {SUMS_ASSET}"
            )));
        }
        Ok(())
    }
}

/// The running version (from Cargo).
pub fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("valid crate version")
}

/// One executable to download.
struct DownloadItem<'a> {
    asset: &'a ReleaseAsset,
    sha256: [u8; 32],
    staged: PathBuf,
    target: PathBuf,
}

fn network(e: ureq::Error) -> UpdateError {
    UpdateError::Network(e.to_string())
}

fn find_asset<'a>(release: &'a Release, name: &str) -> Option<&'a ReleaseAsset> {
    release.assets.iter().find(|a| a.name == name)
}

/// `value` when it is a valid "owner/name", else [`DEFAULT_REPO`].
fn repo_from(value: Option<String>) -> String {
    match value.map(|v| v.trim().to_string()) {
        Some(v) if is_valid_repo(&v) => v,
        Some(v) if !v.is_empty() => {
            tracing::warn!("ignoring invalid {REPO_ENV}={v:?}; using {DEFAULT_REPO}");
            DEFAULT_REPO.to_string()
        }
        _ => DEFAULT_REPO.to_string(),
    }
}

/// GitHub "owner/name": two non-empty parts of `[A-Za-z0-9._-]`, not `.`/`..`.
fn is_valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, name].iter().all(|p| {
        !p.is_empty()
            && *p != "."
            && *p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    })
}

/// `127.0.0.1[:port]`, `localhost[:port]` or `[::1][:port]`.
fn is_loopback_authority(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((host, _)) => host,
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Semver from a tag such as `v0.2.1` (a leading `v`/`V` is optional).
pub fn parse_tag(tag: &str) -> Option<Version> {
    let t = tag.trim();
    let t = t
        .strip_prefix('v')
        .or_else(|| t.strip_prefix('V'))
        .unwrap_or(t);
    Version::parse(t).ok()
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// A release from the API, before it is compared with the running version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRelease {
    release: Release,
    draft: bool,
    prerelease: bool,
}

/// Decode the `releases/latest` JSON. `html_url` is only kept when it is HTTPS (the
/// UI opens it in the browser); otherwise the repository's releases page is used.
fn parse_release(json: &[u8], repo: &str) -> Result<ParsedRelease, UpdateError> {
    let api: ApiRelease = serde_json::from_slice(json)
        .map_err(|e| UpdateError::Other(format!("unexpected release JSON: {e}")))?;
    let version = parse_tag(&api.tag_name).ok_or_else(|| {
        UpdateError::Other(format!("release tag {:?} is not a version", api.tag_name))
    })?;
    let html_url = api
        .html_url
        .filter(|u| u.starts_with("https://"))
        .unwrap_or_else(|| format!("https://github.com/{repo}/releases/latest"));
    Ok(ParsedRelease {
        release: Release {
            tag: api.tag_name,
            version,
            html_url,
            notes: api.body.unwrap_or_default(),
            assets: api
                .assets
                .into_iter()
                .map(|a| ReleaseAsset {
                    name: a.name,
                    url: a.browser_download_url,
                    size: a.size,
                })
                .collect(),
        },
        draft: api.draft,
        prerelease: api.prerelease,
    })
}

/// True when `skipped` names `tag` (with or without the `v`).
fn is_skipped(tag: &str, skipped: Option<&str>) -> bool {
    let Some(skipped) = skipped.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    if skipped.eq_ignore_ascii_case(tag.trim()) {
        return true;
    }
    matches!((parse_tag(skipped), parse_tag(tag)), (Some(a), Some(b)) if a == b)
}

/// Offer `parsed` only when it is a final release, newer than `current`, not skipped,
/// and carries every required asset.
fn evaluate(parsed: ParsedRelease, current: &Version, skipped: Option<&str>) -> Option<Release> {
    let release = parsed.release;
    if parsed.draft || parsed.prerelease {
        tracing::info!("update check: {} is a draft/pre-release", release.tag);
        return None;
    }
    if release.version <= *current {
        tracing::info!("update check: {} is not newer than {current}", release.tag);
        return None;
    }
    if is_skipped(&release.tag, skipped) {
        tracing::info!("update check: {} is skipped by the user", release.tag);
        return None;
    }
    let missing: Vec<&str> = [GUI_ASSET, SUMS_ASSET, SIG_ASSET]
        .into_iter()
        .filter(|name| find_asset(&release, name).is_none())
        .collect();
    if !missing.is_empty() {
        tracing::info!(
            "update check: {} has no {} yet",
            release.tag,
            missing.join(", ")
        );
        return None;
    }
    Some(release)
}

/// Check the minisign signature of `sums` against the trusted `keys`, then the signed
/// trusted comment: it must be exactly `WinPods <tag> windows-x64` and `tag` must be
/// newer than `current`.
fn verify_signature(
    sums: &[u8],
    minisig: &str,
    keys: &[String],
    tag: &str,
    current: &Version,
) -> Result<(), UpdateError> {
    if keys.is_empty() {
        return Err(UpdateError::Signature(
            "this build has no update signing key".into(),
        ));
    }
    let signature = Signature::decode(minisig)
        .map_err(|e| UpdateError::Signature(format!("malformed {SIG_ASSET}: {e}")))?;
    let mut last_error = None;
    let mut verified = false;
    for key in keys {
        match PublicKey::from_base64(key.trim()) {
            Ok(public_key) => match public_key.verify(sums, &signature, false) {
                Ok(()) => {
                    verified = true;
                    break;
                }
                Err(e) => last_error = Some(e.to_string()),
            },
            Err(e) => tracing::error!("ignoring an invalid built-in update key: {e}"),
        }
    }
    if !verified {
        return Err(UpdateError::Signature(format!(
            "{SUMS_ASSET} does not verify ({})",
            last_error.unwrap_or_else(|| "no usable key".into())
        )));
    }
    let expected = format!("WinPods {tag} windows-x64");
    let comment = signature.trusted_comment();
    if comment != expected {
        return Err(UpdateError::Signature(format!(
            "signed for {comment:?}, expected {expected:?}"
        )));
    }
    let version = parse_tag(tag)
        .ok_or_else(|| UpdateError::Signature(format!("tag {tag:?} is not a version")))?;
    if version <= *current {
        return Err(UpdateError::Signature(format!(
            "{tag} is not newer than the running version {current} (downgrade refused)"
        )));
    }
    Ok(())
}

/// Parse `sha256sum` output: `<64 hex>  <name>` (also `*name` binary-mode markers and
/// upper-case hex as PowerShell's Get-FileHash writes). Conflicting duplicates fail.
fn parse_sha256sums(text: &str) -> Result<HashMap<String, [u8; 32]>, UpdateError> {
    let mut table = HashMap::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || UpdateError::Integrity(format!("{SUMS_ASSET} line {}: malformed", number + 1));
        let (hash, name) = line.split_once(char::is_whitespace).ok_or_else(bad)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        let name = name.strip_prefix("./").unwrap_or(name);
        let mut digest = [0u8; 32];
        hex::decode_to_slice(hash, &mut digest).map_err(|_| bad())?;
        if name.is_empty() {
            return Err(bad());
        }
        if let Some(previous) = table.insert(name.to_string(), digest)
            && previous != digest
        {
            return Err(UpdateError::Integrity(format!(
                "{SUMS_ASSET} lists {name} twice with different hashes"
            )));
        }
    }
    Ok(table)
}

/// One file swapped by [`swap_files`] (kept for rollback).
struct Swapped<'a> {
    staged: &'a Path,
    target: &'a Path,
    old: PathBuf,
    had_old: bool,
}

/// `target → target.old`, `staged → target` for every pair, retrying sharing
/// violations for `timeout`. On failure everything done so far is renamed back.
fn swap_files(files: &[(PathBuf, PathBuf)], timeout: Duration) -> Result<(), UpdateError> {
    let mut done: Vec<Swapped<'_>> = Vec::with_capacity(files.len());
    for (staged, target) in files {
        let fail = |done: &[Swapped<'_>], what: String| {
            let rollback = rollback(done, timeout);
            tracing::error!("update swap failed: {what}");
            match rollback {
                Ok(()) => UpdateError::Swap(format!("{what}; nothing was changed")),
                Err(r) => UpdateError::Swap(format!("{what}; rollback failed: {r}")),
            }
        };
        if !staged.is_file() {
            return Err(fail(
                &done,
                format!("staged file {} is missing", staged.display()),
            ));
        }
        let old = win::with_suffix(target, ".old");
        match fs::remove_file(&old) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(fail(
                    &done,
                    format!("cannot remove leftover {}: {e}", old.display()),
                ));
            }
        }
        let had_old = target.exists();
        if had_old
            && let Err(e) = win::retry_sharing(timeout, || win::move_file(target, &old, false))
        {
            return Err(fail(
                &done,
                format!("cannot rename {}: {e}", target.display()),
            ));
        }
        if let Err(e) = win::retry_sharing(timeout, || win::move_file(staged, target, false)) {
            let mut what = format!("cannot move {} into place: {e}", staged.display());
            if had_old
                && let Err(r) = win::retry_sharing(timeout, || win::move_file(&old, target, false))
            {
                what.push_str(&format!(
                    "; could not restore {} from {}: {r}",
                    target.display(),
                    old.display()
                ));
            }
            return Err(fail(&done, what));
        }
        done.push(Swapped {
            staged,
            target,
            old,
            had_old,
        });
    }
    Ok(())
}

/// Undo [`swap_files`] steps in reverse: new exe back to `.new`, `.old` back home.
fn rollback(done: &[Swapped<'_>], timeout: Duration) -> Result<(), String> {
    let mut errors = Vec::new();
    for s in done.iter().rev() {
        if let Err(e) = win::retry_sharing(timeout, || win::move_file(s.target, s.staged, true)) {
            errors.push(format!("{}: {e}", s.target.display()));
            continue;
        }
        if s.had_old
            && let Err(e) = win::retry_sharing(timeout, || win::move_file(&s.old, s.target, false))
        {
            errors.push(format!("{}: {e}", s.old.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Delete `<target>.old` and `<target>.new` for each target, waiting up to `wait`
/// for files that are still in use.
fn cleanup_leftovers(targets: &[PathBuf], wait: Duration) {
    for target in targets {
        for suffix in [".old", ".new"] {
            let path = win::with_suffix(target, suffix);
            match win::retry_sharing(wait, || fs::remove_file(&path)) {
                Ok(()) => tracing::info!("removed {}", path.display()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("could not remove {}: {e}", path.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests;
