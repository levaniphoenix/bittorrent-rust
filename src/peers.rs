pub mod peers {
    use anyhow::Result;
    use bytes::{Buf, BufMut, BytesMut};
    use futures_util::{SinkExt, StreamExt};
    use serde::de::{self, Deserialize, Deserializer, Visitor};
    use serde::ser::{Serialize, Serializer};
    use sha1::Digest;
    use std::fmt;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio::time;
    use tokio_util::codec::Decoder;
    use tokio_util::codec::Encoder;

    use crate::activepeer::activepeer::ActivePeer;

    const BLOCK_MAX: usize = 1 << 14;

    #[derive(Debug, Clone)]
    pub struct Peer {
        pub ip4: SocketAddrV4,
    }

    impl Peer {
        pub fn new(ip4: SocketAddrV4) -> Self {
            Self { ip4 }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Peers(pub Vec<Peer>);

    struct PeersVisitor;
    impl<'de> Visitor<'de> for PeersVisitor {
        type Value = Peers;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("6 bytes, the first 4 bytes are a peer's IP address and the last 2 are a peer's port number")
        }
        fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if v.len() % 6 != 0 {
                return Err(E::custom(format!("length is {}", v.len())));
            }
            // TODO: use array_chunks when stable; then we can also pattern-match in closure args
            Ok(Peers(
                v.chunks_exact(6)
                    .map(|slice_6| {
                        Peer::new(SocketAddrV4::new(
                            Ipv4Addr::new(slice_6[0], slice_6[1], slice_6[2], slice_6[3]),
                            u16::from_be_bytes([slice_6[4], slice_6[5]]),
                        ))
                    })
                    .collect(),
            ))
        }
    }
    impl<'de> Deserialize<'de> for Peers {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_bytes(PeersVisitor)
        }
    }
    impl Serialize for Peers {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut single_slice = Vec::with_capacity(6 * self.0.len());
            for peer in &self.0 {
                single_slice.extend(peer.ip4.ip().octets());
                single_slice.extend(peer.ip4.port().to_be_bytes());
            }
            serializer.serialize_bytes(&single_slice)
        }
    }

    #[derive(Debug, Clone)]
    #[repr(C)]
    pub struct Request {
        index: [u8; 4],
        begin: [u8; 4],
        length: [u8; 4],
    }
    impl Request {
        pub fn new(index: u32, begin: u32, length: u32) -> Self {
            Self {
                index: index.to_be_bytes(),
                begin: begin.to_be_bytes(),
                length: length.to_be_bytes(),
            }
        }
        pub fn index(&self) -> u32 {
            u32::from_be_bytes(self.index)
        }
        pub fn begin(&self) -> u32 {
            u32::from_be_bytes(self.begin)
        }
        pub fn length(&self) -> u32 {
            u32::from_be_bytes(self.length)
        }
        pub fn as_bytes_mut(&mut self) -> &mut [u8] {
            let bytes = self as *mut Self as *mut [u8; std::mem::size_of::<Self>()];
            // Safety: Handshake is a POD with repr(c)
            let bytes: &mut [u8; std::mem::size_of::<Self>()] = unsafe { &mut *bytes };
            bytes
        }
    }

    #[repr(C)]
    pub struct Piece<T: ?Sized = [u8]> {
        index: [u8; 4],
        begin: [u8; 4],
        block: T,
    }

    impl Piece {
        pub fn index(&self) -> u32 {
            u32::from_be_bytes(self.index)
        }
        pub fn begin(&self) -> u32 {
            u32::from_be_bytes(self.begin)
        }
        pub fn block(&self) -> &[u8] {
            &self.block
        }
        const PIECE_LEAD: usize = std::mem::size_of::<Piece<()>>();
        pub fn ref_from_bytes(data: &[u8]) -> Option<&Self> {
            if data.len() < Self::PIECE_LEAD {
                return None;
            }
            let n = data.len();
            // NOTE: The slicing here looks really weird. The reason we do it is because we need the
            // length part of the fat pointer to Piece to old the length of _just_ the `block` field.
            // And the only way we can change the length of the fat pointer to Piece is by changing the
            // length of the fat pointer to the slice, which we do by slicing it. We can't slice it at
            // the front (as it would invalidate the ptr part of the fat pointer), so we slice it at
            // the back!
            let piece = &data[..n - Self::PIECE_LEAD] as *const [u8] as *const Piece;
            // Safety: Piece is a POD with repr(c), _and_ the fat pointer data length is the length of
            // the trailing DST field (thanks to the PIECE_LEAD offset).
            Some(unsafe { &*piece })
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    pub enum MessageTag {
        Choke = 0,
        Unchoke = 1,
        Interested = 2,
        NotInterested = 3,
        Have = 4,
        Bitfield = 5,
        Request = 6,
        Piece = 7,
        Cancel = 8,
    }
    #[derive(Debug, Clone)]
    pub struct Message {
        pub tag: MessageTag,
        pub payload: Vec<u8>,
    }

    pub struct MessageFramer;
    const MAX: usize = 1 << 16;
    impl Decoder for MessageFramer {
        type Item = Message;
        type Error = std::io::Error;
        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            if src.len() < 4 {
                // Not enough data to read length marker.
                return Ok(None);
            }
            // Read length marker.
            let mut length_bytes = [0u8; 4];
            length_bytes.copy_from_slice(&src[..4]);
            let length = u32::from_be_bytes(length_bytes) as usize;
            if length == 0 {
                // this is a heartbeat message.
                // discard it.
                src.advance(4);
                // and then try again in case the buffer has more messages
                return self.decode(src);
            }
            if src.len() < 5 {
                // Not enough data to read tag marker.
                return Ok(None);
            }
            // Check that the length is not too large to avoid a denial of
            // service attack where the server runs out of memory.
            if length > MAX {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Frame of length {} is too large.", length),
                ));
            }
            if src.len() < 4 + length {
                // The full string has not yet arrived.
                //
                // We reserve more space in the buffer. This is not strictly
                // necessary, but is a good idea performance-wise.
                src.reserve(4 + length - src.len());
                // We inform the Framed that we need more bytes to form the next
                // frame.
                return Ok(None);
            }
            // Use advance to modify src such that it no longer contains
            // this frame.
            let tag = match src[4] {
                0 => MessageTag::Choke,
                1 => MessageTag::Unchoke,
                2 => MessageTag::Interested,
                3 => MessageTag::NotInterested,
                4 => MessageTag::Have,
                5 => MessageTag::Bitfield,
                6 => MessageTag::Request,
                7 => MessageTag::Piece,
                8 => MessageTag::Cancel,
                tag => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Unknown message type {}.", tag),
                    ))
                }
            };
            let data = if src.len() > 5 {
                src[5..4 + length].to_vec()
            } else {
                Vec::new()
            };
            src.advance(4 + length);
            Ok(Some(Message { tag, payload: data }))
        }
    }
    impl Encoder<Message> for MessageFramer {
        type Error = std::io::Error;
        fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
            // Don't send a message if it is longer than the other end will
            // accept.
            if item.payload.len() + 1 > MAX {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Frame of length {} is too large.", item.payload.len()),
                ));
            }
            // Convert the length into a byte array.
            let len_slice = u32::to_be_bytes(item.payload.len() as u32 + 1);
            // Reserve space in the buffer.
            dst.reserve(4 /* length */ + 1 /* tag */ + item.payload.len());
            // Write the length and string to the buffer.
            dst.extend_from_slice(&len_slice);
            dst.put_u8(item.tag as u8);
            dst.extend_from_slice(&item.payload);
            Ok(())
        }
    }

    pub struct WorkQueue {
        pub sender: mpsc::Sender<usize>,
        pub receiver: tokio::sync::Mutex<mpsc::Receiver<usize>>,
    }

    impl WorkQueue {
        pub fn new(pieces: Vec<usize>) -> Self {
            let (sender, receiver) = mpsc::channel(pieces.len());
            for piece in pieces {
                let _ = sender.try_send(piece); // Load initial pieces
            }
            WorkQueue {
                sender,
                receiver: tokio::sync::Mutex::new(receiver),
            }
        }

        pub async fn get_piece(&self) -> Option<usize> {
            let mut receiver = self.receiver.lock().await;

            if receiver.is_empty() {
                return None;
            }

            receiver.recv().await
        }

        pub async fn return_piece(&self, piece_index: usize) {
            let _ = self.sender.send(piece_index).await;
        }
    }

    pub async fn connect_to_peer(peer: &Peer) -> Option<ActivePeer> {
        let timeout_duration = Duration::from_secs(2);
        println!("connecting to {:?}", peer.ip4);
        let connection_attempt =
            time::timeout(timeout_duration, TcpStream::connect(peer.ip4)).await;
        match connection_attempt {
            Ok(Ok(stream)) => Some(ActivePeer::new(tokio_util::codec::Framed::new(
                stream,
                MessageFramer,
            ))),
            Ok(Err(_)) => {
                println!("Failed to connect to peer {:?}", peer.ip4);
                None
            }
            Err(_) => {
                println!("Failed to connect to peer {:?}", peer.ip4);
                None
            }
        }
    }
}
