use anyhow::Context;
use reqwest::{header::USER_AGENT, Client};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::{
    hashes::hashes::Hashes,
    peers::peers::Peer,
    tracker::{TrackerRequest, TrackerResponse},
};

/// A Metainfo file (also known as .torrent files).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TorrentFile {
    /// The URL of the tracker.
    pub announce: String,
    pub info: Info,
}
impl TorrentFile {
    pub fn info_hash(&self) -> [u8; 20] {
        let info_encoded =
            serde_bencode::to_bytes(&self.info).expect("re-encode info section should be fine");
        let mut hasher = Sha1::new();
        hasher.update(&info_encoded);
        hasher
            .finalize()
            .try_into()
            .expect("GenericArray<_, 20> == [_; 20]")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Info {
    /// The suggested name to save the file (or directory) as. It is purely advisory.
    ///
    /// In the single file case, the name key is the name of a file, in the muliple file case, it's
    /// the name of a directory.
    pub name: String,
    /// The number of bytes in each piece the file is split into.
    ///
    /// For the purposes of transfer, files are split into fixed-size pieces which are all the same
    /// length except for possibly the last one which may be truncated. piece length is almost
    /// always a power of two, most commonly 2^18 = 256K (BitTorrent prior to version 3.2 uses 2
    /// 20 = 1 M as default).
    #[serde(rename = "piece length")]
    pub plength: usize,
    /// Each entry of `pieces` is the SHA1 hash of the piece at the corresponding index.
    pub pieces: Hashes,
    #[serde(flatten)]
    pub keys: Keys,
}

impl Info {
    pub fn calculate_length(&self) -> usize {
        match &self.keys {
            Keys::SingleFile { length } => length.clone(),
            Keys::MultiFile { files } => {
                let mut sum: usize = 0;
                for file in files.iter() {
                    sum += file.length;
                }
                sum
            }
        }
    }
}
/// There is a key `length` or a key `files`, but not both or neither.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Keys {
    /// If `length` is present then the download represents a single file.
    SingleFile {
        /// The length of the file in bytes.
        length: usize,
    },
    /// Otherwise it represents a set of files which go in a directory structure.
    ///
    /// For the purposes of the other keys in `Info`, the multi-file case is treated as only having
    /// a single file by concatenating the files in the order they appear in the files list.
    MultiFile { files: Vec<File> },
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct File {
    /// The length of the file, in bytes.
    pub length: usize,
    /// Subdirectory names for this file, the last of which is the actual file name
    /// (a zero length list is an error case).
    pub path: Vec<String>,
}

#[derive(Debug)]
pub struct DownloadInfo {
    pub downloaded: usize,
    pub uploaded: usize,
    pub left: usize,
}

pub struct Torrent {
    pub torrent_file: TorrentFile,
    pub peers: Vec<Peer>,
    pub info_hash: [u8; 20],
}

impl Torrent {
    pub fn new(torrent_file: TorrentFile) -> Self {
        Self {
            torrent_file: torrent_file.clone(),
            peers: Vec::new(),
            info_hash: torrent_file.info_hash(),
        }
    }

    pub async fn contact_tracker(&self) -> anyhow::Result<TrackerResponse> {
        let request = TrackerRequest {
            peer_id: String::from("00112233445566718890"),
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: self.torrent_file.info.calculate_length(),
            no_peer_id: 0,
            compact: 1,
        };

        let url_params =
            serde_urlencoded::to_string(&request).context("url-encode tracker parameters")?;
        let tracker_url = format!(
            "{}?{}&info_hash={}",
            self.torrent_file.announce,
            url_params,
            &urlencode(&self.info_hash),
        );

        let client = Client::new();
        let response = client
            .get(tracker_url)
            .header(USER_AGENT, "MyCustomUserAgent/1.0")
            .send()
            .await
            .context("query tracker")?;
        let response = response.bytes().await.context("fetch tracker response")?;
        let tracker_info: TrackerResponse =
            serde_bencode::from_bytes(&response).context("parse tracker response")?;
        Ok(tracker_info)
    }
}

fn urlencode(t: &[u8; 20]) -> String {
    let mut encoded = String::with_capacity(3 * t.len());
    for &byte in t {
        encoded.push('%');
        encoded.push_str(&hex::encode(&[byte]));
    }
    encoded
}
