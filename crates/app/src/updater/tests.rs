//! Updater tests against a loopback HTTP server (std `TcpListener` thread) with
//! releases signed by throw-away minisign keys. Every file lives in a temp folder;
//! the real executable is never touched and no external host is contacted.

use super::*;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Cursor};
use std::net::{TcpListener, TcpStream};
use std::os::windows::fs::OpenOptionsExt;
use std::sync::{Arc, Mutex};

use crate::test_support::EnvVarGuard;

const REPO: &str = "test-owner/WinPods";
const OLD_EXE: &[u8] = b"MZ old WinPods build";
const OLD_CLI: &[u8] = b"MZ old winpods-cli build";
/// `FILE_SHARE_READ` only: no `FILE_SHARE_DELETE`, so renames of the file fail.
const SHARE_READ_ONLY: u32 = 0x1;

// ---------------------------------------------------------------------------
// Loopback HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Route {
    status: u16,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Server {
    base: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    /// Request heads (request line + headers) in arrival order.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = Server {
            base: format!("http://{}", listener.local_addr().unwrap()),
            routes: Arc::default(),
            requests: Arc::default(),
        };
        let shared = server.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    let _ = shared.serve(stream);
                });
            }
        });
        server
    }

    fn route(&self, path: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().insert(
            path.to_string(),
            Route {
                status,
                body: body.into(),
            },
        );
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn serve(&self, stream: TcpStream) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        self.requests.lock().unwrap().push(head.clone());
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let route = self.routes.lock().unwrap().get(&path).cloned();
        let route = route.unwrap_or(Route {
            status: 404,
            body: b"{\"message\":\"Not Found\"}".to_vec(),
        });
        let mut out = stream;
        write!(
            out,
            "HTTP/1.1 {} {}\r\nContent-Type: application/octet-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            route.status,
            if route.status == 200 { "OK" } else { "Error" },
            route.body.len()
        )?;
        out.write_all(&route.body)?;
        out.flush()
    }
}

// ---------------------------------------------------------------------------
// Signed test releases
// ---------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn new_exe_bytes() -> Vec<u8> {
    // > 3 download chunks of 64 KiB
    (0..200_000u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect()
}

fn keypair() -> minisign::KeyPair {
    minisign::KeyPair::generate_unencrypted_keypair().unwrap()
}

/// What to publish; every field defaults to a correct, signed release.
struct Spec {
    tag: String,
    exe: Vec<u8>,
    cli: Option<Vec<u8>>,
    /// Size listed in the API for WinPods.exe (default: the real size).
    listed_size: Option<u64>,
    /// Bytes whose hash goes into SHA256SUMS for WinPods.exe (default: `exe`).
    hashed_exe: Option<Vec<u8>>,
    /// Replace the whole SHA256SUMS text (signed as given).
    sums_text: Option<String>,
    /// Serve this SHA256SUMS text instead of the signed one (tampering).
    served_sums: Option<String>,
    comment: Option<String>,
    signer: Option<minisign::KeyPair>,
    /// Asset names to list in the API (default: all).
    assets: Option<Vec<&'static str>>,
}

impl Spec {
    fn new(tag: &str) -> Spec {
        Spec {
            tag: tag.to_string(),
            exe: new_exe_bytes(),
            cli: None,
            listed_size: None,
            hashed_exe: None,
            sums_text: None,
            served_sums: None,
            comment: None,
            signer: None,
            assets: None,
        }
    }
}

