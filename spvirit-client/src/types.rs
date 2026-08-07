use std::net::SocketAddr;
use std::time::Duration;

use spvirit_codec::spvd_decode::{DecodedValue, StructureDesc};

/// Configuration for PV operations (get, put, monitor, info).
#[derive(Clone, Debug)]
pub struct PvOptions {
    pub pv_name: String,
    pub timeout: Duration,
    pub server_addr: Option<SocketAddr>,
    pub search_addr: Option<std::net::IpAddr>,
    pub bind_addr: Option<std::net::IpAddr>,
    pub name_servers: Vec<SocketAddr>,
    pub udp_port: u16,
    pub tcp_port: u16,
    pub debug: bool,
    pub no_broadcast: bool,
    pub authnz_user: Option<String>,
    pub authnz_host: Option<String>,
}

/// Backwards-compatible alias — prefer [`PvOptions`].
pub type PvGetOptions = PvOptions;

impl PvOptions {
    pub fn new(pv_name: String) -> Self {
        Self {
            pv_name,
            timeout: Duration::from_secs(5),
            server_addr: None,
            search_addr: None,
            bind_addr: None,
            name_servers: Vec::new(),
            udp_port: 5076,
            tcp_port: 5075,
            debug: false,
            no_broadcast: false,
            authnz_user: None,
            authnz_host: None,
        }
    }
}

/// A single monitor update delivered by [`PvaClient::pvmonitor`](crate::pva_client::PvaClient::pvmonitor).
#[derive(Debug, Clone)]
pub struct PvMonitorEvent {
    pub pv_name: String,
    pub value: DecodedValue,
}

#[derive(Debug)]
pub struct PvGetResult {
    pub pv_name: String,
    pub value: DecodedValue,
    pub raw_pva: Vec<u8>,
    pub raw_pvd: Vec<u8>,
    pub introspection: StructureDesc,
}

#[derive(Debug)]
pub enum PvGetError {
    Io(std::io::Error),
    Timeout(&'static str),
    Search(&'static str),
    Protocol(String),
    Decode(String),
    /// A typed failure from `spvirit-codec`.
    Codec(spvirit_codec::DecodeError),
}

impl std::fmt::Display for PvGetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PvGetError::Io(e) => write!(f, "io error: {}", e),
            PvGetError::Timeout(ctx) => write!(f, "timeout: {}", ctx),
            PvGetError::Search(ctx) => write!(f, "search error: {}", ctx),
            PvGetError::Protocol(ctx) => write!(f, "protocol error: {}", ctx),
            PvGetError::Decode(ctx) => write!(f, "decode error: {}", ctx),
            PvGetError::Codec(e) => write!(f, "codec error: {}", e),
        }
    }
}

impl std::error::Error for PvGetError {}

impl From<std::io::Error> for PvGetError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<spvirit_codec::DecodeError> for PvGetError {
    fn from(value: spvirit_codec::DecodeError) -> Self {
        Self::Codec(value)
    }
}
