//! Extension protocol (BEP 10) and metadata exchange (BEP 9).
//!
//! Wire behaviour matches the proven implementation: the extension handshake
//! is extended message 0 carrying a bencoded dictionary with an `m` map, and
//! `ut_metadata` messages carry a bencoded header optionally followed by a
//! raw 16 KiB metadata slice.

use std::collections::BTreeMap;

use crate::bencode::{self, Value};
use crate::error::{Error, Result};

/// Extension message id of the extension handshake itself.
pub const HANDSHAKE_ID: u8 = 0;
/// The id we advertise for `ut_metadata`; peers must use it when talking to us.
pub const OUR_UT_METADATA_ID: u8 = 1;
/// Metadata is exchanged in 16 KiB slices.
pub const METADATA_PIECE_SIZE: usize = 16 * 1024;

/// The parts of a peer's extension handshake we care about.
#[derive(Clone, Debug, Default)]
pub struct ExtensionHandshake {
    /// The id the *peer* chose for ut_metadata messages sent to it.
    pub ut_metadata_id: Option<u8>,
    /// Size of the info dictionary in bytes, needed to fetch magnet metadata.
    pub metadata_size: Option<u64>,
    /// Peer client name and version, when advertised.
    pub client: Option<String>,
}

/// Build our extension handshake payload (excluding the message framing).
pub fn build_handshake(metadata_size: Option<u64>) -> Vec<u8> {
    let mut m = BTreeMap::new();
    m.insert(
        b"ut_metadata".to_vec(),
        Value::Int(i64::from(OUR_UT_METADATA_ID)),
    );

    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), Value::Dict(m));
    root.insert(
        b"v".to_vec(),
        Value::Bytes(
            concat!("npeer ", env!("CARGO_PKG_VERSION"))
                .as_bytes()
                .to_vec(),
        ),
    );
    if let Some(size) = metadata_size {
        root.insert(b"metadata_size".to_vec(), Value::Int(size as i64));
    }
    bencode::encode(&Value::Dict(root))
}

/// Parse a peer's extension handshake payload.
pub fn parse_handshake(payload: &[u8]) -> Result<ExtensionHandshake> {
    // Some clients append data after the dictionary; only the prefix matters.
    let (root, _) = bencode::decode_prefix(payload)
        .map_err(|_| Error::protocol("extension handshake is not valid bencode"))?;

    let ut_metadata_id = root
        .get(b"m")
        .and_then(|m| m.get(b"ut_metadata"))
        .and_then(Value::as_int)
        .and_then(|id| u8::try_from(id).ok());
    let metadata_size = root
        .get(b"metadata_size")
        .and_then(Value::as_int)
        .and_then(|n| u64::try_from(n).ok());
    let client = root.get(b"v").and_then(Value::as_str).map(str::to_owned);

    Ok(ExtensionHandshake {
        ut_metadata_id,
        metadata_size,
        client,
    })
}

/// A `ut_metadata` message, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataMessage {
    Request {
        piece: u32,
    },
    Data {
        piece: u32,
        total_size: Option<u64>,
        data: Vec<u8>,
    },
    Reject {
        piece: u32,
    },
}

/// Build a `ut_metadata` request payload for one metadata piece.
pub fn build_metadata_request(piece: u32) -> Vec<u8> {
    let mut dict = BTreeMap::new();
    dict.insert(b"msg_type".to_vec(), Value::Int(0));
    dict.insert(b"piece".to_vec(), Value::Int(i64::from(piece)));
    bencode::encode(&Value::Dict(dict))
}

/// Build a `ut_metadata` data payload: bencoded header + raw slice.
pub fn build_metadata_data(piece: u32, total_size: u64, data: &[u8]) -> Vec<u8> {
    let mut dict = BTreeMap::new();
    dict.insert(b"msg_type".to_vec(), Value::Int(1));
    dict.insert(b"piece".to_vec(), Value::Int(i64::from(piece)));
    dict.insert(b"total_size".to_vec(), Value::Int(total_size as i64));
    let mut out = bencode::encode(&Value::Dict(dict));
    out.extend_from_slice(data);
    out
}

/// Build a `ut_metadata` reject payload.
pub fn build_metadata_reject(piece: u32) -> Vec<u8> {
    let mut dict = BTreeMap::new();
    dict.insert(b"msg_type".to_vec(), Value::Int(2));
    dict.insert(b"piece".to_vec(), Value::Int(i64::from(piece)));
    bencode::encode(&Value::Dict(dict))
}

/// Parse a `ut_metadata` payload. For data messages the raw metadata slice
/// starts right after the bencoded header dictionary.
pub fn parse_metadata_message(payload: &[u8]) -> Result<MetadataMessage> {
    let (header, header_len) = bencode::decode_prefix(payload)
        .map_err(|_| Error::protocol("ut_metadata header is not valid bencode"))?;
    let msg_type = header
        .get(b"msg_type")
        .and_then(Value::as_int)
        .ok_or_else(|| Error::protocol("ut_metadata message has no msg_type"))?;
    let piece = header
        .get(b"piece")
        .and_then(Value::as_int)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| Error::protocol("ut_metadata message has no piece number"))?;

    match msg_type {
        0 => Ok(MetadataMessage::Request { piece }),
        1 => {
            let total_size = header
                .get(b"total_size")
                .and_then(Value::as_int)
                .and_then(|n| u64::try_from(n).ok());
            Ok(MetadataMessage::Data {
                piece,
                total_size,
                data: payload[header_len..].to_vec(),
            })
        }
        2 => Ok(MetadataMessage::Reject { piece }),
        other => Err(Error::protocol(format!(
            "unknown ut_metadata msg_type {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trip() {
        let payload = build_handshake(Some(1234));
        // Must be canonical bencode with sorted keys `m`, `metadata_size`, `v`.
        assert!(payload.starts_with(b"d1:md11:ut_metadatai1ee13:metadata_sizei1234e"));
        let parsed = parse_handshake(&payload).unwrap();
        assert_eq!(parsed.ut_metadata_id, Some(1));
        assert_eq!(parsed.metadata_size, Some(1234));
        assert!(parsed.client.unwrap().starts_with("npeer"));
    }

    #[test]
    fn handshake_without_metadata_support() {
        let parsed = parse_handshake(b"d1:v4:abcde").unwrap();
        assert_eq!(parsed.ut_metadata_id, None);
        assert_eq!(parsed.metadata_size, None);
    }

    #[test]
    fn metadata_request_matches_reference_bytes() {
        // The reference built exactly `d8:msg_typei0e5:piecei0ee`.
        assert_eq!(build_metadata_request(0), b"d8:msg_typei0e5:piecei0ee");
    }

    #[test]
    fn metadata_data_round_trip() {
        let payload = build_metadata_data(2, 40_000, b"raw-info-bytes");
        match parse_metadata_message(&payload).unwrap() {
            MetadataMessage::Data {
                piece,
                total_size,
                data,
            } => {
                assert_eq!(piece, 2);
                assert_eq!(total_size, Some(40_000));
                assert_eq!(data, b"raw-info-bytes");
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn metadata_reject_round_trip() {
        let payload = build_metadata_reject(7);
        assert_eq!(
            parse_metadata_message(&payload).unwrap(),
            MetadataMessage::Reject { piece: 7 }
        );
    }
}