struct Fixture {
    server: Server,
    dir: tempfile::TempDir,
    keys: minisign::KeyPair,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(GUI_ASSET), OLD_EXE).unwrap();
        Fixture {
            server: Server::start(),
            dir,
            keys: keypair(),
        }
    }

    fn exe(&self) -> PathBuf {
        self.dir.path().join(GUI_ASSET)
    }

    fn cli(&self) -> PathBuf {
        self.dir.path().join(CLI_ASSET)
    }

    fn options(&self) -> UpdaterOptions {
        UpdaterOptions {
            api_base: self.server.base.clone(),
            allow_http_loopback: true,
            public_keys: vec![self.keys.pk.to_base64()],
            exe_dir: Some(self.dir.path().to_path_buf()),
            swap_timeout: Duration::from_millis(300),
        }
    }

    fn updater(&self) -> Updater {
        Updater::with_options(REPO, Version::new(0, 2, 0), self.options())
    }

    /// Register the release on the server and return it as `check` would.
    fn publish(&self, spec: Spec) -> Release {
        let tag = &spec.tag;
        let hashed = spec.hashed_exe.as_deref().unwrap_or(&spec.exe);
        let sums = spec.sums_text.clone().unwrap_or_else(|| {
            let mut s = format!("{}  {GUI_ASSET}\n", sha256_hex(hashed));
            if let Some(cli) = &spec.cli {
                s.push_str(&format!("{} *{CLI_ASSET}\n", sha256_hex(cli)));
            }
            s
        });
        let comment = spec
            .comment
            .clone()
            .unwrap_or_else(|| format!("WinPods {tag} windows-x64"));
        let signer = spec.signer.as_ref().unwrap_or(&self.keys);
        let sig = minisign::sign(
            Some(&signer.pk),
            &signer.sk,
            Cursor::new(sums.as_bytes()),
            Some(&comment),
            Some("signature from a WinPods test key"),
        )
        .unwrap()
        .into_string();
        let served_sums = spec.served_sums.clone().unwrap_or(sums);

        let dl = |name: &str| format!("/dl/{tag}/{name}");
        self.server.route(&dl(GUI_ASSET), 200, spec.exe.clone());
        self.server.route(&dl(SUMS_ASSET), 200, served_sums);
        self.server.route(&dl(SIG_ASSET), 200, sig);
        if let Some(cli) = &spec.cli {
            self.server.route(&dl(CLI_ASSET), 200, cli.clone());
        }

        let mut all = vec![
            (GUI_ASSET, spec.listed_size.unwrap_or(spec.exe.len() as u64)),
            (SUMS_ASSET, 100),
            (SIG_ASSET, 100),
        ];
        if let Some(cli) = &spec.cli {
            all.push((CLI_ASSET, cli.len() as u64));
        }
        let assets: Vec<serde_json::Value> = all
            .into_iter()
            .filter(|(name, _)| spec.assets.as_ref().is_none_or(|keep| keep.contains(name)))
            .map(|(name, size)| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": self.server.url(&dl(name)),
                    "size": size,
                })
            })
            .collect();
        let json = serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/{REPO}/releases/tag/{tag}"),
            "body": format!("Release notes for {tag}"),
            "draft": false,
            "prerelease": false,
            "assets": assets,
        });
        let json = serde_json::to_vec(&json).unwrap();
        self.server
            .route(&format!("/repos/{REPO}/releases/latest"), 200, json.clone());
        parse_release(&json, REPO).unwrap().release
    }

    fn staged_files(&self) -> Vec<String> {
        fs::read_dir(self.dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".new") || n.ends_with(".old"))
            .collect()
    }
}

fn no_progress(_: u64, _: u64) {}

fn expect_signature_error(result: Result<StagedUpdate, UpdateError>) -> String {
    match result {
        Err(UpdateError::Signature(msg)) => msg,
        other => panic!("expected a signature error, got {other:?}"),
    }
}

