//! Bencode decoding and encoding (BEP 3).
//!
//! The decoder mirrors the behaviour proven in the original implementation:
//! a recursive descent parser that reports how many bytes each value
//! consumed. On top of that it adds error handling (no panics on untrusted
//! input), a recursion depth limit, and an exact way to locate the raw bytes
//! of the `info` dictionary so the info hash is computed over the original
//! encoding rather than a re-serialisation.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::error::{Error, Result};

/// Maximum nesting depth accepted from untrusted input.
const MAX_DEPTH: usize = 64;

/// A decoded bencode value.
///
/// Dictionary keys are raw byte strings; a `BTreeMap` keeps them in the
/// sorted order the spec requires for canonical encoding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Look up a key in a dictionary value.
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        self.as_dict().and_then(|d| d.get(key))
    }
}

/// Decode a complete bencode document, rejecting trailing bytes.
pub fn decode(bytes: &[u8]) -> Result<Value> {
    let (value, consumed) = decode_prefix(bytes)?;
    if consumed != bytes.len() {
        return Err(Error::Bencode(format!(
            "trailing data after value ({} unused bytes)",
            bytes.len() - consumed
        )));
    }
    Ok(value)
}

/// Decode one value from the front of `bytes`, returning it together with
/// the number of bytes consumed.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Value, usize)> {
    decode_at_depth(bytes, 0)
}

fn decode_at_depth(bytes: &[u8], depth: usize) -> Result<(Value, usize)> {
    if depth > MAX_DEPTH {
        return Err(Error::Bencode("value nested too deeply".into()));
    }

    match bytes.first() {
        Some(b'i') => decode_int(bytes),
        Some(c) if c.is_ascii_digit() => {
            let (bytes, consumed) = decode_bytes(bytes)?;
            Ok((Value::Bytes(bytes), consumed))
        }
        Some(b'l') => {
            let mut values = Vec::new();
            let mut pos = 1;
            loop {
                match bytes.get(pos) {
                    Some(b'e') => return Ok((Value::List(values), pos + 1)),
                    Some(_) => {
                        let (value, used) = decode_at_depth(&bytes[pos..], depth + 1)?;
                        values.push(value);
                        pos += used;
                    }
                    None => return Err(Error::Bencode("unterminated list".into())),
                }
            }
        }
        Some(b'd') => {
            let mut map = BTreeMap::new();
            let mut pos = 1;
            loop {
                match bytes.get(pos) {
                    Some(b'e') => return Ok((Value::Dict(map), pos + 1)),
                    Some(_) => {
                        let (key, key_used) = decode_bytes(&bytes[pos..])?;
                        pos += key_used;
                        let (value, value_used) = decode_at_depth(&bytes[pos..], depth + 1)?;
                        pos += value_used;
                        map.insert(key, value);
                    }
                    None => return Err(Error::Bencode("unterminated dictionary".into())),
                }
            }
        }
        Some(other) => Err(Error::Bencode(format!("unexpected byte 0x{other:02x}"))),
        None => Err(Error::Bencode("empty input".into())),
    }
}

fn decode_int(bytes: &[u8]) -> Result<(Value, usize)> {
    let end = bytes
        .iter()
        .position(|&b| b == b'e')
        .ok_or_else(|| Error::Bencode("unterminated integer".into()))?;
    let text = std::str::from_utf8(&bytes[1..end])
        .map_err(|_| Error::Bencode("integer is not ASCII".into()))?;
    let number: i64 = text
        .parse()
        .map_err(|_| Error::Bencode(format!("invalid integer `{text}`")))?;
    Ok((Value::Int(number), end + 1))
}

fn decode_bytes(bytes: &[u8]) -> Result<(Vec<u8>, usize)> {
    let colon = bytes
        .iter()
        .position(|&b| b == b':')
        .ok_or_else(|| Error::Bencode("string is missing `:`".into()))?;
    let len_text = std::str::from_utf8(&bytes[..colon])
        .map_err(|_| Error::Bencode("string length is not ASCII".into()))?;
    let len: usize = len_text
        .parse()
        .map_err(|_| Error::Bencode(format!("invalid string length `{len_text}`")))?;

    let start = colon + 1;
    let end = start
        .checked_add(len)
        .filter(|&end| end <= bytes.len())
        .ok_or_else(|| Error::Bencode("string is longer than the input".into()))?;
    Ok((bytes[start..end].to_vec(), end))
}

/// Encode a value back into bencode form.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
        }
        Value::List(values) => {
            out.push(b'l');
            for value in values {
                encode_into(value, out);
            }
            out.push(b'e');
        }
        Value::Dict(map) => {
            out.push(b'd');
            for (key, value) in map {
                encode_into(&Value::Bytes(key.clone()), out);
                encode_into(value, out);
            }
            out.push(b'e');
        }
    }
}

