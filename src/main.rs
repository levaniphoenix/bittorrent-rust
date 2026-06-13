pub mod command;
mod decoder;
mod handshake;
pub mod hashes;
pub mod peers;
mod torrent;
mod tracker;
mod download_manager;
mod client;
mod progress;

use std::cmp::min;
use std::sync::Arc;
use anyhow::Context;
use clap::Parser;
use rand::prelude::SliceRandom;
use command::{Args, Command};
use decoder::decode_bencoded_value;
use sha1::{Digest, Sha1};
use torrent::{Keys, Torrent, TorrentFile};
use tokio::task::JoinSet;
use tokio_mpmc::channel;
use crate::client::DownloadClient;
use crate::download_manager::FileManager;
use crate::progress::DownloadProgress;
use tokio::sync::broadcast;

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
        }
        Command::Peers { torrent } => {
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: TorrentFile =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            let torrent = Torrent::new(t.clone());

            let tracker_info = torrent
                .contact_tracker()
                .await
                .context("getting info from tracker")?;

            for peer in tracker_info.peers.0.iter() {
                println!("{:?}", peer);
            }
        }
        Command::Download { torrent } => {
            let dot_torrent = std::fs::read(torrent).context("read torrent file")?;
            let t: TorrentFile =
                serde_bencode::from_bytes(&dot_torrent).context("parse torrent file")?;
            let torrent = Torrent::new(t.clone());

            let tracker_info = torrent
                .contact_tracker()
                .await
                .context("getting info from tracker")?;

            let num_pieces = torrent.torrent_file.info.pieces.0.len();
            let (piece_tx, piece_rx) = channel(num_pieces);
            let (data_tx, data_rx) = channel(num_pieces);

            for i in 0..num_pieces {
                let result = piece_tx.send(i).await;
                if result.is_err() {
                    eprintln!("Error: Failed to send piece index to the channel.");
                    break;
                }
            }

            let mut set = JoinSet::new();
            let (broadcast_sender, _broadcast_receiver) = broadcast::channel(16);

            let progress = Arc::new(DownloadProgress::new(num_pieces));
            let progress_display = progress.clone();
            set.spawn(async move {
                loop {
                    progress_display.display();
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
            });

            let file_manager = FileManager::new(t, "Download".to_string(), data_rx, broadcast_sender.clone());
            file_manager.pre_allocate_files().expect("could not pre allocate files");
            set.spawn(file_manager.process());

            let mut peers = tracker_info.peers.0.clone();
            let mut rng = rand::rng();
            peers.shuffle(&mut rng);

            let num_workers = min(40, peers.len());

            let endgame_progress = progress.clone();
            let endgame_tx = piece_tx.clone();
            set.spawn(async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    if endgame_progress.is_endgame() {
                        let remaining = endgame_progress.pieces_remaining();
                        if remaining == 0 {
                            break;
                        }
                        eprintln!("\n[Endgame mode: {} pieces remaining, duplicating work]", remaining);
                        for i in 0..num_pieces {
                            if !endgame_progress.is_piece_completed(i) {
                                let _ = endgame_tx.send(i).await;
                            }
                        }
                    }
                }
                Ok(())
            });

            let reannounce_torrent = torrent.clone();
            let _reannounce_tx = piece_tx.clone();
            let reannounce_progress = progress.clone();
            set.spawn(async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                    if reannounce_progress.pieces_remaining() == 0 {
                        break;
                    }
                    eprintln!("\n[Re-announcing to tracker for more peers]");
                    if let Ok(new_tracker_info) = reannounce_torrent.contact_tracker().await {
                        let mut new_peers = new_tracker_info.peers.0;
                        new_peers.shuffle(&mut rand::rng());
                        eprintln!("\n[Got {} new peers from tracker]", new_peers.len());
                    }
                }
                Ok(())
            });

            for peer in peers.into_iter().take(num_workers) {
                let progress_clone = progress.clone();
                let mut client = DownloadClient::new(
                    peer.ip4.clone(),
                    torrent.clone(),
                    piece_tx.clone(),
                    piece_rx.clone(),
                    data_tx.clone(),
                    broadcast_sender.subscribe(),
                    progress.clone(),
                );

                set.spawn(async move {
                    match client.try_connect().await {
                        Ok(_) => {
                            progress_clone.connect_peer();
                            eprintln!("\nConnected to peer: {}", peer.ip4);
                            let _ = client.start_message_loop().await;
                            progress_clone.disconnect_peer();
                        }
                        Err(_) => {
                            eprintln!("\nError: Failed to connect to torrent peer: {}", peer.ip4);
                        }
                    }
                    Ok(())
                });
            }

            set.join_all().await;

        }
    }
    Ok(())
}
