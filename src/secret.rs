//! Subscription passwords in the config file.
//!
//! A random 32-byte key is stored in `~/.config/deppo/key` (mode 0600).
//! The salt is the server name, the subscription id, and a fixed filler.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config;

const FILLER: &[u8] = b"deppo-linux-filler-v1";
const MAGIC: &[u8] = b"DPL1";

pub fn encrypt(server: &str, user: &str, password: &str) -> Result<Vec<u8>> {
    let key = derive(server, user)?;
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).map_err(|err| anyhow::anyhow!("random nonce: {err}"))?;
    let (cipher, tag) = aes_gcm_encrypt(&key, &nonce, password.as_bytes())?;
    let mut out = Vec::with_capacity(4 + nonce.len() + cipher.len() + tag.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&cipher);
    out.extend_from_slice(&tag);
    Ok(out)
}

pub fn decrypt(server: &str, user: &str, blob: &[u8]) -> Result<String> {
    if blob.len() < 4 + 12 + 16 || &blob[..4] != MAGIC {
        bail!("the stored password is not a Deppo password");
    }
    let nonce = &blob[4..16];
    let tag_at = blob.len() - 16;
    let cipher = &blob[16..tag_at];
    let tag: [u8; 16] = blob[tag_at..].try_into().unwrap();
    let key = derive(server, user)?;
    let plain = aes_gcm_decrypt(&key, nonce, cipher, &tag)?;
    String::from_utf8(plain).context("the stored password is not text")
}

fn derive(server: &str, user: &str) -> Result<[u8; 32]> {
    let master = load_or_create_key()?;
    let mut hasher = Sha256::new();
    hasher.update(master);
    hasher.update(server.as_bytes());
    hasher.update([0]);
    hasher.update(user.as_bytes());
    hasher.update([0]);
    hasher.update(FILLER);
    Ok(hasher.finalize().into())
}

fn key_path() -> PathBuf {
    config::config_dir().join("key")
}

fn load_or_create_key() -> Result<[u8; 32]> {
    let path = key_path();
    if path.is_file() {
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if bytes.len() != 32 {
            bail!("password key file is the wrong size");
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    fs::create_dir_all(config::config_dir()).context("create config directory")?;
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|err| anyhow::anyhow!("random password key: {err}"))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(&key)?;
    file.sync_all().ok();
    Ok(key)
}

fn aes_gcm_encrypt(key: &[u8; 32], nonce: &[u8], plain: &[u8]) -> Result<(Vec<u8>, [u8; 16])> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).context("AES key")?;
    let nonce = Nonce::from_slice(nonce);
    let mut out = cipher.encrypt(nonce, plain).map_err(|_| anyhow::anyhow!("encrypt password"))?;
    if out.len() < 16 {
        bail!("encrypt password");
    }
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&out[out.len() - 16..]);
    out.truncate(out.len() - 16);
    Ok((out, tag))
}

fn aes_gcm_decrypt(key: &[u8; 32], nonce: &[u8], cipher_text: &[u8], tag: &[u8; 16]) -> Result<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).context("AES key")?;
    let nonce = Nonce::from_slice(nonce);
    let mut data = Vec::with_capacity(cipher_text.len() + 16);
    data.extend_from_slice(cipher_text);
    data.extend_from_slice(tag);
    cipher.decrypt(nonce, data.as_ref()).map_err(|_| anyhow::anyhow!("decrypt password"))
}
