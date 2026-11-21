//! Explicit public-key allowlists; no implicit trust from a dial ticket.
use crate::Result;
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    #[serde(default)]
    pub peers: Vec<String>,
}
impl Trust {
    pub fn parse(text: &str) -> Result<Self> {
        let trust: Self = toml::from_str(text)?;
        for id in &trust.peers {
            let _: EndpointId = id.parse()?;
        }
        Ok(trust)
    }
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn contains(&self, id: EndpointId) -> bool {
        self.peers
            .iter()
            .any(|peer| peer.parse::<EndpointId>().ok() == Some(id))
    }
    pub fn add(path: &Path, id: EndpointId) -> Result<()> {
        fs::create_dir_all(path.parent().ok_or("trust parent missing")?)?;
        let mut trust = Self::load(path)?;
        if !trust.contains(id) {
            trust.peers.push(id.to_string());
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(path.parent().ok_or("trust parent missing")?)?;
        use std::io::Write;
        temp.write_all(toml::to_string_pretty(&trust)?.as_bytes())?;
        temp.persist(path)?;
        Ok(())
    }
}
pub fn node_key(root: &Path) -> Result<SecretKey> {
    use std::io::Write;
    fs::create_dir_all(root)?;
    let path = root.join("node.key");
    if !path.exists() {
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        soldr_platform::fs::permissions::restore_mode(temp.path(), Some(0o600))?;
        temp.write_all(&SecretKey::generate().to_bytes())?;
        temp.as_file().sync_all()?;
        match temp.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    soldr_platform::fs::permissions::restore_mode(&path, Some(0o600))?;
    let bytes: [u8; 32] = fs::read(path)?
        .try_into()
        .map_err(|_| "invalid Dist node.key length")?;
    Ok(SecretKey::from_bytes(&bytes))
}