fn expect_integrity_error(result: Result<StagedUpdate, UpdateError>) -> String {
    match result {
        Err(UpdateError::Integrity(msg)) => msg,
        other => panic!("expected an integrity error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

#[test]
fn check_finds_a_newer_release_and_sends_the_api_headers() {
    let fx = Fixture::new();
    let published = fx.publish(Spec::new("v0.3.0"));
    let found = fx.updater().check(None).unwrap().expect("an update");
    assert_eq!(found, published);
    assert_eq!(found.version, Version::new(0, 3, 0));
    assert_eq!(found.notes, "Release notes for v0.3.0");
    assert!(found.html_url.starts_with("https://github.com/"));

    let requests = fx.server.requests();
    let head = requests
        .iter()
        .find(|r| r.starts_with(&format!("GET /repos/{REPO}/releases/latest ")))
        .expect("API request")
        .to_ascii_lowercase();
    assert!(head.contains("user-agent: winpods/0.2.0\r\n"), "{head}");
    assert!(
        head.contains("accept: application/vnd.github+json\r\n"),
        "{head}"
    );
    assert!(
        head.contains("x-github-api-version: 2022-11-28\r\n"),
        "{head}"
    );
}

#[test]
fn check_ignores_same_and_older_versions() {
    let fx = Fixture::new();
    fx.publish(Spec::new("v0.2.0"));
    assert_eq!(fx.updater().check(None).unwrap(), None);
    fx.publish(Spec::new("v0.1.9"));
    assert_eq!(fx.updater().check(None).unwrap(), None);
}

#[test]
fn check_honours_the_skipped_version() {
    let fx = Fixture::new();
    fx.publish(Spec::new("v0.3.0"));
    let updater = fx.updater();
    assert_eq!(updater.check(Some("v0.3.0")).unwrap(), None);
    assert_eq!(updater.check(Some("0.3.0")).unwrap(), None);
    assert!(updater.check(Some("v0.2.5")).unwrap().is_some());
    assert!(updater.check(Some("")).unwrap().is_some());
}

#[test]
fn check_treats_missing_payload_assets_as_no_update_yet() {
    for keep in [vec![SUMS_ASSET, SIG_ASSET], vec![GUI_ASSET, SIG_ASSET]] {
        let fx = Fixture::new();
        let mut spec = Spec::new("v0.3.0");
        spec.assets = Some(keep.clone());
        fx.publish(spec);
        assert_eq!(fx.updater().check(None).unwrap(), None, "{keep:?}");
    }
}

#[test]
fn check_offers_an_unsigned_release_that_stage_refuses() {
    // Plan §0.2: until the signing secrets exist CI publishes without the .minisig;
    // the UI must still learn about the release to offer "open release page".
    let fx = Fixture::new();
    let mut spec = Spec::new("v0.3.0");
    spec.assets = Some(vec![GUI_ASSET, SUMS_ASSET]);
    fx.publish(spec);
    let updater = fx.updater();
    let release = updater.check(None).unwrap().expect("an unsigned update");
    assert!(!release.is_signed());
    assert_eq!(release.version, Version::new(0, 3, 0));

    let msg = expect_signature_error(updater.stage(&release, &no_progress));
    assert!(msg.contains("unsigned"), "{msg}");
    assert!(fx.staged_files().is_empty());
    assert_eq!(fs::read(fx.exe()).unwrap(), OLD_EXE);
    assert!(
        !fx.server.requests().iter().any(|r| r.contains("/dl/")),
        "nothing is downloaded for an unsigned release"
    );

    // A complete release reports its signature asset.
    let signed = fx.publish(Spec::new("v0.3.0"));
    assert!(signed.is_signed());
}

#[test]
fn check_without_any_release_is_none() {
    let fx = Fixture::new(); // no route → 404
    assert_eq!(fx.updater().check(None).unwrap(), None);
}

#[test]
fn check_reports_bad_json_and_server_errors() {
    let fx = Fixture::new();
    let path = format!("/repos/{REPO}/releases/latest");
    fx.server.route(&path, 200, "not json");
    assert!(matches!(
        fx.updater().check(None),
        Err(UpdateError::Other(_))
    ));
    fx.server.route(&path, 500, "boom");
    assert!(matches!(
        fx.updater().check(None),
        Err(UpdateError::Network(_))
    ));
}

#[test]
fn check_rejects_an_invalid_repository_without_a_request() {
    let fx = Fixture::new();
    let updater = Updater::with_options("../../evil", Version::new(0, 2, 0), fx.options());
    assert!(matches!(updater.check(None), Err(UpdateError::Other(_))));
    assert!(fx.server.requests().is_empty());
}

#[test]
fn production_agent_refuses_plain_http() {
    let fx = Fixture::new();
    fx.publish(Spec::new("v0.3.0"));
    let opts = UpdaterOptions {
        allow_http_loopback: false,
        ..fx.options()
    };
    let updater = Updater::with_options(REPO, Version::new(0, 2, 0), opts);
    assert!(matches!(updater.check(None), Err(UpdateError::Network(_))));
    assert!(fx.server.requests().is_empty());
}

#[test]
fn test_mode_still_refuses_plain_http_to_other_hosts() {
    let fx = Fixture::new();
    let mut release = fx.publish(Spec::new("v0.3.0"));
    for asset in &mut release.assets {
        asset.url = format!("http://example.invalid/{}", asset.name);
    }
    let result = fx.updater().stage(&release, &no_progress);
    assert!(matches!(result, Err(UpdateError::Network(_))), "{result:?}");
}

// ---------------------------------------------------------------------------
// stage + install
// ---------------------------------------------------------------------------

#[test]
fn stage_and_install_replace_the_exe() {
    let fx = Fixture::new();
    let release = fx.publish(Spec::new("v0.3.0"));
    let updater = fx.updater();

    let calls = Mutex::new(Vec::new());
    let progress = |done: u64, total: u64| calls.lock().unwrap().push((done, total));
    let staged = updater.stage(&release, &progress).unwrap();

    let new_path = win::with_suffix(&fx.exe(), ".new");
    assert_eq!(staged.files, vec![(new_path.clone(), fx.exe())]);
    assert_eq!(staged.release, release);
    assert_eq!(fs::read(&new_path).unwrap(), new_exe_bytes());
    assert_eq!(fs::read(fx.exe()).unwrap(), OLD_EXE);

    let calls = calls.into_inner().unwrap();
    let total = new_exe_bytes().len() as u64;
    assert_eq!(calls.first(), Some(&(0, total)));
    assert_eq!(calls.last(), Some(&(total, total)));
    assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
    assert!(calls.len() >= 4, "streamed in chunks: {calls:?}");

    updater.install(&staged).unwrap();
    assert_eq!(fs::read(fx.exe()).unwrap(), new_exe_bytes());
    assert_eq!(
        fs::read(win::with_suffix(&fx.exe(), ".old")).unwrap(),
        OLD_EXE
    );
    assert!(!new_path.exists());

    cleanup_leftovers(&[fx.exe()], Duration::from_secs(1));
    assert!(fx.staged_files().is_empty());
}

#[test]
fn stage_also_updates_a_cli_next_to_the_exe() {
    let fx = Fixture::new();
    fs::write(fx.cli(), OLD_CLI).unwrap();
    let mut spec = Spec::new("v0.3.0");
    spec.cli = Some(b"MZ new cli".to_vec());
    let release = fx.publish(spec);
    let updater = fx.updater();
    let staged = updater.stage(&release, &no_progress).unwrap();
    assert_eq!(staged.files.len(), 2);
    updater.install(&staged).unwrap();
    assert_eq!(fs::read(fx.exe()).unwrap(), new_exe_bytes());
    assert_eq!(fs::read(fx.cli()).unwrap(), b"MZ new cli");
}

#[test]
fn stage_skips_the_cli_when_the_release_has_none() {
    let fx = Fixture::new();
    fs::write(fx.cli(), OLD_CLI).unwrap();
    let release = fx.publish(Spec::new("v0.3.0"));
    let staged = fx.updater().stage(&release, &no_progress).unwrap();
    assert_eq!(staged.files.len(), 1);
    assert_eq!(staged.files[0].1, fx.exe());
}

#[test]
fn stage_rejects_a_tampered_checksum_file() {
    let fx = Fixture::new();
    let mut spec = Spec::new("v0.3.0");
    // The genuine checksums are signed; an attacker serves their own binary and its
    // hash but cannot re-sign.
    let evil = b"MZ evil".to_vec();
    spec.hashed_exe = Some(new_exe_bytes());
    spec.served_sums = Some(format!("{}  {GUI_ASSET}\n", sha256_hex(&evil)));
    spec.exe = evil;
    let release = fx.publish(spec);
    expect_signature_error(fx.updater().stage(&release, &no_progress));
    assert!(fx.staged_files().is_empty());
}

#[test]
fn stage_rejects_a_signature_from_another_key() {
    let fx = Fixture::new();
    let mut spec = Spec::new("v0.3.0");
    spec.signer = Some(keypair());
    let release = fx.publish(spec);
    expect_signature_error(fx.updater().stage(&release, &no_progress));
}

#[test]
fn stage_accepts_the_second_of_two_trusted_keys() {
    let fx = Fixture::new();
    let release = fx.publish(Spec::new("v0.3.0"));
    let mut opts = fx.options();
    opts.public_keys = vec![keypair().pk.to_base64(), fx.keys.pk.to_base64()];
    let updater = Updater::with_options(REPO, Version::new(0, 2, 0), opts);
    assert!(updater.stage(&release, &no_progress).is_ok());
}

#[test]
fn stage_rejects_a_wrong_trusted_comment() {
    for comment in [
        "WinPods v0.4.0 windows-x64",
        "WinPods v0.3.0",
        "WinPods v0.3.0 windows-arm64",
        "timestamp:1700000000",
    ] {
        let fx = Fixture::new();
        let mut spec = Spec::new("v0.3.0");
        spec.comment = Some(comment.to_string());
        let release = fx.publish(spec);
        let msg = expect_signature_error(fx.updater().stage(&release, &no_progress));
        assert!(msg.contains("expected"), "{msg}");
    }
}

#[test]
fn stage_refuses_a_validly_signed_downgrade() {
    for tag in ["v0.1.0", "v0.2.0"] {
        let fx = Fixture::new();
        // `check` would never offer these; a crafted Release must still be refused.
        let release = fx.publish(Spec::new(tag));
        let msg = expect_signature_error(fx.updater().stage(&release, &no_progress));
        assert!(msg.contains("downgrade"), "{msg}");
    }
}

#[test]
fn stage_without_built_in_keys_is_a_signature_error() {
    let fx = Fixture::new();
    let release = fx.publish(Spec::new("v0.3.0"));
    let mut opts = fx.options();
    opts.public_keys.clear();
    let updater = Updater::with_options(REPO, Version::new(0, 2, 0), opts);
    expect_signature_error(updater.stage(&release, &no_progress));
    // The shipped key list is empty until the release key exists.
    if update_key::UPDATE_PUBLIC_KEYS.is_empty() {
        assert!(UpdaterOptions::default().public_keys.is_empty());
    }
}

#[test]
fn stage_rejects_a_size_mismatch() {
    let fx = Fixture::new();
    let mut spec = Spec::new("v0.3.0");
    spec.listed_size = Some(new_exe_bytes().len() as u64 + 1);
    let release = fx.publish(spec);
    let msg = expect_integrity_error(fx.updater().stage(&release, &no_progress));
    assert!(msg.contains("bytes"), "{msg}");
    assert!(fx.staged_files().is_empty());
    assert_eq!(fs::read(fx.exe()).unwrap(), OLD_EXE);
}

#[test]
fn stage_rejects_a_hash_mismatch() {
    let fx = Fixture::new();
    let mut spec = Spec::new("v0.3.0");
    spec.hashed_exe = Some(b"some other build".to_vec());
    let release = fx.publish(spec);
    let msg = expect_integrity_error(fx.updater().stage(&release, &no_progress));
    assert!(msg.contains("SHA-256"), "{msg}");
    assert!(fx.staged_files().is_empty());
}

#[test]
fn stage_removes_earlier_files_when_a_later_one_fails() {
    let fx = Fixture::new();
    fs::write(fx.cli(), OLD_CLI).unwrap();
    let mut spec = Spec::new("v0.3.0");
    let cli = b"MZ new cli".to_vec();
    // Correct entry for the exe, wrong one for the CLI.
    spec.sums_text = Some(format!(
        "{}  {GUI_ASSET}\n{}  {CLI_ASSET}\n",
        sha256_hex(&new_exe_bytes()),
        sha256_hex(b"not the cli")
    ));
    spec.cli = Some(cli);
    let release = fx.publish(spec);
    expect_integrity_error(fx.updater().stage(&release, &no_progress));
    assert!(fx.staged_files().is_empty());
}

#[test]
fn stage_reports_missing_assets() {
    let fx = Fixture::new();
    let full = fx.publish(Spec::new("v0.3.0"));
    let without = |name: &str| Release {
        assets: full
            .assets
            .iter()
            .filter(|a| a.name != name)
            .cloned()
            .collect(),
        ..full.clone()
    };
    let updater = fx.updater();
    expect_signature_error(updater.stage(&without(SIG_ASSET), &no_progress));
    // unsigned wins over the missing checksums (UI: "open release page")
    let bare = Release {
        assets: without(SIG_ASSET)
            .assets
            .into_iter()
            .filter(|a| a.name != SUMS_ASSET)
            .collect(),
        ..full.clone()
    };
    expect_signature_error(updater.stage(&bare, &no_progress));
    expect_integrity_error(updater.stage(&without(SUMS_ASSET), &no_progress));
    expect_integrity_error(updater.stage(&without(GUI_ASSET), &no_progress));

    let mut spec = Spec::new("v0.3.0");
    spec.sums_text = Some(format!("{}  other.zip\n", sha256_hex(b"zip")));
    let release = fx.publish(spec);
    let msg = expect_integrity_error(updater.stage(&release, &no_progress));
    assert!(msg.contains("no entry"), "{msg}");
}

#[test]
fn stage_refuses_a_missing_exe_folder() {
    let fx = Fixture::new();
    let release = fx.publish(Spec::new("v0.3.0"));
    let mut opts = fx.options();
    opts.exe_dir = Some(fx.dir.path().join("missing"));
    let updater = Updater::with_options(REPO, Version::new(0, 2, 0), opts);
    assert!(matches!(
        updater.stage(&release, &no_progress),
        Err(UpdateError::NotWritable(_))
    ));
}

// ---------------------------------------------------------------------------
// swap
// ---------------------------------------------------------------------------

fn staged_pair(dir: &Path, name: &str, old: &[u8], new: &[u8]) -> (PathBuf, PathBuf) {
    let target = dir.join(name);
    fs::write(&target, old).unwrap();
    let staged = win::with_suffix(&target, ".new");
    fs::write(&staged, new).unwrap();
    (staged, target)
}

#[test]
fn swap_without_an_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(CLI_ASSET);
    let staged = win::with_suffix(&target, ".new");
    fs::write(&staged, b"new").unwrap();
    swap_files(
        &[(staged.clone(), target.clone())],
        Duration::from_millis(300),
    )
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"new");
    assert!(!staged.exists());
    assert!(!win::with_suffix(&target, ".old").exists());
}

#[test]
fn swap_replaces_a_leftover_old_file() {
    let dir = tempfile::tempdir().unwrap();
    let pair = staged_pair(dir.path(), GUI_ASSET, b"old", b"new");
    let old = win::with_suffix(&pair.1, ".old");
    fs::write(&old, b"older").unwrap();
    swap_files(std::slice::from_ref(&pair), Duration::from_millis(300)).unwrap();
    assert_eq!(fs::read(&pair.1).unwrap(), b"new");
    assert_eq!(fs::read(&old).unwrap(), b"old");
}

#[test]
fn swap_fails_cleanly_when_the_target_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let (staged, target) = staged_pair(dir.path(), GUI_ASSET, b"old", b"new");
    let _hold = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_ONLY)
        .open(&target)
        .unwrap();
    let start = std::time::Instant::now();
    let err = swap_files(
        &[(staged.clone(), target.clone())],
        Duration::from_millis(300),
    );
    assert!(matches!(&err, Err(UpdateError::Swap(msg)) if msg.contains("nothing was changed")));
    assert!(start.elapsed() >= Duration::from_millis(300), "retried");
    assert_eq!(fs::read(&target).unwrap(), b"old");
    assert_eq!(fs::read(&staged).unwrap(), b"new");
    assert!(!win::with_suffix(&target, ".old").exists());
}

