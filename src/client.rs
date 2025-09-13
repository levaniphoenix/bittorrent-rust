use std::net::SocketAddrV4;
use std::time::Duration;
use tokio_mpmc::{Receiver, Sender};
use anyhow::{Context, Result};
use futures_util::{select, FutureExt, SinkExt, StreamExt};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::{pin, time};
use tokio_util::codec::Framed;
use crate::command::BroadcastCommand;
use crate::handshake::Handshake;
use crate::peers::peers::{Message, MessageFramer, MessageTag, Piece, Request};
use crate::torrent::{Info, Torrent};

const BLOCK_MAX: usize = 1 << 14;
const CLIENT_ID: [u8; 20] = *b"00112233445566778899";

pub struct DownloadClient {
    ip: SocketAddrV4,
    piece_sender : Sender<usize>,
    piece_receiver : Receiver<usize>,
    data_sender: Sender<(usize,Vec<u8>)>,
    broadcast_receiver: tokio::sync::broadcast::Receiver<BroadcastCommand>,
    peer_state: Option<PeerState>,
    tcp_stream : Option<Framed<TcpStream, MessageFramer>>,
    torrent: Torrent,
}

impl DownloadClient {
    pub fn new(ip: SocketAddrV4,
               torrent: Torrent,
               piece_sender : Sender<usize>,
               piece_receiver : Receiver<usize>,
               data_sender: Sender<(usize,Vec<u8>)>,
               broadcast_receiver: tokio::sync::broadcast::Receiver<BroadcastCommand>) -> Self {
        DownloadClient {
            ip,
            torrent,
            piece_sender,
            piece_receiver,
            data_sender,
            broadcast_receiver,
            peer_state: None,
            tcp_stream: None,
        }
    }

    pub async fn try_connect(&mut self) -> Result<()>{
        let timeout_duration = Duration::from_secs(5);
        println!("Thread {:?} | connecting to {:?}", std::thread::current().id(), self.ip);

        let connection_attempt = time::timeout(timeout_duration, TcpStream::connect(&self.ip))
            .await
            .map_err(|_| anyhow::anyhow!("Thread {:?} timed out", std::thread::current().id()))?;

        match connection_attempt {
            Ok(tcp_stream) => {self.tcp_stream = Some(Framed::new(tcp_stream, MessageFramer))},
            Err(e) => {
                eprintln!("Connection failed: {e}");
                return Err(anyhow::anyhow!("Connection failed: {e}"));
            }
        }
        Ok(())
    }

    pub async fn download_piece(&mut self, piece_index: usize) -> Result<()> {
        let torrent_info = &self.torrent.torrent_file.info;
        let piece_hash = torrent_info.pieces.0[piece_index];
        let piece_size = Self::calculate_piece_size(piece_index, torrent_info);
        let n_blocks = (piece_size + (BLOCK_MAX - 1)) / BLOCK_MAX;

        let mut piece_data = vec![0; piece_size]; // preallocate
        let mut received = 0;

        for block_index in 0..n_blocks {
            let block_size = Self::calculate_block_size(block_index, n_blocks, piece_size);

            let mut request = Request::new(
                piece_index as u32,
                (block_index * BLOCK_MAX ) as u32,
                block_size as u32,
            );

            let request_bytes = request.as_bytes_mut();
            self.tcp_stream.as_mut().unwrap().send(Message {
                tag: MessageTag::Request,
                payload: request_bytes.to_vec(),
            }).await.with_context(|| "Failed to send block request to peer")?;
        }

        while received < n_blocks {
            let msg = self.tcp_stream.as_mut().unwrap().next().await.expect("peer always sends a piece").context("peer message was invalid")?;
            match msg.tag {
                MessageTag::Choke => {}
                MessageTag::Unchoke => {}
                MessageTag::Interested => {}
                MessageTag::NotInterested => {}
                MessageTag::Have => {}
                MessageTag::Bitfield => {
                    println!("Received bitfield");
                }
                MessageTag::Request => {}
                MessageTag::Piece => {
                    println!("Received a block");
                    let piece = Piece::ref_from_bytes(&msg.payload[..]).expect("always get all Piece response fields from peer");
                    piece_data[piece.begin() as usize .. piece.begin() as usize + piece.block().len()]
                        .copy_from_slice(piece.block());
                    received += 1;
                }
                MessageTag::Cancel => {}
            }
        }

        let mut hasher = Sha1::new();
        hasher.update(&piece_data);
        let hash: [u8; 20] = hasher
            .finalize()
            .try_into()
            .expect("GenericArray<_, 20> == [_; 20]");
        if hash != piece_hash {
            println!("Piece {piece_index} failed hash check");
            return Err(anyhow::anyhow!("Hash mismatch for piece {}", piece_index));
        }

        self.data_sender.send((piece_index,piece_data)).await.with_context(|| "Failed to send piece data to writer")?;

        Ok(())
    }

