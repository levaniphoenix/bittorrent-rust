pub mod activepeer;
mod command;
mod decoder;
mod handshake;
mod hashes;
mod peers;
mod torrent;
mod tracker;

use std::io::Write;
use std::sync::Arc;

use activepeer::activepeer::ActivePeer;
use anyhow::Context;
use clap::Parser;
use command::{Args, Command};
use decoder::decode_bencoded_value;
use peers::peers::{connect_to_peer, WorkQueue};
use sha1::{Digest, Sha1};
use torrent::{Keys, Torrent, TorrentFile};

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
            let t: TorrentFile =
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
            let t: TorrentFile =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            let torrent = Torrent::new(t);

            let tracker_info = torrent
                .contact_tracker()
                .await
                .context("getting info from tracker")?;

            println!("{:?}", tracker_info.peers.0);
        }
        Command::Download { torrent } => {
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: TorrentFile =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            let torrent = Arc::new(Torrent::new(t));

            let tracker_info = torrent
                .contact_tracker()
                .await
                .context("getting info from tracker")?;

            let work_queue =
                WorkQueue::new((0..torrent.torrent_file.info.pieces.0.len()).collect());
            let work_queue = Arc::new(work_queue);
            let buffer = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));

            let mut workers = vec![];

            let num_workers = 1;
            let peers = Arc::new(tracker_info.peers.clone());

            for _ in 0..num_workers {
                let peer_info_ref = peers.clone();
                let file_ref = torrent.clone();
                let work_queue_ref = work_queue.clone();
                let buffer_ref = buffer.clone();
                workers.push(tokio::spawn(async move {
                    let peers = peer_info_ref;

                    //try connecting to a peer

                    let mut peer: Option<ActivePeer> = None;
                    for recieved_peer in peers.0.iter() {
                        let result = connect_to_peer(recieved_peer).await;
                        match result {
                            Some(connection) => {
                                peer = Some(connection);
                                break;
                            }
                            None => {}
                        }
                    }

                    let mut peer = peer.expect("connect to a peer");
                    peer.start_exchanging_messages(&file_ref, &work_queue_ref, buffer_ref)
                        .await;
                }));
            }

            for worker in workers {
                worker.await?;
            }

            let file_name = &torrent.torrent_file.info.name;
            let mut f = std::fs::File::create(file_name)?;
            let buffer_guard = buffer.lock().await;
            f.write_all(&buffer_guard)?;
        }
    }
    Ok(())
}
