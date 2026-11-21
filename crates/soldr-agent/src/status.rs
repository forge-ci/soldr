//! Protobuf status snapshot for `soldr agent status`, atomically replaced.
use crate::Result;
use prost::Message;
use serde::Serialize;
use std::{
    io::{Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Clone, PartialEq, Message, Serialize)]
pub struct Status {
    #[prost(string, tag = "1")]
    pub state: String,
    #[prost(uint64, tag = "2")]
    pub updated_ms: u64,
    #[prost(string, tag = "3")]
    pub signal_url: String,
    #[prost(string, tag = "4")]
    pub detail: String,
}
impl Status {
    pub fn write(dir: &Path, state: &str, signal_url: &str, detail: &str) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let status = Self {
            state: state.into(),
            updated_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            signal_url: signal_url.into(),
            detail: detail.into(),
        };
        let mut staged = tempfile::NamedTempFile::new_in(dir)?;
        staged.write_all(&status.encode_to_vec())?;
        staged.persist(dir.join("status.pb"))?;
        Ok(())
    }
    pub fn read(dir: &Path) -> Result<Self> {
        let mut data = Vec::new();
        std::fs::File::open(dir.join("status.pb"))?
            .take(16 * 1024 + 1)
            .read_to_end(&mut data)?;
        if data.len() > 16 * 1024 {
            return Err("agent status too large".into());
        }
        Ok(Self::decode(data.as_slice())?)
    }
}
