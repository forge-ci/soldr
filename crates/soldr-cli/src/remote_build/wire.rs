use std::io::{self, Read};

use prost::Message;

pub(super) const MAX_FRAME: usize = 256 * 1024 * 1024;

#[derive(Clone, PartialEq, Message)]
pub(super) struct Start {
    #[prost(string, tag = "1")]
    pub commit: String,
    #[prost(message, repeated, tag = "2")]
    pub files: Vec<Source>,
    #[prost(enumeration = "Action", tag = "3")]
    pub action: i32,
    #[prost(string, repeated, tag = "4")]
    pub args: Vec<String>,
    #[prost(sint32, optional, tag = "5")]
    pub cache_zstd_level: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "6")]
    pub manifest_json: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct Source {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub digest: String,
    #[prost(bool, tag = "3")]
    pub executable: bool,
    #[prost(bool, tag = "4")]
    pub symlink: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub(super) enum Action {
    Build = 0,
    Test = 1,
    Dylint = 2,
    Lint = 3,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct Session {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, repeated, tag = "2")]
    pub missing: Vec<String>,
    #[prost(bool, tag = "3")]
    pub manifest_accepted: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct Blob {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct ResultFrame {
    #[prost(int32, tag = "1")]
    pub exit_code: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub stdout: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub stderr: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub attestation: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct Fetch {
    #[prost(string, repeated, tag = "1")]
    pub globs: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct Artifact {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
    #[prost(bool, tag = "3")]
    pub executable: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(super) struct Artifacts {
    #[prost(message, repeated, tag = "1")]
    pub files: Vec<Artifact>,
}

pub(super) fn encode(message: &impl Message) -> io::Result<Vec<u8>> {
    if message.encoded_len() > MAX_FRAME {
        return Err(io::Error::other("remote build message exceeds 256 MiB"));
    }
    zstd::stream::encode_all(message.encode_to_vec().as_slice(), 3)
}

pub(super) fn decode<M: Message + Default>(compressed: &[u8]) -> io::Result<M> {
    let decoder = zstd::stream::Decoder::new(compressed)?;
    let mut data = Vec::new();
    decoder.take(MAX_FRAME as u64 + 1).read_to_end(&mut data)?;
    if data.len() > MAX_FRAME {
        return Err(io::Error::other("remote build message exceeds 256 MiB"));
    }
    M::decode(data.as_slice()).map_err(io::Error::other)
}
