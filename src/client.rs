use std::net::SocketAddrV4;
use std::sync::Arc;
use std::time::Duration;
use tokio_mpmc::{Receiver, Sender};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time;
use tokio_util::codec::Framed;
use crate::command::BroadcastCommand;
use crate::handshake::Handshake;
use crate::peers::{Message, MessageFramer, MessageTag, Piece, Request};
use crate::progress::DownloadProgress;
use crate::torrent::{Torrent, PEER_ID};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

pub struct DownloadClient {
    ip: SocketAddrV4,
    piece_sender: Sender<usize>,
    piece_receiver: Receiver<usize>,
    data_sender: Sender<(usize, Vec<u8>)>,
    broadcast_receiver: tokio::sync::broadcast::Receiver<BroadcastCommand>,
    tcp_stream: Option<Framed<TcpStream, MessageFramer>>,
    torrent: Torrent,
    progress: Arc<DownloadProgress>,
}

impl DownloadClient {
    pub fn new(
        ip: SocketAddrV4,
        torrent: Torrent,
        piece_sender: Sender<usize>,
        piece_receiver: Receiver<usize>,
        data_sender: Sender<(usize, Vec<u8>)>,
        broadcast_receiver: tokio::sync::broadcast::Receiver<BroadcastCommand>,
        progress: Arc<DownloadProgress>,
    ) -> Self {
        DownloadClient {
            ip,
            torrent,
            piece_sender,
            piece_receiver,
            data_sender,
            broadcast_receiver,
            tcp_stream: None,
            progress,
        }
    }

    pub async fn try_connect(&mut self) -> Result<()> {
        let timeout_duration = Duration::from_secs(5);
        eprintln!("\nConnecting to {}", self.ip);

        let tcp_stream = time::timeout(timeout_duration, TcpStream::connect(&self.ip))
            .await
            .context("connection timed out")?
            .context("connection failed")?;

        self.tcp_stream = Some(Framed::new(tcp_stream, MessageFramer));
        Ok(())
    }

    pub async fn download_piece(&mut self, piece_index: usize) -> Result<()> {
        let info = &self.torrent.torrent_file.info;
        let piece_hash = info.pieces.0[piece_index];
        let piece_size = info.piece_size(piece_index);
        let n_blocks = info.num_blocks(piece_index);

        let mut piece_data = vec![0u8; piece_size];
        let mut received = 0;

        for block_index in 0..n_blocks {
            let block_size = info.block_size(piece_index, block_index);
            let mut request = Request::new(
                piece_index as u32,
                (block_index * crate::torrent::BLOCK_MAX) as u32,
                block_size as u32,
            );
            let request_bytes = request.as_bytes_mut().to_vec();

            self.tcp_stream
                .as_mut()
                .context("not connected")?
                .send(Message {
                    tag: MessageTag::Request,
                    payload: request_bytes,
                })
                .await
                .context("failed to send block request")?;
        }

        while received < n_blocks {
            let stream = self.tcp_stream.as_mut().context("not connected")?;
            let next_msg = stream.next();
            let msg = time::timeout(DOWNLOAD_TIMEOUT, next_msg)
                .await
                .context("download timed out")?
                .context("peer stream ended")?
                .context("peer message was invalid")?;

            match msg.tag {
                MessageTag::Choke => {
                    return Err(anyhow::anyhow!("peer choked during download"));
                }
                MessageTag::Unchoke | MessageTag::Have | MessageTag::Bitfield => {}
                MessageTag::Piece => {
                    let piece = Piece::ref_from_bytes(&msg.payload)
                        .context("invalid piece response")?;

                    if piece.index() != piece_index as u32 {
                        return Err(anyhow::anyhow!(
                            "expected piece {}, got {}",
                            piece_index,
                            piece.index()
                        ));
                    }

                    let begin = piece.begin() as usize;
                    let end = begin + piece.block().len();
                    if end > piece_size {
                        return Err(anyhow::anyhow!("piece block out of bounds"));
                    }

                    piece_data[begin..end].copy_from_slice(piece.block());
                    received += 1;
                }
                _ => {}
            }
        }

        let mut hasher = Sha1::new();
        hasher.update(&piece_data);
        let hash: [u8; 20] = hasher
            .finalize()
            .try_into()
            .expect("GenericArray<_, 20> == [_; 20]");

        if hash != piece_hash {
            return Err(anyhow::anyhow!("hash mismatch for piece {}", piece_index));
        }

        self.progress.add_bytes(piece_data.len() as u64);
        self.progress.complete_piece(piece_index);

        self.data_sender
            .send((piece_index, piece_data))
            .await
            .context("failed to send piece data to writer")?;

        Ok(())
    }

    pub async fn start_message_loop(mut self) -> Result<()> {
        let torrent = self.torrent.clone();
        self.exchange_handshakes(&torrent).await?;
        self.exchange_bitfields().await?;
        self.send_message(MessageTag::Interested, Vec::new()).await?;

        loop {
            tokio::select! {
                command = self.broadcast_receiver.recv() => {
                    match command {
                        Ok(BroadcastCommand::Shutdown) => {
                            eprintln!("\nWorker {} shutting down", self.ip);
                            break;
                        }
                        Err(_) => break,
                    }
                },
                piece_index_result = self.piece_receiver.recv() => {
                    match piece_index_result {
                        Ok(Some(index)) => {
                            if self.progress.is_piece_completed(index) {
                                continue;
                            }
                            if let Err(e) = self.download_piece(index).await {
                                eprintln!("\nError downloading piece {}: {}", index, e);
                                self.piece_sender.send(index).await
                                    .context("failed to return piece to queue")?;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                _ = time::sleep(KEEPALIVE_INTERVAL) => {
                    if let Some(stream) = self.tcp_stream.as_mut() {
                        let _ = stream.send(Message {
                            tag: MessageTag::Keepalive,
                            payload: Vec::new(),
                        }).await;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn send_message(&mut self, message_tag: MessageTag, payload: Vec<u8>) -> Result<()> {
        self.tcp_stream
            .as_mut()
            .context("not connected")?
            .send(Message {
                tag: message_tag,
                payload,
            })
            .await
            .context(format!("sending {message_tag:?} message"))
    }

    pub async fn exchange_handshakes(&mut self, torrent: &Torrent) -> Result<Handshake> {
        let mut handshake = Handshake::new(torrent.info_hash, PEER_ID);
        let stream = self.tcp_stream.as_mut().context("not connected")?;
        stream
            .get_mut()
            .write_all(handshake.as_bytes_mut())
            .await
            .context("write handshake")?;

        let mut response_buf = [0u8; 68];
        stream
            .get_mut()
            .read_exact(&mut response_buf)
            .await
            .context("read handshake")?;

        if response_buf[0] != 19 || &response_buf[1..20] != b"BitTorrent protocol" {
            return Err(anyhow::anyhow!("invalid handshake response"));
        }

        handshake.length = response_buf[0];
        handshake.bittorrent.copy_from_slice(&response_buf[1..20]);
        handshake.reserved.copy_from_slice(&response_buf[20..28]);
        handshake.info_hash.copy_from_slice(&response_buf[28..48]);
        handshake.peer_id.copy_from_slice(&response_buf[48..68]);

        Ok(handshake)
    }

    pub async fn exchange_bitfields(&mut self) -> Result<Message> {
        let stream = self.tcp_stream.as_mut().context("not connected")?;
        let msg = stream.next().await.context("peer stream ended")??;
        Ok(msg)
    }
}
