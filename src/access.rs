use anyhow::{Context, Result, bail};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    pub local: String,
    pub clients: BTreeMap<String, String>,
}
fn generate() -> Result<String> {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Could not generate secure access key"))?;
    Ok(format!(
        "hp_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
pub fn key_path(config: &Path) -> PathBuf {
    config.with_extension("access-keys.json")
}
pub fn ensure(config: &Path, clients: &[String]) -> Result<Keys> {
    let path = key_path(config);
    let parent = path.parent().context("Access keys have no parent")?;
    fs::create_dir_all(parent)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut keys = if path.exists() {
        read(config)?
    } else {
        Keys {
            local: generate()?,
            clients: BTreeMap::new(),
        }
    };
    let mut changed = !path.exists();
    for client in clients {
        if client.is_empty() {
            bail!("Client name cannot be empty");
        }
        if !keys.clients.contains_key(client) {
            keys.clients.insert(client.clone(), generate()?);
            changed = true;
        }
    }
    if changed {
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(&keys)?)?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
    }
    Ok(keys)
}
pub fn read(config: &Path) -> Result<Keys> {
    let keys: Keys = serde_json::from_slice(
        &fs::read(key_path(config)).context("Cannot read host access keys")?,
    )
    .context("Invalid host access key file")?;
    if keys.local.is_empty() || keys.clients.values().any(|key| key.is_empty()) {
        bail!("Host access key file contains empty keys");
    }
    Ok(keys)
}
fn equal(a: &str, b: &str) -> bool {
    let mut diff = a.len() ^ b.len();
    for (i, byte) in a.bytes().enumerate() {
        diff |= usize::from(byte ^ b.as_bytes().get(i).copied().unwrap_or(0));
    }
    diff == 0
}
impl Keys {
    pub fn accepts(&self, value: &str) -> bool {
        equal(&self.local, value) || self.clients.values().any(|key| equal(key, value))
    }
}
