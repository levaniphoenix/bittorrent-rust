use serde::{Deserialize, Serialize};

use crate::peers::peers::Peers;
/// Note: the info hash field is _not_ included.
#[derive(Debug, Clone, Serialize)]
pub struct TrackerRequest {
    /// A unique identifier for your client.
    ///
    /// A string of length 20 that you get to pick.
    pub peer_id: String,
    /// The port your client is listening on.
    pub port: u16,
    /// The total amount uploaded so far.
    pub uploaded: usize,
    /// The total amount downloaded so far
    pub downloaded: usize,
    /// The number of bytes left to download.
    pub left: usize,

    pub no_peer_id: u8,
    /// Whether the peer list should use the compact representation
    ///
    /// The compact representation is more commonly used in the wild, the non-compact
    /// representation is mostly supported for backward-compatibility.
    pub compact: u8,
}
#[derive(Debug, Clone, Deserialize)]
pub struct TrackerResponse {
    /// An integer, indicating how often your client should make a request to the tracker in seconds.
    ///
    /// You can ignore this value for the purposes of this challenge.
    pub interval: usize,

    /// A string, which contains list of peers that your client can connect to.
    ///
    /// Each peer is represented using 6 bytes. The first 4 bytes are the peer's IP address and the
    /// last 2 bytes are the peer's port number.
    pub peers: Peers,
}
