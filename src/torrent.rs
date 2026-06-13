use anyhow::Context;
use reqwest::{header::USER_AGENT, Client};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::{
    hashes::Hashes,
    tracker::{TrackerRequest, TrackerResponse},
};

pub const PEER_ID: [u8; 20] = *b"00112233445566778899";
pub const BLOCK_MAX: usize = 1 << 14;

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
    /// In the single file case, the name key is the name of a file, in the multiple file case, it's
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
            Keys::SingleFile { length } => *length,
            Keys::MultiFile { files } => files.iter().map(|f| f.length).sum(),
        }
    }

    pub fn num_pieces(&self) -> usize {
        self.pieces.0.len()
    }

    pub fn piece_size(&self, piece_index: usize) -> usize {
        if piece_index == self.num_pieces() - 1 {
            let md = self.calculate_length() % self.plength;
            if md == 0 { self.plength } else { md }
        } else {
            self.plength
        }
    }

    pub fn block_size(&self, piece_index: usize, block_index: usize) -> usize {
        let piece_size = self.piece_size(piece_index);
        let n_blocks = (piece_size + BLOCK_MAX - 1) / BLOCK_MAX;
        if block_index == n_blocks - 1 {
            let md = piece_size % BLOCK_MAX;
            if md == 0 { BLOCK_MAX } else { md }
        } else {
            BLOCK_MAX
        }
    }

    pub fn num_blocks(&self, piece_index: usize) -> usize {
        let piece_size = self.piece_size(piece_index);
        (piece_size + BLOCK_MAX - 1) / BLOCK_MAX
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

#[derive(Clone, Debug)]
pub struct Torrent {
    pub torrent_file: TorrentFile,
    pub info_hash: [u8; 20],
}

impl Torrent {
    pub fn new(torrent_file: TorrentFile) -> Self {
        Self {
            torrent_file: torrent_file.clone(),
            info_hash: torrent_file.info_hash(),
        }
    }

    pub async fn contact_tracker(&self) -> anyhow::Result<TrackerResponse> {
        let request = TrackerRequest {
            peer_id: String::from_utf8_lossy(&PEER_ID).to_string(),
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
        let max_retries = 5;
        let mut retry_delay = std::time::Duration::from_secs(2);
        
        for attempt in 1..=max_retries {
            eprintln!("Contacting tracker (attempt {}/{})", attempt, max_retries);
            
            let response = match client
                .get(&tracker_url)
                .header(USER_AGENT, "BitTorrent-Rust/1.0")
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("Tracker request failed: {}", e);
                    if attempt < max_retries {
                        eprintln!("Retrying in {}s...", retry_delay.as_secs());
                        tokio::time::sleep(retry_delay).await;
                        retry_delay *= 2;
                    }
                    continue;
                }
            };
            
            let bytes = match response.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("Failed to read tracker response: {}", e);
                    if attempt < max_retries {
                        eprintln!("Retrying in {}s...", retry_delay.as_secs());
                        tokio::time::sleep(retry_delay).await;
                        retry_delay *= 2;
                    }
                    continue;
                }
            };
            
            match serde_bencode::from_bytes(&bytes) {
                Ok(tracker_info) => return Ok(tracker_info),
                Err(e) => {
                    eprintln!("Failed to parse tracker response: {}", e);
                    if attempt < max_retries {
                        eprintln!("Retrying in {}s...", retry_delay.as_secs());
                        tokio::time::sleep(retry_delay).await;
                        retry_delay *= 2;
                    }
                }
            }
        }
        
        anyhow::bail!("Failed to contact tracker after {} attempts", max_retries)
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
