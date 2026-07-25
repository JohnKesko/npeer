//! Small helpers with no better home.

use rand::Rng;

/// Percent-encode raw bytes for tracker query strings.
///
/// Trackers require the raw 20-byte info hash and peer id in the URL, so every
/// byte is escaped rather than relying on a text-oriented urlencoder.
pub fn urlencode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        out.push('%');
        out.push_str(&hex::encode([*byte]));
    }
    out
}

/// Random alphanumeric ASCII string, used for peer id suffixes.
pub fn random_alnum(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// Decode RFC 4648 base32 (no padding), as used by older magnet links.
pub fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut bits: u64 = 0;
    let mut bit_count = 0;
    let mut out = Vec::with_capacity(input.len() * 5 / 8);

    for ch in input.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a',
            b'2'..=b'7' => ch - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(value);
        bit_count += 5;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_every_byte() {
        assert_eq!(urlencode_bytes(&[0x00, 0xff, b'a']), "%00%ff%61");
    }

    #[test]
    fn base32_decodes_reference_vector() {
        // RFC 4648: "MZXW6YTBOI" -> "foobar"
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw6ytboi").unwrap(), b"foobar");
        assert!(base32_decode("not base32!").is_none());
    }

    #[test]
    fn random_alnum_length() {
        assert_eq!(random_alnum(12).len(), 12);
    }
}
