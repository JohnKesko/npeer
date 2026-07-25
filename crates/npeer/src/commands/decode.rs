//! `npeer decode`: bencode-to-JSON, kept as a debugging aid from the
//! original implementation.

use std::io::Read;

use anyhow::{Context, Result};
use npeer_core::bencode;

pub fn run(value: Option<&str>) -> Result<()> {
    let bytes: Vec<u8> = match value {
        Some(text) => text.as_bytes().to_vec(),
        None => {
            let mut buffer = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buffer)
                .context("could not read stdin")?;
            buffer
        }
    };
    let trimmed = trim_ascii(&bytes);
    let decoded = bencode::decode(trimmed)?;
    let json = bencode::to_json(&decoded);
    println!("{}", serde_json::to_string(&json)?);
    Ok(())
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}