#[test]
fn swap_rolls_back_when_the_staged_file_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let (staged, target) = staged_pair(dir.path(), GUI_ASSET, b"old", b"new");
    let _hold = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_ONLY)
        .open(&staged)
        .unwrap();
    let err = swap_files(
        &[(staged.clone(), target.clone())],
        Duration::from_millis(300),
    );
    assert!(matches!(err, Err(UpdateError::Swap(_))));
    // exe → exe.old happened, then was renamed back
    assert_eq!(fs::read(&target).unwrap(), b"old");
    assert!(!win::with_suffix(&target, ".old").exists());
}

#[test]
fn swap_rolls_back_earlier_files_when_a_later_one_fails() {
    let dir = tempfile::tempdir().unwrap();
    let first = staged_pair(dir.path(), GUI_ASSET, b"old gui", b"new gui");
    let second = staged_pair(dir.path(), CLI_ASSET, b"old cli", b"new cli");
    let _hold = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_ONLY)
        .open(&second.1)
        .unwrap();
    let fx_updater = Updater::with_options(
        REPO,
        Version::new(0, 2, 0),
        UpdaterOptions {
            exe_dir: Some(dir.path().to_path_buf()),
            swap_timeout: Duration::from_millis(300),
            ..UpdaterOptions::default()
        },
    );
    let staged = StagedUpdate {
        release: Release {
            tag: "v0.3.0".into(),
            version: Version::new(0, 3, 0),
            html_url: String::new(),
            notes: String::new(),
            assets: Vec::new(),
        },
        files: vec![first.clone(), second.clone()],
    };
    let err = fx_updater.install(&staged);
    assert!(matches!(&err, Err(UpdateError::Swap(msg)) if msg.contains("nothing was changed")));
    // the first file is back to the old build, its staged copy restored
    assert_eq!(fs::read(&first.1).unwrap(), b"old gui");
    assert_eq!(fs::read(&first.0).unwrap(), b"new gui");
    assert!(!win::with_suffix(&first.1, ".old").exists());
    // the locked file is untouched
    assert_eq!(fs::read(&second.1).unwrap(), b"old cli");
    assert_eq!(fs::read(&second.0).unwrap(), b"new cli");
}

