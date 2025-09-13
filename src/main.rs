pub mod activepeer;
mod command;
mod decoder;
mod handshake;
mod hashes;
mod peers;
mod torrent;
mod tracker;
mod download_manager;
mod client;

use anyhow::Context;
use clap::Parser;
use command::{Args, Command};
use decoder::decode_bencoded_value;
use sha1::{Digest, Sha1};
use tokio::net::windows::named_pipe::PipeEnd::Client;
use torrent::{Keys, Torrent, TorrentFile};
use tokio::task::JoinSet;
use tokio_mpmc::{channel};
use crate::client::DownloadClient;
use crate::download_manager::FileManager;
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

            let num_workers = 1;
            let mut set = JoinSet::new();
            let (broadcast_sender, broadcast_receiver) = broadcast::channel(16);

            let mut file_manager = FileManager::new(t, "Download".to_string(), data_rx, broadcast_sender.clone());
            file_manager.pre_allocate_files().expect("could not pre allocate files");
            set.spawn(file_manager.process());

            for _ in 0..num_workers {

                let mut client = DownloadClient::new(
                    tracker_info.peers.0.first().unwrap().ip4.clone(),
                    torrent.clone(),
                    piece_tx.clone(),
                    piece_rx.clone(),
                    data_tx.clone(),
                    broadcast_sender.subscribe(),
                );
                let result = client.try_connect().await;
                match result {
                    Ok(_) => {
                        eprintln!("connected to peer");
                        set.spawn(client.start_message_loop());
                    }
                    Err(_) => {
                        eprintln!("Error: Failed to connect to torrent peer");
                    }
                }
            }

            set.join_all().await;

        }
    }
    Ok(())
}

// ideas to improve
// disconnect from peer if chocked for minute or not receiving msg for min
