mod command;
mod decoder;
mod handshake;
mod hashes;
mod peers;
mod torrent;
mod tracker;

use anyhow::Context;
use clap::Parser;
use command::{Args, Command};
use decoder::decode_bencoded_value;
use handshake::Handshake;
use reqwest::header::USER_AGENT;
use reqwest::Client;
use sha1::{Digest, Sha1};
use tokio::time::{self, Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use torrent::{Keys, Torrent};
use tracker::{TrackerRequest, TrackerResponse};

fn urlencode(t: &[u8; 20]) -> String {
    let mut encoded = String::with_capacity(3 * t.len());
    for &byte in t {
        encoded.push('%');
        encoded.push_str(&hex::encode(&[byte]));
    }
    encoded
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Decode { value } => {
            let v = decode_bencoded_value(&value).0;
            println!("{v}");
        }
        Command::Info { torrent } => {
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: Torrent =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            println!("Tracker URL: {}", t.announce);
            if let Keys::SingleFile { length } = &t.info.keys {
                println!("Length: {length}");
            }

            if let Keys::MultiFile { files } = &t.info.keys {
                println!("Files: {:?}", files);
            }

            let info_encoded =
                serde_bencode::to_bytes(&t.info).context("re-encode info section")?;
            let mut hasher = Sha1::new();
            hasher.update(&info_encoded);
            let info_hash = hasher.finalize();
            println!("Info Hash: {}", hex::encode(&info_hash));
            println!("Piece Length: {}", t.info.plength);
            println!("Piece Hashes:");
            for hash in t.info.pieces.0 {
                println!("{}", hex::encode(&hash));
            }
        }
        Command::Peers { torrent } => {
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: Torrent =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            let mut length = if let torrent::Keys::SingleFile { length } = &t.info.keys {
                length.clone()
            } else {
                0
            };

            length = if let torrent::Keys::MultiFile { files } = &t.info.keys {
                let mut sum: usize = 0;
                for file in files.iter() {
                    sum += file.length;
                }
                sum
            } else {
                length
            };

            let info_hash = t.info_hash();
            let request = TrackerRequest {
                peer_id: String::from("00112233445566718890"),
                port: 6881,
                uploaded: 0,
                downloaded: 0,
                left: length,
                no_peer_id: 0,
                compact: 1,
            };
            let url_params =
                serde_urlencoded::to_string(&request).context("url-encode tracker parameters")?;
            let tracker_url = format!(
                "{}?{}&info_hash={}",
                t.announce,
                url_params,
                &urlencode(&info_hash)
            );
            //println!("{:?}", tracker_url);
            let client = Client::new();
            let response = client
                .get(tracker_url)
                .header(USER_AGENT, "MyCustomUserAgent/1.0")
                .send()
                .await
                .context("query tracker")?;
            let response = response.bytes().await.context("fetch tracker response")?;
            println!("Tracker response");
            println!("{:?}", response);
            let response: TrackerResponse =
                serde_bencode::from_bytes(&response).context("parse tracker response")?;

            let mut peer: Option<tokio::net::TcpStream> = None;
            let timeout_duration = Duration::from_secs(2);
            for recieved_peer in response.peers.0.iter() {
                println!("connecting to {:?}", recieved_peer.ip4);
                let connection_attempt =
                    time::timeout(timeout_duration, TcpStream::connect(recieved_peer.ip4)).await;
                match connection_attempt {
                    Ok(Ok(stream)) => {
                        println!("connected");
                        peer = Some(stream);
                        break;
                    }
                    Ok(Err(_)) => {
                        println!("Failed to connect to peer {:?}", recieved_peer.ip4);
                    }
                    Err(_) => {
                        println!("Failed to connect to peer {:?}", recieved_peer.ip4);
                    }
                }
            }

            let mut handshake = Handshake::new(info_hash, *b"00112233445566778899");
            {
                let handshake_bytes =
                    &mut handshake as *mut Handshake as *mut [u8; std::mem::size_of::<Handshake>()];
                // Safety: Handshake is a POD with repr(c)
                let handshake_bytes: &mut [u8; std::mem::size_of::<Handshake>()] =
                    unsafe { &mut *handshake_bytes };
                peer.as_mut()
                    .unwrap()
                    .write_all(handshake_bytes)
                    .await
                    .context("write handshake")?;
                peer.unwrap()
                    .read_exact(handshake_bytes)
                    .await
                    .context("read handshake")?;
            }
            assert_eq!(handshake.length, 19);
            assert_eq!(&handshake.bittorrent, b"BitTorrent protocol");
            println!("Peer ID: {}", hex::encode(&handshake.peer_id));
        }
    }
    Ok(())
}