#[test]
fn swap_reports_a_missing_staged_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(GUI_ASSET);
    fs::write(&target, b"old").unwrap();
    let staged = win::with_suffix(&target, ".new");
    let err = swap_files(&[(staged, target.clone())], Duration::from_millis(100));
    assert!(matches!(err, Err(UpdateError::Swap(_))));
    assert_eq!(fs::read(&target).unwrap(), b"old");
}

#[test]
fn cleanup_removes_old_and_new_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join(GUI_ASSET);
    let cli = dir.path().join(CLI_ASSET);
    for p in [&exe, &cli] {
        fs::write(p, b"current").unwrap();
        fs::write(win::with_suffix(p, ".old"), b"old").unwrap();
        fs::write(win::with_suffix(p, ".new"), b"new").unwrap();
    }
    cleanup_leftovers(&[exe.clone(), cli.clone()], Duration::from_millis(200));
    let mut left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, vec![GUI_ASSET.to_string(), CLI_ASSET.to_string()]);
}

// ---------------------------------------------------------------------------
// pure helpers
// ---------------------------------------------------------------------------

#[test]
fn sha256sums_parsing() {
    let a = sha256_hex(b"a");
    let b = sha256_hex(b"b").to_uppercase();
    let text = format!("# comment\n\n{a}  WinPods.exe\r\n{b} *winpods-cli.exe\n{a}  ./x.zip\n");
    let table = parse_sha256sums(&text).unwrap();
    assert_eq!(table.len(), 3);
    assert_eq!(hex::encode(table["WinPods.exe"]), a);
    assert_eq!(hex::encode(table["winpods-cli.exe"]), b.to_lowercase());
    assert!(table.contains_key("x.zip"));

    // identical duplicates are fine, conflicting ones are not
    assert!(parse_sha256sums(&format!("{a}  f\n{a}  f\n")).is_ok());
    assert!(parse_sha256sums(&format!("{a}  f\n{b}  f\n")).is_err());
    for bad in ["abc  f", "nothing", a.as_str(), &format!("{a}  ")] {
        assert!(parse_sha256sums(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn tags_parse_with_or_without_v() {
    assert_eq!(parse_tag("v0.2.1"), Some(Version::new(0, 2, 1)));
    assert_eq!(parse_tag("0.2.1"), Some(Version::new(0, 2, 1)));
    assert_eq!(parse_tag("V1.0.0"), Some(Version::new(1, 0, 0)));
    assert!(!parse_tag("v1.0.0-beta.1").unwrap().pre.is_empty());
    assert_eq!(parse_tag("release-1"), None);
    assert_eq!(parse_tag("v1.0"), None);
}

#[test]
fn skipped_matching() {
    assert!(is_skipped("v0.3.0", Some("v0.3.0")));
    assert!(is_skipped("v0.3.0", Some("0.3.0")));
    assert!(is_skipped("v0.3.0", Some(" V0.3.0 ")));
    assert!(!is_skipped("v0.3.0", Some("v0.3.1")));
    assert!(!is_skipped("v0.3.0", None));
    assert!(!is_skipped("v0.3.0", Some("")));
}

#[test]
fn drafts_and_prereleases_are_never_offered() {
    let release = Release {
        tag: "v9.0.0".into(),
        version: Version::new(9, 0, 0),
        html_url: String::new(),
        notes: String::new(),
        assets: [GUI_ASSET, SUMS_ASSET, SIG_ASSET]
            .iter()
            .map(|n| ReleaseAsset {
                name: n.to_string(),
                url: String::new(),
                size: 1,
            })
            .collect(),
    };
    let current = Version::new(0, 2, 0);
    let make = |draft, prerelease| ParsedRelease {
        release: release.clone(),
        draft,
        prerelease,
    };
    assert!(evaluate(make(false, false), &current, None).is_some());
    assert!(evaluate(make(true, false), &current, None).is_none());
    assert!(evaluate(make(false, true), &current, None).is_none());
}

#[test]
fn release_json_sanitises_the_page_url() {
    let json = br#"{"tag_name":"v1.2.3","html_url":"file:///C:/evil","body":null,"assets":[]}"#;
    let parsed = parse_release(json, REPO).unwrap();
    assert_eq!(
        parsed.release.html_url,
        format!("https://github.com/{REPO}/releases/latest")
    );
    assert_eq!(parsed.release.notes, "");
    assert!(parse_release(br#"{"tag_name":"nightly"}"#, REPO).is_err());
}

#[test]
fn malformed_signatures_are_signature_errors() {
    let current = Version::new(0, 2, 0);
    let keys = vec![keypair().pk.to_base64()];
    assert!(matches!(
        verify_signature(b"x", "garbage", &keys, "v0.3.0", &current),
        Err(UpdateError::Signature(_))
    ));
    assert!(matches!(
        verify_signature(b"x", "garbage", &[], "v0.3.0", &current),
        Err(UpdateError::Signature(_))
    ));
    // an invalid built-in key is skipped, leaving no usable key
    let kp = keypair();
    let sig = minisign::sign(
        Some(&kp.pk),
        &kp.sk,
        Cursor::new(b"x"),
        Some("WinPods v0.3.0 windows-x64"),
        None,
    )
    .unwrap()
    .into_string();
    let msg = match verify_signature(b"x", &sig, &["not a key".into()], "v0.3.0", &current) {
        Err(UpdateError::Signature(msg)) => msg,
        other => panic!("{other:?}"),
    };
    assert!(msg.contains("no usable key"), "{msg}");
    verify_signature(b"x", &sig, &[kp.pk.to_base64()], "v0.3.0", &current).unwrap();
}

#[test]
fn repository_validation_and_env_override() {
    assert!(is_valid_repo("sheepy928/WinPods"));
    assert!(is_valid_repo("a-b/c.d_e"));
    for bad in [
        "", "a", "a/", "/b", "a/b/c", "../b", "a/..", "a b/c", "a/b?x",
    ] {
        assert!(!is_valid_repo(bad), "{bad:?}");
    }
    assert_eq!(repo_from(None), DEFAULT_REPO);
    assert_eq!(repo_from(Some(" me/fork ".into())), "me/fork");
    assert_eq!(repo_from(Some("not a repo".into())), DEFAULT_REPO);
    {
        let _env = EnvVarGuard::set(REPO_ENV, "someone/WinPods-test");
        assert_eq!(Updater::repo_from_env(), "someone/WinPods-test");
    }
    let _env = EnvVarGuard::unset(REPO_ENV);
    assert_eq!(Updater::repo_from_env(), DEFAULT_REPO);
}

#[test]
fn loopback_authorities() {
    assert!(is_loopback_authority("127.0.0.1:8080"));
    assert!(is_loopback_authority("localhost"));
    assert!(is_loopback_authority("[::1]:80"));
    assert!(!is_loopback_authority("example.com"));
    assert!(!is_loopback_authority("127.0.0.1.evil.com"));
    assert!(!is_loopback_authority("[::2]"));
}

#[test]
fn shared_types_are_thread_safe() {
    // The UI keeps `Arc<Updater>` and runs checks/downloads on worker threads.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Updater>();
    assert_send_sync::<UpdateError>();
    assert_send_sync::<StagedUpdate>();
    fn assert_send<T: Send>() {}
    assert_send::<crate::single_instance::InstanceGuard>();
    assert_send::<crate::logging::LogGuard>();
}

#[test]
fn version_and_defaults() {
    assert_eq!(
        current_version(),
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
    );
    let opts = UpdaterOptions::default();
    assert_eq!(opts.api_base, GITHUB_API);
    assert!(!opts.allow_http_loopback);
    assert_eq!(opts.exe_dir, None);
    assert_eq!(opts.swap_timeout, Duration::from_secs(10));
    // The test binary is not a temp copy and target\ is writable.
    assert!(Updater::can_install_in_place());
}
