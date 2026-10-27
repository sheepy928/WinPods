//! Release signing helper (not shipped).
//!
//! ```text
//! winpods-sign keygen <out-dir>                    # writes winpods-update.pub / .key
//! winpods-sign sign <key-file|-> <file> <comment>  # writes <file>.minisig
//! winpods-sign verify <pubkey-base64> <file>       # checks <file>.minisig
//! ```
//! The secret key password comes from `MINISIGN_PASSWORD`. With `-` as key file the
//! secret key text is read from `MINISIGN_SECRET_KEY` (GitHub Actions secret).

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("keygen") if args.len() == 2 => keygen(Path::new(&args[1])),
        Some("sign") if args.len() == 4 => sign(&args[1], Path::new(&args[2]), &args[3]),
        Some("verify") if args.len() == 3 => verify(&args[1], Path::new(&args[2])),
        _ => bail!(
            "usage: winpods-sign keygen <out-dir> | sign <key-file|-> <file> <trusted-comment> \
             | verify <pubkey-base64> <file>"
        ),
    }
}

fn password() -> Result<String> {
    std::env::var("MINISIGN_PASSWORD").context("MINISIGN_PASSWORD is not set")
}

fn keygen(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)?;
    let pair = minisign::KeyPair::generate_encrypted_keypair(Some(password()?))?;
    let pk = pair.pk.to_box()?.to_string();
    let sk = pair.sk.to_box(Some("WinPods release signing key"))?.to_string();
    let pub_path = out_dir.join("winpods-update.pub");
    let key_path = out_dir.join("winpods-update.key");
    if key_path.exists() {
        bail!("{} already exists; refusing to overwrite", key_path.display());
    }
    fs::write(&pub_path, &pk)?;
    fs::write(&key_path, &sk)?;
    println!("public key file: {}", pub_path.display());
    println!("secret key file: {}", key_path.display());
    println!("public key: {}", public_key_line(&pk)?);
    Ok(())
}

fn public_key_line(pk_box: &str) -> Result<String> {
    pk_box
        .lines()
        .find(|l| !l.starts_with("untrusted comment:") && !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .context("public key box has no key line")
}

fn sign(key_file: &str, file: &Path, trusted_comment: &str) -> Result<()> {
    let sk_text = if key_file == "-" {
        std::env::var("MINISIGN_SECRET_KEY").context("MINISIGN_SECRET_KEY is not set")?
    } else {
        fs::read_to_string(key_file).with_context(|| format!("reading {key_file}"))?
    };
    let sk = minisign::SecretKeyBox::from_string(&sk_text)?.into_secret_key(Some(password()?))?;
    let data = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let signature = minisign::sign(
        None,
        &sk,
        Cursor::new(data),
        Some(trusted_comment),
        Some("signature from the WinPods release key"),
    )?;
    let out = sig_path(file);
    fs::write(&out, signature.to_string())?;
    println!("wrote {}", out.display());
    Ok(())
}

fn verify(public_key: &str, file: &Path) -> Result<()> {
    let pk = minisign_verify::PublicKey::from_base64(public_key.trim())
        .map_err(|e| anyhow::anyhow!("bad public key: {e}"))?;
    let sig_text = fs::read_to_string(sig_path(file))?;
    let signature = minisign_verify::Signature::decode(&sig_text)
        .map_err(|e| anyhow::anyhow!("bad signature: {e}"))?;
    let data = fs::read(file)?;
    pk.verify(&data, &signature, false)
        .map_err(|e| anyhow::anyhow!("verification failed: {e}"))?;
    println!("OK: {} (trusted comment: {})", file.display(), signature.trusted_comment());
    Ok(())
}

fn sig_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".minisig");
    PathBuf::from(name)
}