/// Locate the raw bytes of the top-level `info` dictionary in a torrent file.
///
/// Walks the outer dictionary token by token so it cannot be fooled by an
/// `info`-like byte pattern inside another string, which a plain substring
/// search could be.
pub fn top_level_value_span(document: &[u8], wanted_key: &[u8]) -> Result<Range<usize>> {
    if document.first() != Some(&b'd') {
        return Err(Error::Bencode("document is not a dictionary".into()));
    }
    let mut pos = 1;
    loop {
        match document.get(pos) {
            Some(b'e') | None => {
                let key = String::from_utf8_lossy(wanted_key);
                return Err(Error::Bencode(format!("dictionary has no `{key}` key")));
            }
            Some(_) => {
                let (key, key_used) = decode_bytes(&document[pos..])?;
                pos += key_used;
                let (_, value_used) = decode_at_depth(&document[pos..], 1)?;
                if key == wanted_key {
                    return Ok(pos..pos + value_used);
                }
                pos += value_used;
            }
        }
    }
}

/// Render a decoded value as JSON-compatible `serde_json::Value`.
///
/// Byte strings that are not UTF-8 are shown as hex, matching the behaviour
/// of the original `decode` command.
pub fn to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Bytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(s) => serde_json::Value::String(s.to_owned()),
            Err(_) => serde_json::Value::String(hex::encode(bytes)),
        },
        Value::List(values) => serde_json::Value::Array(values.iter().map(to_json).collect()),
        Value::Dict(map) => {
            let mut object = serde_json::Map::new();
            for (key, value) in map {
                let key = match std::str::from_utf8(key) {
                    Ok(s) => s.to_owned(),
                    Err(_) => hex::encode(key),
                };
                object.insert(key, to_json(value));
            }
            serde_json::Value::Object(object)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> Value {
        Value::Bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn decodes_primitives() {
        assert_eq!(decode(b"i52e").unwrap(), Value::Int(52));
        assert_eq!(decode(b"i-42e").unwrap(), Value::Int(-42));
        assert_eq!(decode(b"5:hello").unwrap(), bytes("hello"));
        assert_eq!(decode(b"0:").unwrap(), bytes(""));
    }

    #[test]
    fn decodes_containers() {
        assert_eq!(
            decode(b"l5:helloi52ee").unwrap(),
            Value::List(vec![bytes("hello"), Value::Int(52)])
        );
        let dict = decode(b"d3:foo3:bar5:helloi52ee").unwrap();
        assert_eq!(dict.get(b"foo"), Some(&bytes("bar")));
        assert_eq!(dict.get(b"hello"), Some(&Value::Int(52)));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode(b"").is_err());
        assert!(decode(b"i52").is_err());
        assert!(decode(b"5:hi").is_err());
        assert!(decode(b"l5:hello").is_err());
        assert!(decode(b"x").is_err());
        assert!(
            decode(b"i52ei1e").is_err(),
            "trailing data must be rejected"
        );
    }

    #[test]
    fn rejects_deep_nesting() {
        let mut evil = vec![b'l'; 1000];
        evil.extend(vec![b'e'; 1000]);
        assert!(decode(&evil).is_err());
    }

    #[test]
    fn encode_round_trips() {
        let original = b"d3:bari-7e4:listl2:hii0ee3:zzz3:\x00\x01\x02e".to_vec();
        let value = decode(&original).unwrap();
        assert_eq!(encode(&value), original);
    }

    #[test]
    fn finds_info_span() {
        let doc = b"d8:announce3:url4:infod4:name4:test6:lengthi5eee";
        // Keys are unsorted here on purpose; the walker does not care.
        let span = top_level_value_span(doc, b"info").unwrap();
        let raw = &doc[span];
        assert!(raw.starts_with(b"d4:name"));
        assert_eq!(decode(raw).unwrap().get(b"length"), Some(&Value::Int(5)));
    }

    #[test]
    fn info_span_not_fooled_by_string_contents() {
        // The value of `comment` contains the bytes `4:infod`.
        let doc = b"d7:comment9:x4:infody4:infod6:lengthi1eee";
        let span = top_level_value_span(doc, b"info").unwrap();
        assert!(doc[span].starts_with(b"d6:length"));
    }

    #[test]
    fn json_rendering_uses_hex_for_binary() {
        let value = decode(b"d3:bin2:\xff\xfee").unwrap();
        assert_eq!(to_json(&value).to_string(), r#"{"bin":"fffe"}"#);
    }
}
