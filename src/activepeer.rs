pub mod activepeer {

    use anyhow::{Context, Error, Result};
    use futures_util::{lock::Mutex, SinkExt, StreamExt};
    use sha1::{Digest, Sha1};
    use std::{collections::VecDeque, io::SeekFrom, sync::Arc};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };
    use tokio_util::codec::Framed;

    use crate::{
        handshake::Handshake,
        peers::peers::{Message, MessageFramer, MessageTag, Piece, Request, WorkQueue},
        torrent::{Info, Torrent},
    };

    const BLOCK_MAX: usize = 1 << 14;
    const PEER_ID: [u8; 20] = *b"00112233445566778899";

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

    pub struct ActivePeer {
        pub connection: Framed<TcpStream, MessageFramer>,
        pub peer_state: PeerState,
        pub bitfield: Vec<u8>,
    }

    impl ActivePeer {
        pub fn new(connection: Framed<TcpStream, MessageFramer>) -> Self {
            Self {
                connection: connection,
                peer_state: PeerState::new(),
                bitfield: Vec::new(),
            }
        }

        pub async fn download_piece(
            &mut self,
            piece_index: usize,
            t: &Info,
            work_queue: &WorkQueue,
            buffer: Arc<tokio::sync::Mutex<Vec<u8>>>,
        ) -> Result<()> {
            let piece_hash = &t.pieces.0[piece_index];
            let piece_size = if piece_index == t.pieces.0.len() - 1 {
                let md = t.calculate_length() % t.plength;
                if md == 0 {
                    t.plength
                } else {
                    md
                }
            } else {
                t.plength
            };

            let nblocks = (piece_size + (BLOCK_MAX - 1)) / BLOCK_MAX;
            let mut all_blocks = Vec::<u8>::with_capacity(piece_size);

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
                    piece_index as u32,
                    (block * BLOCK_MAX) as u32,
                    block_size as u32,
                );

                let request_bytes = Vec::from(request.as_bytes_mut());
                self.connection
                    .send(Message {
                        tag: MessageTag::Request,
                        payload: request_bytes,
                    })
                    .await
                    .with_context(|| format!("send request for block {block}"))?;

                let piece_msg = self
                    .connection
                    .next()
                    .await
                    .expect("peer always sends a piece")
                    .context("peer message was invalid")?;

                let piece = Piece::ref_from_bytes(&piece_msg.payload[..])
                    .expect("always get all Piece response fields from peer");

                all_blocks.extend(piece.block());
            }

            let mut hasher = Sha1::new();
            hasher.update(&all_blocks);
            let hash: [u8; 20] = hasher
                .finalize()
                .try_into()
                .expect("GenericArray<_, 20> == [_; 20]");
            if hash != *piece_hash {
                println!("Piece {piece_index} failed hash check");
                work_queue.return_piece(piece_index).await;
                return Err(anyhow::anyhow!("Hash mismatch for piece {}", piece_index));
            }

            // If integrity check passes, write the piece to buffer
            let mut buffer = buffer.lock().await;
            buffer.extend(all_blocks);

            println!("Successfully downloaded and verified piece {}", piece_index);
            if piece_index == t.pieces.0.len() - 1 {
                work_queue.sender.send(999999).await.unwrap();
            }
            Ok(())
        }

        pub async fn start_exchanging_messages(
            &mut self,
            torrent: &Torrent,
            work_queue: &WorkQueue,
            buffer: Arc<tokio::sync::Mutex<Vec<u8>>>,
        ) {
            //step 1. do handshake
            let handshake = self.exchange_handshakes(torrent).await;

            //step 2. get bitfield
            // self.bitfield = self
            //     .exchange_bitfields()
            //     .await
            //     .expect("should return bitfield")
            //     .payload;

            //step 3. send interested message
            self.send_message(MessageTag::Interested, Vec::new())
                .await
                .expect("should send intersted message");

            self.peer_state.am_interested = true;

            //step 4. start trying to download

            while let Some(piece_index) = work_queue.get_piece().await {
                let piece_size =
                    ActivePeer::get_piece_size(piece_index, &torrent.torrent_file.info);
                let nblocks = (piece_size + (BLOCK_MAX - 1)) / BLOCK_MAX;
                let mut all_blocks = Vec::<u8>::with_capacity(piece_size);

                let mut blocks_to_download: VecDeque<usize> = (0..nblocks).collect();
                while !blocks_to_download.is_empty() {
                    //send request for block
                    if !self.peer_state.peer_choking {
                        let block_index = blocks_to_download.pop_front().unwrap();
                        match self
                            .send_block_request(
                                piece_index,
                                block_index,
                                &torrent.torrent_file.info,
                            )
                            .await
                        {
                            Ok(()) => {}
                            Err(_) => {
                                blocks_to_download.push_back(block_index);
                            }
                        }
                    }
                    //wait for response
                    let message = self
                        .connection
                        .next()
                        .await
                        .expect("recieve message from peer")
                        .context("invalid message from peer");

                    let message = match message {
                        Err(e) => {
                            println!("{}", e);
                            //blocks_to_download.push_back(block_index);
                            break;
                        }
                        Ok(recv_message) => recv_message,
                    };

                    //process the message
                    match message.tag {
                        MessageTag::Choke => {
                            self.peer_state.peer_choking = true;
                            println!("choked");
                        }
                        MessageTag::Unchoke => {
                            self.peer_state.peer_choking = false;
                            println!("unchocked");
                        }
                        MessageTag::Interested => {}
                        MessageTag::NotInterested => {}
                        MessageTag::Have => {
                            println!("recieved a have message");
                        }
                        MessageTag::Bitfield => self.bitfield = message.payload,
                        MessageTag::Request => {}
                        MessageTag::Piece => {
                            let piece = Piece::ref_from_bytes(&message.payload[..])
                                .expect("always get all Piece response fields from peer");

                            all_blocks.extend(piece.block());
                        }
                        MessageTag::Cancel => {}
                    }
                }

                if blocks_to_download.is_empty() {
                    let mut hasher = Sha1::new();
                    hasher.update(&all_blocks);
                    let hash: [u8; 20] = hasher
                        .finalize()
                        .try_into()
                        .expect("GenericArray<_, 20> == [_; 20]");
                    let piece_hash = &torrent.torrent_file.info.pieces.0[piece_index];
                    if hash != *piece_hash {
                        println!("Piece {} failed hash check", piece_index + 1);
                        work_queue.return_piece(piece_index).await;
                        all_blocks = Vec::<u8>::new();
                    }

                    if !all_blocks.is_empty() {
                        let mut buffer = buffer.lock().await;
                        buffer.extend(all_blocks);

                        println!(
                            "Successfully downloaded and verified piece {} : {}",
                            piece_index + 1,
                            torrent.torrent_file.info.pieces.0.len()
                        );
                    }
                }
            }
        }

        pub async fn exchange_handshakes(&mut self, torrent: &Torrent) -> Result<Handshake> {
            let mut handshake = Handshake::new(torrent.info_hash, PEER_ID);
            {
                let handshake_bytes = handshake.as_bytes_mut();
                self.connection
                    .get_mut()
                    .write_all(handshake_bytes)
                    .await
                    .context("write handshake")?;
                self.connection
                    .get_mut()
                    .read_exact(handshake_bytes)
                    .await
                    .context("read handshake")?;
            }

            Ok(handshake)
        }

        pub async fn exchange_bitfields(&mut self) -> Result<Message> {
            let bitfield = self
                .connection
                .next()
                .await
                .expect("peer always sends a bitfields")
                .context("peer message was invalid")?;
            Ok(bitfield)
        }

        pub async fn send_message(
            &mut self,
            message_tag: MessageTag,
            payload: Vec<u8>,
        ) -> Result<()> {
            self.connection
                .send(Message {
                    tag: message_tag,
                    payload: payload,
                })
                .await
                .context("send interested message")
        }

        pub async fn send_block_request(
            &mut self,
            piece_index: usize,
            block: usize,
            info: &Info,
        ) -> Result<(), Error> {
            let piece_size = ActivePeer::get_piece_size(piece_index, info);
            let nblocks = (piece_size + (BLOCK_MAX - 1)) / BLOCK_MAX;

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
                piece_index as u32,
                (block * BLOCK_MAX) as u32,
                block_size as u32,
            );

            let request_bytes = Vec::from(request.as_bytes_mut());
            self.connection
                .send(Message {
                    tag: MessageTag::Request,
                    payload: request_bytes,
                })
                .await
                .with_context(|| format!("send request for block {block}"))
        }
        pub fn get_piece_size(piece_index: usize, t: &Info) -> usize {
            let piece_size = if piece_index == t.pieces.0.len() - 1 {
                let md = t.calculate_length() % t.plength;
                if md == 0 {
                    t.plength
                } else {
                    md
                }
            } else {
                t.plength
            };

            piece_size
        }
    }
}
