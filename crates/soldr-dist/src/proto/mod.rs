//! Handwritten prost types. Source of truth: dist.proto.
use prost::Message;
use std::collections::HashMap;
#[derive(Clone, PartialEq, Message)]
pub struct InputFile {
    #[prost(string, tag = "1")]
    pub abs_path: String,
    #[prost(string, tag = "2")]
    pub blake3: String,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    #[prost(bool, tag = "4")]
    pub executable: bool,
}
#[derive(Clone, PartialEq, Message)]
pub struct DistJob {
    #[prost(string, tag = "1")]
    pub compiler_kind: String,
    #[prost(string, tag = "2")]
    pub compiler_identity: String,
    #[prost(string, repeated, tag = "3")]
    pub argv: Vec<String>,
    #[prost(map = "string, string", tag = "4")]
    pub env: HashMap<String, String>,
    #[prost(string, tag = "5")]
    pub cwd: String,
    #[prost(bytes = "vec", tag = "6")]
    pub stdin: Vec<u8>,
    #[prost(message, repeated, tag = "7")]
    pub inputs: Vec<InputFile>,
    #[prost(string, repeated, tag = "8")]
    pub outputs: Vec<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(string, repeated, tag = "2")]
    pub toolchains: Vec<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Offer {
    #[prost(message, optional, tag = "1")]
    pub job: Option<DistJob>,
    #[prost(string, repeated, tag = "2")]
    pub speculative: Vec<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Need {
    #[prost(string, repeated, tag = "1")]
    pub digests: Vec<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Blob {
    #[prost(string, tag = "1")]
    pub digest: String,
    #[prost(uint64, tag = "2")]
    pub size: u64,
}
#[derive(Clone, PartialEq, Message)]
pub struct Accepted {}
#[derive(Clone, PartialEq, Message)]
pub struct Declined {
    #[prost(string, tag = "1")]
    pub reason: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct OutputFile {
    #[prost(string, tag = "1")]
    pub abs_path: String,
    #[prost(string, tag = "2")]
    pub blake3: String,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    #[prost(uint32, tag = "4")]
    pub mode: u32,
}
#[derive(Clone, PartialEq, Message)]
pub struct Done {
    #[prost(int32, tag = "1")]
    pub exit_code: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub stdout: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub stderr: Vec<u8>,
    #[prost(message, repeated, tag = "4")]
    pub outputs: Vec<OutputFile>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Fetch {
    #[prost(string, repeated, tag = "1")]
    pub digests: Vec<String>,
}
#[derive(Clone, PartialEq, Message)]
pub struct Status {
    #[prost(string, tag = "1")]
    pub node_id: String,
    #[prost(string, tag = "2")]
    pub role: String,
    #[prost(string, repeated, tag = "3")]
    pub trusted_clients: Vec<String>,
    #[prost(string, repeated, tag = "4")]
    pub trusted_workers: Vec<String>,
    #[prost(uint64, tag = "5")]
    pub cas_bytes: u64,
    #[prost(uint64, tag = "6")]
    pub running_joblets: u64,
    #[prost(uint64, tag = "7")]
    pub pid: u64,
}
#[derive(Clone, PartialEq, Message)]
pub struct Reply {
    #[prost(message, optional, tag = "1")]
    pub accepted: Option<Accepted>,
    #[prost(message, optional, tag = "2")]
    pub declined: Option<Declined>,
    #[prost(message, optional, tag = "3")]
    pub done: Option<Done>,
    #[prost(message, optional, tag = "4")]
    pub need: Option<Need>,
}
