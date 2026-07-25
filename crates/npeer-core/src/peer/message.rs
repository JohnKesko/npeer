//! Peer wire messages: a 4-byte big-endian length prefix, a 1-byte id, and
//! an id-specific payload. A zero-length message is a keep-alive.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};

/// Upper bound for a single incoming message. Large enough for the bitfield
/// of any realistic torrent, small enough to stop a hostile peer making us
/// allocate without bound.
const MAX_MESSAGE_SIZE: u32 = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        data: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// BEP 10 extension message: extension id plus raw payload.
    Extended {
        id: u8,
        payload: Vec<u8>,
    },
    /// A message id we do not understand; ignored but kept for logging.
    Unknown {
        id: u8,
    },
}

impl Message {
    /// Short name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Message::KeepAlive => "keep-alive",
            Message::Choke => "choke",
            Message::Unchoke => "unchoke",
            Message::Interested => "interested",
            Message::NotInterested => "not-interested",
            Message::Have(_) => "have",
            Message::Bitfield(_) => "bitfield",
            Message::Request { .. } => "request",
            Message::Piece { .. } => "piece",
            Message::Cancel { .. } => "cancel",
            Message::Extended { .. } => "extended",
            Message::Unknown { .. } => "unknown",
        }
    }
}

/// Read one framed message.
pub async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message> {
    let mut length_bytes = [0u8; 4];
    reader.read_exact(&mut length_bytes).await?;
    let length = u32::from_be_bytes(length_bytes);

    if length == 0 {
        return Ok(Message::KeepAlive);
    }
    if length > MAX_MESSAGE_SIZE {
        return Err(Error::protocol(format!(
            "peer sent an oversized message ({length} bytes)"
        )));
    }

    let mut body = vec![0u8; length as usize];
    reader.read_exact(&mut body).await?;
    let id = body[0];
    let payload = &body[1..];

    let message = match id {
        0 => Message::Choke,
        1 => Message::Unchoke,
        2 => Message::Interested,
        3 => Message::NotInterested,
        4 => Message::Have(read_u32(payload, 0)?),
        5 => Message::Bitfield(payload.to_vec()),
        6 => Message::Request {
            index: read_u32(payload, 0)?,
            begin: read_u32(payload, 4)?,
            length: read_u32(payload, 8)?,
        },
        7 => {
            if payload.len() < 8 {
                return Err(Error::protocol("piece message shorter than its header"));
            }
            Message::Piece {
                index: read_u32(payload, 0)?,
                begin: read_u32(payload, 4)?,
                data: payload[8..].to_vec(),
            }
        }
        8 => Message::Cancel {
            index: read_u32(payload, 0)?,
            begin: read_u32(payload, 4)?,
            length: read_u32(payload, 8)?,
        },
        20 => {
            if payload.is_empty() {
                return Err(Error::protocol("extended message without an extension id"));
            }
            Message::Extended {
                id: payload[0],
                payload: payload[1..].to_vec(),
            }
        }
        other => Message::Unknown { id: other },
    };
    Ok(message)
}

/// Write one framed message.
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: &Message) -> Result<()> {
    let mut body: Vec<u8> = Vec::new();
    match message {
        Message::KeepAlive => {}
        Message::Choke => body.push(0),
        Message::Unchoke => body.push(1),
        Message::Interested => body.push(2),
        Message::NotInterested => body.push(3),
        Message::Have(index) => {
            body.push(4);
            body.extend_from_slice(&index.to_be_bytes());
        }
        Message::Bitfield(bits) => {
            body.push(5);
            body.extend_from_slice(bits);
        }
        Message::Request {
            index,
            begin,
            length,
        } => {
            body.push(6);
            body.extend_from_slice(&index.to_be_bytes());
            body.extend_from_slice(&begin.to_be_bytes());
            body.extend_from_slice(&length.to_be_bytes());
        }
        Message::Piece { index, begin, data } => {
            body.push(7);
            body.extend_from_slice(&index.to_be_bytes());
            body.extend_from_slice(&begin.to_be_bytes());
            body.extend_from_slice(data);
        }
        Message::Cancel {
            index,
            begin,
            length,
        } => {
            body.push(8);
            body.extend_from_slice(&index.to_be_bytes());
            body.extend_from_slice(&begin.to_be_bytes());
            body.extend_from_slice(&length.to_be_bytes());
        }
        Message::Extended { id, payload } => {
            body.push(20);
            body.push(*id);
            body.extend_from_slice(payload);
        }
        Message::Unknown { .. } => return Ok(()),
    }

    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    framed.extend_from_slice(&body);
    writer.write_all(&framed).await?;
    Ok(())
}

fn read_u32(payload: &[u8], offset: usize) -> Result<u32> {
    payload
        .get(offset..offset + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| Error::protocol("message payload shorter than expected"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn round_trip(message: Message) -> Message {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &message).await.unwrap();
        read_message(&mut buffer.as_slice()).await.unwrap()
    }

    #[tokio::test]
    async fn messages_round_trip() {
        for message in [
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(42),
            Message::Bitfield(vec![0b1010_0000]),
            Message::Request {
                index: 1,
                begin: 16384,
                length: 16384,
            },
            Message::Piece {
                index: 1,
                begin: 0,
                data: vec![1, 2, 3],
            },
            Message::Cancel {
                index: 1,
                begin: 0,
                length: 16384,
            },
            Message::Extended {
                id: 0,
                payload: b"d1:md11:ut_metadatai1eee".to_vec(),
            },
        ] {
            assert_eq!(round_trip(message.clone()).await, message);
        }
    }

    #[tokio::test]
    async fn request_wire_format_matches_spec() {
        // length=13, id=6, index/begin/length big-endian: the exact byte
        // layout the reference implementation produced.
        let mut buffer = Vec::new();
        write_message(
            &mut buffer,
            &Message::Request {
                index: 2,
                begin: 16384,
                length: 1024,
            },
        )
        .await
        .unwrap();
        assert_eq!(buffer[..5], [0, 0, 0, 13, 6]);
        assert_eq!(buffer[5..9], 2u32.to_be_bytes());
        assert_eq!(buffer[9..13], 16384u32.to_be_bytes());
        assert_eq!(buffer[13..17], 1024u32.to_be_bytes());
    }

    #[tokio::test]
    async fn rejects_oversized_messages() {
        let mut evil = Vec::new();
        evil.extend_from_slice(&(MAX_MESSAGE_SIZE + 1).to_be_bytes());
        evil.push(5);
        let err = read_message(&mut evil.as_slice()).await.unwrap_err();
        assert!(err.to_string().contains("oversized"));
    }

    #[tokio::test]
    async fn unknown_ids_are_tolerated() {
        let framed = [0u8, 0, 0, 1, 99];
        let message = read_message(&mut framed.as_slice()).await.unwrap();
        assert_eq!(message, Message::Unknown { id: 99 });
    }
}
