mod command;
mod decoder;
mod handshake;
mod hashes;
mod peers;
mod torrent;
mod tracker;

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use command::{Args, Command};
use decoder::decode_bencoded_value;
use futures_util::{SinkExt, StreamExt};
use handshake::Handshake;
use peers::peers::{Message, MessageFramer, MessageTag, Piece, Request};
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

const BLOCK_MAX: usize = 1 << 14;

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
            let tracker_info: TrackerResponse =
                serde_bencode::from_bytes(&response).context("parse tracker response")?;

            let mut peer: Option<tokio::net::TcpStream> = None;
            let timeout_duration = Duration::from_secs(2);
            for recieved_peer in tracker_info.peers.0.iter() {
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

            let mut peer = peer.context("could not connect to a peer")?;
            let mut handshake = Handshake::new(info_hash, *b"00112233445566778899");
            {
                let handshake_bytes = handshake.as_bytes_mut();
                peer.write_all(handshake_bytes)
                    .await
                    .context("write handshake")?;
                peer.read_exact(handshake_bytes)
                    .await
                    .context("read handshake")?;
            }

            assert_eq!(handshake.length, 19);
            assert_eq!(&handshake.bittorrent, b"BitTorrent protocol");
            println!("Peer ID: {}", hex::encode(&handshake.peer_id));

            let mut peer = tokio_util::codec::Framed::new(peer, MessageFramer);

            let bitfield = peer
                .next()
                .await
                .expect("peer always sends a bitfields")
                .context("peer message was invalid")?;
            assert_eq!(bitfield.tag, MessageTag::Bitfield);
            println!("got bitfield: {:?}", bitfield.payload);

            peer.send(Message {
                tag: MessageTag::Interested,
                payload: Vec::new(),
            })
            .await
            .context("send interested message")?;
            let unchoke = peer
                .next()
                .await
                .expect("peer always sends an unchoke")
                .context("peer message was invalid")?;
            println!("received {:?}", unchoke.tag);
            assert_eq!(unchoke.tag, MessageTag::Unchoke);
            assert!(unchoke.payload.is_empty());
            let piece_i = 0;
            let piece_hash = &t.info.pieces.0[piece_i];
            let piece_size = if piece_i == t.info.pieces.0.len() - 1 {
                let md = length % t.info.plength;
                if md == 0 {
                    t.info.plength
                } else {
                    md
                }
            } else {
                t.info.plength
            };

            let nblocks = (piece_size + (BLOCK_MAX - 1)) / BLOCK_MAX;
            let mut all_blocks = Vec::with_capacity(piece_size);
            for block in 0..nblocks {
                let block_size = if block == nblocks - 1 {
                    let md = piece_size % BLOCK_MAX;
                    if md == 0 {
                        BLOCK_MAX
                    } else {
                        md
                    }
                } else {
                    BLOCK_MAX
                };

                let mut request = Request::new(
                    piece_i as u32,
                    (block * BLOCK_MAX) as u32,
                    block_size as u32,
                );

                let request_bytes = Vec::from(request.as_bytes_mut());
                peer.send(Message {
                    tag: MessageTag::Request,
                    payload: request_bytes,
                })
                .await
                .with_context(|| format!("send request for block {block}"))?;

                let piece = peer
                    .next()
                    .await
                    .expect("peer always sends a piece")
                    .context("peer message was invalid")?;
                assert_eq!(piece.tag, MessageTag::Piece);
                assert!(!piece.payload.is_empty());

                let piece = Piece::ref_from_bytes(&piece.payload[..])
                    .expect("always get all Piece response fields from peer");
                assert_eq!(piece.index() as usize, piece_i);
                assert_eq!(piece.begin() as usize, block * BLOCK_MAX);
                assert_eq!(piece.block().len(), block_size);
                all_blocks.extend(piece.block());
            }
            assert_eq!(all_blocks.len(), piece_size);
            let mut hasher = Sha1::new();
            hasher.update(&all_blocks);
            let hash: [u8; 20] = hasher
                .finalize()
                .try_into()
                .expect("GenericArray<_, 20> == [_; 20]");
            assert_eq!(&hash, piece_hash);
            let output = PathBuf::from("test.pdf");
            tokio::fs::write(&output, all_blocks)
                .await
                .context("write out downloaded piece")?;
            println!("Piece {piece_i} downloaded to {}.", output.display());
        }
    }
    Ok(())
}
