//! HTTP(S) tracker announces.

use std::sync::OnceLock;
use std::time::Duration;

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use crate::util::urlencode_bytes;

use super::{AnnounceParams, AnnounceResponse, parse_compact_v4, parse_compact_v6};

const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(20);

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("npeer/", env!("CARGO_PKG_VERSION")))
            .timeout(ANNOUNCE_TIMEOUT)
            .build()
            .expect("default reqwest client")
    })
}

pub(super) async fn announce(url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let request_url = build_url(url, params);

    let response = client()
        .get(&request_url)
        .send()
        .await
        .map_err(|e| Error::Tracker(format!("{url}: {}", describe(&e))))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Tracker(format!("{url}: HTTP {status}")));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| Error::Tracker(format!("{url}: reading body: {}", describe(&e))))?;

    parse_response(&body).map_err(|e| Error::Tracker(format!("{url}: {e}")))
}

/// reqwest errors hide their cause behind `source()`; flatten the chain so
/// users see "connection refused" or "certificate expired", not just
/// "error sending request".
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// Build the announce URL. The info hash and peer id are raw bytes, so they
/// are percent-encoded byte by byte, exactly like the proven implementation.
fn build_url(base: &str, p: &AnnounceParams) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{base}{separator}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant={}&key={}",
        urlencode_bytes(p.info_hash.as_bytes()),
        urlencode_bytes(p.peer_id.as_bytes()),
        p.port,
        p.uploaded,
        p.downloaded,
        p.left,
        p.num_want,
        p.key,
    );
    if let Some(event) = p.event.as_query_value() {
        url.push_str("&event=");
        url.push_str(event);
    }
    url
}

fn parse_response(body: &[u8]) -> Result<AnnounceResponse> {
    let root = bencode::decode(body)
        .map_err(|_| Error::Tracker("response is not valid bencode".into()))?;

    if let Some(reason) = root.get(b"failure reason") {
        let reason = reason
            .as_bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_else(|| "unknown reason".into());
        return Err(Error::Tracker(reason));
    }

    let interval = root
        .get(b"interval")
        .and_then(Value::as_int)
        .filter(|&n| n > 0)
        .map(|n| Duration::from_secs(n as u64))
        .unwrap_or(Duration::from_secs(30 * 60));
    let min_interval = root
        .get(b"min interval")
        .and_then(Value::as_int)
        .filter(|&n| n > 0)
        .map(|n| Duration::from_secs(n as u64));

    let mut peers = Vec::new();
    match root.get(b"peers") {
        // Compact format (BEP 23): one long byte string.
        Some(Value::Bytes(bytes)) => peers.extend(parse_compact_v4(bytes)),
        // Original dictionary-model format.
        Some(Value::List(list)) => {
            for entry in list {
                let Some(ip) = entry.get(b"ip").and_then(Value::as_str) else {
                    continue;
                };
                let Some(port) = entry.get(b"port").and_then(Value::as_int) else {
                    continue;
                };
                let Ok(port) = u16::try_from(port) else {
                    continue;
                };
                if let Ok(ip) = ip.parse::<std::net::IpAddr>() {
                    peers.push(std::net::SocketAddr::from((ip, port)));
                }
            }
        }
        _ => {}
    }
    if let Some(Value::Bytes(bytes)) = root.get(b"peers6") {
        peers.extend(parse_compact_v6(bytes));
    }

    let seeders = root
        .get(b"complete")
        .and_then(Value::as_int)
        .and_then(|n| u32::try_from(n).ok());
    let leechers = root
        .get(b"incomplete")
        .and_then(Value::as_int)
        .and_then(|n| u32::try_from(n).ok());

    Ok(AnnounceResponse {
        interval,
        min_interval,
        peers,
        seeders,
        leechers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InfoHash, PeerId};

    fn params() -> AnnounceParams {
        AnnounceParams {
            info_hash: InfoHash([0xAB; 20]),
            peer_id: PeerId(*b"-NP0100-abcdefghijkl"),
            port: 6881,
            uploaded: 1,
            downloaded: 2,
            left: 3,
            event: super::super::AnnounceEvent::Started,
            num_want: 50,
            key: 7,
        }
    }

    #[test]
    fn url_contains_encoded_hash_and_event() {
        let url = build_url("http://t.example/announce", &params());
        assert!(url.starts_with("http://t.example/announce?info_hash=%ab%ab"));
        assert!(url.contains("&event=started"));
        assert!(url.contains("&compact=1"));
        assert!(url.contains("&left=3"));
    }

    #[test]
    fn url_appends_to_existing_query() {
        let url = build_url("http://t.example/announce?auth=x", &params());
        assert!(url.starts_with("http://t.example/announce?auth=x&info_hash="));
    }

    #[test]
    fn parses_compact_response() {
        let body =
            b"d8:completei5e10:incompletei2e8:intervali1800e5:peers6:\x7f\x00\x00\x01\x1a\xe1e";
        let parsed = parse_response(body).unwrap();
        assert_eq!(parsed.interval, Duration::from_secs(1800));
        assert_eq!(parsed.peers.len(), 1);
        assert_eq!(parsed.peers[0].to_string(), "127.0.0.1:6881");
        assert_eq!(parsed.seeders, Some(5));
        assert_eq!(parsed.leechers, Some(2));
    }

    #[test]
    fn parses_dict_model_response() {
        let body = b"d8:intervali60e5:peersld2:ip9:127.0.0.14:porti8080eeee";
        let parsed = parse_response(body).unwrap();
        assert_eq!(parsed.peers[0].to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn surfaces_failure_reason() {
        let body = b"d14:failure reason12:unregisterede";
        let err = parse_response(body).unwrap_err();
        assert!(err.to_string().contains("unregistered"));
    }
}
