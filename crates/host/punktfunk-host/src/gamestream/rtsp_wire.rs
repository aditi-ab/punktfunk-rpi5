//! The text half of GameStream RTSP: one request off a byte buffer, its request line and
//! headers, the ANNOUNCE attribute map. `std` and `anyhow` only, with no host state, so a fuzz
//! target includes this file by path; every byte here arrives before any pairing check.
//! Sockets, sealing and what a request does are [`super::rtsp`]'s.

use anyhow::{Context, Result};
use std::collections::HashMap;

pub(super) const MAX_RTSP_HEADER: usize = 16 * 1024;
const MAX_RTSP_BODY: usize = 64 * 1024;

pub(super) struct Request {
    pub(super) method: String,
    pub(super) uri: String,
    pub(super) cseq: String,
    pub(super) head: String,
    pub(super) body: String,
}

/// One whole message off the front of `buf`: headers, then a `Content-Length` body. `None`
/// while more bytes are owed; the remainder stays in `buf`.
pub(super) fn take_message(buf: &mut Vec<u8>) -> Result<Option<Request>> {
    if let Some(end) = find_subslice(buf, b"\r\n\r\n") {
        // Cap even when the terminator is present: an oversized block with `\r\n\r\n`
        // would otherwise skip the no-terminator cap below.
        if end > MAX_RTSP_HEADER {
            anyhow::bail!("RTSP headers exceed limit");
        }
        let head = std::str::from_utf8(&buf[..end]).context("RTSP header utf8")?;
        let content_len = header_value(head, "content-length")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        // Bound Content-Length before buffering it (allocation amplification).
        if content_len > MAX_RTSP_BODY {
            anyhow::bail!("RTSP Content-Length {content_len} exceeds limit");
        }
        let total = end + 4 + content_len;
        if buf.len() >= total {
            let head = head.to_string();
            let body = String::from_utf8_lossy(&buf[end + 4..total]).into_owned();
            buf.drain(..total);
            return Ok(Some(parse_request(&head, body)));
        }
        // Headers complete; body still arriving.
    } else if buf.len() > MAX_RTSP_HEADER {
        // No terminator within the cap — dribble would grow forever.
        anyhow::bail!("RTSP headers exceed limit");
    }
    Ok(None)
}

pub(super) fn parse_request(head: &str, body: String) -> Request {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let uri = parts.next().unwrap_or("").to_string();
    let cseq = header_value(head, "cseq").unwrap_or("0").trim().to_string();
    Request {
        method,
        uri,
        cseq,
        head: head.to_string(),
        body,
    }
}

pub(super) fn parse_announce(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("a=") {
            if let Some((k, v)) = rest.split_once(':') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

/// Moonlight V+'s passthrough ask (`x-ml-audio.codec`, absent for Opus): `Ok(true)` for 16-bit
/// PCM, `Err` for a codec this host can't encode. Opus under another codec's label plays as
/// noise or silence, so ANNOUNCE refuses those.
pub(super) fn audio_passthrough(map: &HashMap<String, String>) -> Result<bool, String> {
    match map.get("x-ml-audio.codec").map(|s| s.trim()) {
        None | Some("opus") => Ok(false),
        Some("pcm" | "pcm_s16" | "s16") => Ok(true),
        Some(other) => Err(other.to_string()),
    }
}

/// SETUP URI `…/streamid=video/0/0` → `"video"` / `"audio"` / `"control"` / `"mic"`.
/// Only the text between `=` and `/` counts, as Sunshine reads it: moonlight-common-rust
/// sends `stream=control/13/0`.
pub(super) fn stream_type(uri: &str) -> Option<&str> {
    let after = uri.split_once('=')?.1;
    let token = after.split('/').next()?;
    match token {
        "audio" | "video" | "control" | "mic" => Some(token),
        _ => None,
    }
}

pub(super) fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub(super) fn header_value<'a>(head: &'a str, key_lower: &str) -> Option<&'a str> {
    head.split("\r\n").find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(key_lower)).then(|| v.trim_start())
    })
}
