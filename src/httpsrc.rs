//! A seekable `MediaSource` over HTTP Range requests, used to stream songs from
//! Simple Player Web Server (`/stream/<id>` supports Range).
//!
//! A background thread keeps downloading a few chunks ahead of the read position, so the
//! decoder never waits on the network (a 0.5–1 s request used to stall playback).

use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use symphonia::core::io::MediaSource;

/// Each request fetches this much. Requests can take ~0.6 s each (reverse proxy, Wi-Fi…),
/// so big chunks keep the throughput well above even 24-bit/192 kHz FLAC.
const CHUNK: u64 = 1024 * 1024;
/// Chunks kept downloaded ahead of the read position (6 MB ≈ 15 s of 24/96 FLAC).
const AHEAD: u64 = 6;

struct State {
    len: u64,
    /// Chunk index → bytes.
    chunks: BTreeMap<u64, Arc<Vec<u8>>>,
    /// Chunk the reader is in (the downloader works forward from here).
    reading: u64,
    error: Option<String>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

pub struct HttpSource {
    shared: Arc<Shared>,
    len: u64,
    pos: u64,
}

fn total_len(resp: &reqwest::blocking::Response) -> Option<u64> {
    resp.headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| {
            resp.headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        })
}

impl HttpSource {
    pub fn open(client: Client, url: String) -> io::Result<HttpSource> {
        // First chunk; Content-Range tells the total size.
        let resp = client
            .get(&url)
            .header(RANGE, format!("bytes=0-{}", CHUNK - 1))
            .send()
            .map_err(io::Error::other)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(io::Error::other(format!("HTTP {status}")));
        }
        let partial = status.as_u16() == 206;
        let total = total_len(&resp);
        let body = resp.bytes().map_err(io::Error::other)?.to_vec();
        let mut chunks = BTreeMap::new();
        let len;
        if partial {
            len = total.unwrap_or(body.len() as u64);
            chunks.insert(0, Arc::new(body));
        } else {
            // Server ignored Range and sent the whole file: keep it all.
            len = body.len() as u64;
            for (i, c) in body.chunks(CHUNK as usize).enumerate() {
                chunks.insert(i as u64, Arc::new(c.to_vec()));
            }
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                len,
                chunks,
                reading: 0,
                error: None,
                stop: false,
            }),
            cv: Condvar::new(),
        });
        if partial {
            let s = shared.clone();
            thread::Builder::new()
                .name("http-prefetch".into())
                .spawn(move || downloader(s, client, url))
                .map_err(io::Error::other)?;
        }
        Ok(HttpSource {
            shared,
            len,
            pos: 0,
        })
    }
}

/// Fetches the first missing chunk from the reader's chunk onwards; idles when far enough ahead.
fn downloader(shared: Arc<Shared>, client: Client, url: String) {
    let last_chunk = |len: u64| len.saturating_sub(1) / CHUNK;
    loop {
        let (want, len) = {
            let mut st = shared.state.lock().unwrap();
            loop {
                if st.stop {
                    return;
                }
                let first = st.reading;
                let last = (first + AHEAD).min(last_chunk(st.len));
                // drop what is far behind or far ahead (after a seek)
                let keep_from = first.saturating_sub(1);
                let keep_to = last + 2;
                st.chunks.retain(|k, _| *k >= keep_from && *k <= keep_to);
                if let Some(c) = (first..=last).find(|c| !st.chunks.contains_key(c)) {
                    break (c, st.len);
                }
                st = shared
                    .cv
                    .wait_timeout(st, Duration::from_millis(500))
                    .unwrap()
                    .0;
            }
        };
        let start = want * CHUNK;
        let end = ((want + 1) * CHUNK).min(len) - 1;
        let mut result = Err(String::new());
        for attempt in 0..3 {
            match client
                .get(&url)
                .header(RANGE, format!("bytes={start}-{end}"))
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.bytes())
            {
                Ok(b) => {
                    result = Ok(b.to_vec());
                    break;
                }
                Err(e) => {
                    result = Err(e.to_string());
                    thread::sleep(Duration::from_millis(300 * (attempt + 1)));
                }
            }
        }
        let mut st = shared.state.lock().unwrap();
        match result {
            Ok(bytes) => {
                st.chunks.insert(want, Arc::new(bytes));
            }
            Err(e) => st.error = Some(e),
        }
        shared.cv.notify_all();
    }
}

impl Read for HttpSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let idx = self.pos / CHUNK;
        let chunk = {
            let mut st = self.shared.state.lock().unwrap();
            if st.reading != idx {
                st.reading = idx;
                self.shared.cv.notify_all();
            }
            loop {
                if let Some(c) = st.chunks.get(&idx) {
                    break c.clone();
                }
                if let Some(e) = st.error.take() {
                    return Err(io::Error::other(e));
                }
                st = self
                    .shared
                    .cv
                    .wait_timeout(st, Duration::from_millis(200))
                    .unwrap()
                    .0;
            }
        };
        let off = (self.pos - idx * CHUNK) as usize;
        if off >= chunk.len() {
            return Ok(0);
        }
        let n = out.len().min(chunk.len() - off);
        out[..n].copy_from_slice(&chunk[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => self.len as i64 + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if new < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

impl Drop for HttpSource {
    fn drop(&mut self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.stop = true;
        }
        self.shared.cv.notify_all();
    }
}

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}
