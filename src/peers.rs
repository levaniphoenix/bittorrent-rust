pub mod peers {
    use serde::de::{self, Deserialize, Deserializer, Visitor};
    use serde::ser::{Serialize, Serializer};
    use std::fmt;
    use std::net::{Ipv4Addr, SocketAddrV4};
    #[derive(Debug, Clone)]
    pub struct Peers(pub Vec<Peer>);

    #[derive(Debug, Clone)]
    pub struct Peer {
        pub ip4: SocketAddrV4,
    }

    impl Peer {
        pub fn new(ip4: SocketAddrV4) -> Self {
            Self { ip4 }
        }
    }
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
}