    pub async fn start_message_loop(mut self) -> Result<()> {
        let torrent = self.torrent.clone();
        //call after successfully connecting to a peer
        let _ = self.exchange_handshakes(&torrent).await;

        //later exchange bitfields

        //step 3. send interested message
        self.send_message(MessageTag::Interested, Vec::new())
            .await
            .expect("should send interested message");

        // while let Ok(Some(piece_index)) = self.piece_receiver.recv().await {
        //     let result = self.download_piece(piece_index).await;
        //     match result {
        //         Ok(_) => {}
        //         Err(e) => {
        //             eprintln!("Error downloading {piece_index} piece from peer: {e}");
        //             self.piece_sender.send(piece_index).await.with_context(|| "Failed to send piece back to queue")?;
        //         }
        //     }
        // }

        // let mut shutdown_fut = self.broadcast_receiver.recv();
        // pin!(shutdown_fut);

        loop {
                tokio::select! {
                    command = self.broadcast_receiver.recv() => {
                        println!("worker Shutting down");
                        break;
                    },
                    piece_index_result = self.piece_receiver.recv() => {
                        match piece_index_result {
                            Ok(piece_index) => {
                                if let Some(index) = piece_index{
                                    self.download_piece(index).await?;
                                }
                            },
                            Err(e) => {
                                eprintln!("Error receiving piece {e}");
                            }
                        }
                    }

                }
            }
        eprintln!("end message loop");
        Ok(())
    }

    pub async fn send_message(
        &mut self,
        message_tag: MessageTag,
        payload: Vec<u8>,
    ) -> Result<()> {
        self.tcp_stream.as_mut().unwrap()
            .send(Message {
                tag: message_tag,
                payload,
            })
            .await
            .context(format!("sending {message_tag:?} message"))
    }

    pub async fn exchange_handshakes(&mut self, torrent: &Torrent) -> Result<Handshake> {
        let mut handshake = Handshake::new(torrent.info_hash, CLIENT_ID);
        {
            let handshake_bytes = handshake.as_bytes_mut();
            self.tcp_stream.as_mut().unwrap()
                .get_mut()
                .write_all(handshake_bytes)
                .await
                .context("write handshake")?;
            self.tcp_stream.as_mut().unwrap()
                .get_mut()
                .read_exact(handshake_bytes)
                .await
                .context("read handshake")?;
        }

        Ok(handshake)
    }

    pub async fn exchange_bitfields(&mut self) -> Result<Message> {
        let bitfield = self
            .tcp_stream.as_mut().unwrap()
            .next()
            .await
            .expect("peer always sends a bitfields")?;
        Ok(bitfield)
    }

    pub fn calculate_piece_size(piece_index: usize, torrent_info: &Info) -> usize{
        if piece_index == torrent_info.pieces.0.len() - 1 {
            let md = torrent_info.calculate_length() % torrent_info.plength;
            if md == 0 {
                torrent_info.plength
            } else {
                md
            }
        } else {
            torrent_info.plength
        }
    }

    pub fn calculate_block_size(block_index: usize, number_of_block: usize, piece_size: usize) -> usize {
        if block_index == number_of_block - 1 {
            let md = piece_size % BLOCK_MAX;
            if md == 0 {
                BLOCK_MAX
            } else {
                md
            }
        } else {
            BLOCK_MAX
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerState {
    pub am_choking: bool,
    pub am_interested: bool,
    pub peer_choking: bool,
    pub peer_interested: bool,
}

impl PeerState {
    pub fn new() -> Self {
        Self {
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
        }
    }
}