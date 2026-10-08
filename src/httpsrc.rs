//! A seekable `MediaSource` over HTTP Range requests, used to stream songs from
//! Simple Player Web Server (`/stream/<id>` supports Range).

use std::io::{self, Read, Seek, SeekFrom};

use reqwest::blocking::Client;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use symphonia::core::io::MediaSource;

const CHUNK: u64 = 512 * 1024;

pub struct HttpSource {
    client: Client,
    url: String,
    len: u64,
    pos: u64,
    buf: Vec<u8>,
    buf_start: u64,
}

impl HttpSource {
    pub fn open(client: Client, url: String) -> io::Result<HttpSource> {
        // Ask for the first chunk; the Content-Range header tells us the total size.
        let resp = client
            .get(&url)
            .header(RANGE, format!("bytes=0-{}", CHUNK - 1))
            .send()
            .map_err(io::Error::other)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(io::Error::other(format!("HTTP {status}")));
        }
        let total = resp
            .headers()
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
            .unwrap_or(0);
        let buf = resp.bytes().map_err(io::Error::other)?.to_vec();
        let len = if total > 0 { total } else { buf.len() as u64 };
        Ok(HttpSource {
            client,
            url,
            len,
            pos: 0,
            buf,
            buf_start: 0,
        })
    }

    fn fetch(&mut self, at: u64) -> io::Result<()> {
        let end = (at + CHUNK).min(self.len) - 1;
        let resp = self
            .client
            .get(&self.url)
            .header(RANGE, format!("bytes={at}-{end}"))
            .send()
            .map_err(io::Error::other)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(io::Error::other(format!("HTTP {status}")));
        }
        let partial = status.as_u16() == 206;
        let bytes = resp.bytes().map_err(io::Error::other)?;
        if partial {
            self.buf = bytes.to_vec();
            self.buf_start = at;
        } else {
            // Server ignored Range and sent the whole file.
            self.buf = bytes.to_vec();
            self.buf_start = 0;
            self.len = self.buf.len() as u64;
        }
        Ok(())
    }
}

impl Read for HttpSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let in_buf =
            self.pos >= self.buf_start && self.pos < self.buf_start + self.buf.len() as u64;
        if !in_buf {
            self.fetch(self.pos)?;
        }
        let off = (self.pos - self.buf_start) as usize;
        if off >= self.buf.len() {
            return Ok(0);
        }
        let n = out.len().min(self.buf.len() - off);
        out[..n].copy_from_slice(&self.buf[off..off + n]);
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

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}
