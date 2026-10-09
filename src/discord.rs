//! Discord Rich Presence through the local Discord app's IPC pipe (no login needed).
//! Same protocol as SimplePlayer-Discord-Presence (app/ipc.go).

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

trait Pipe: Read + Write + Send {}
impl<T: Read + Write + Send> Pipe for T {}

enum Cmd {
    Set(Option<Value>),
}

#[derive(Clone, Default)]
pub struct Status {
    pub connected: bool,
    pub user: String,
    pub error: String,
}

pub struct Discord {
    tx: Sender<Cmd>,
    status: Arc<Mutex<Status>>,
}

impl Discord {
    pub fn new(client_id: String) -> Discord {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let st = status.clone();
        thread::Builder::new()
            .name("discord".into())
            .spawn(move || run(rx, st, client_id))
            .expect("spawn discord thread");
        Discord { tx, status }
    }

    /// `None` clears the status.
    pub fn set_activity(&self, activity: Option<Value>) {
        let _ = self.tx.send(Cmd::Set(activity));
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

fn run(rx: Receiver<Cmd>, status: Arc<Mutex<Status>>, client_id: String) {
    let mut conn: Option<Box<dyn Pipe>> = None;
    let mut want: Option<Value> = None;
    let mut dirty = false;
    let mut nonce: u64 = 0;
    let set_status = |connected: bool, user: String, error: String| {
        if let Ok(mut s) = status.lock() {
            *s = Status {
                connected,
                user,
                error,
            };
        }
    };

    loop {
        let timeout = if dirty && conn.is_none() {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(3600)
        };
        match rx.recv_timeout(timeout) {
            Ok(cmd) => {
                let mut apply = |cmd: Cmd| match cmd {
                    Cmd::Set(a) => {
                        want = a;
                        dirty = true;
                    }
                };
                apply(cmd);
                while let Ok(more) = rx.try_recv() {
                    apply(more);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !dirty {
            continue;
        }
        if conn.is_none() {
            if want.is_none() {
                dirty = false;
                continue;
            }
            match connect(&client_id) {
                Ok((c, user)) => {
                    set_status(true, user, String::new());
                    conn = Some(c);
                }
                Err(e) => {
                    set_status(false, String::new(), e);
                    continue; // retry after the timeout
                }
            }
        }
        let Some(c) = conn.as_mut() else { continue };
        nonce += 1;
        let mut args = json!({ "pid": std::process::id() });
        if let Some(a) = &want {
            args["activity"] = a.clone();
        }
        let msg = json!({ "cmd": "SET_ACTIVITY", "args": args, "nonce": nonce.to_string() });
        let ok = write_frame(c.as_mut(), OP_FRAME, &msg).is_ok() && read_reply(c.as_mut()).is_ok();
        if ok {
            dirty = false;
        } else {
            conn = None;
            set_status(false, String::new(), crate::tr!("Discord 連線中斷，稍後重試", "Lost the Discord connection, retrying later"));
        }
    }
}

#[cfg(windows)]
fn open_pipe(i: u32) -> std::io::Result<Box<dyn Pipe>> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\discord-ipc-{i}"))?;
    Ok(Box::new(f))
}

#[cfg(unix)]
fn open_pipe(i: u32) -> std::io::Result<Box<dyn Pipe>> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let s = std::os::unix::net::UnixStream::connect(format!("{dir}/discord-ipc-{i}"))?;
    Ok(Box::new(s))
}

fn connect(client_id: &str) -> Result<(Box<dyn Pipe>, String), String> {
    let mut pipe = None;
    for i in 0..10 {
        if let Ok(p) = open_pipe(i) {
            pipe = Some(p);
            break;
        }
    }
    let mut p = pipe.ok_or_else(|| crate::tr!("找不到 Discord App（請確認 Discord 已開啟）", "Discord app not found (is Discord running?)"))?;
    write_frame(
        p.as_mut(),
        OP_HANDSHAKE,
        &json!({ "v": 1, "client_id": client_id }),
    )
    .map_err(|e| e.to_string())?;
    let (op, data) = read_frame(p.as_mut()).map_err(|e| e.to_string())?;
    if op == OP_CLOSE {
        return Err(crate::tr!("Discord 拒絕連線：{}", "Discord refused the connection: {}", data["message"]));
    }
    let user = data["data"]["user"]["global_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| data["data"]["user"]["username"].as_str())
        .unwrap_or("")
        .to_string();
    Ok((p, user))
}

fn read_reply(p: &mut dyn Pipe) -> std::io::Result<Value> {
    loop {
        let (op, data) = read_frame(p)?;
        match op {
            OP_PING => write_frame(p, OP_PONG, &data)?,
            OP_CLOSE => return Err(std::io::Error::other("closed by Discord")),
            OP_FRAME => return Ok(data),
            _ => {}
        }
    }
}

fn write_frame(p: &mut dyn Pipe, op: u32, v: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(v)?;
    let mut buf = Vec::with_capacity(8 + body.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    p.write_all(&buf)?;
    p.flush()
}

fn read_frame(p: &mut dyn Pipe) -> std::io::Result<(u32, Value)> {
    let mut head = [0u8; 8];
    p.read_exact(&mut head)?;
    let op = u32::from_le_bytes(head[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
    if len > 1 << 20 {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut body = vec![0u8; len];
    p.read_exact(&mut body)?;
    Ok((op, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/* ---------------- activity ---------------- */

pub struct NowPlaying<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    /// Public https cover / share links (empty when the server is LAN-only).
    pub cover_url: String,
    pub share_url: String,
}

fn clip(s: &str, fallback: &str) -> String {
    let mut s = if s.trim().is_empty() {
        fallback.to_string()
    } else {
        s.trim().to_string()
    };
    if s.chars().count() < 2 {
        s.push(' ');
    }
    if s.chars().count() > 128 {
        s = s.chars().take(127).collect::<String>() + "…";
    }
    s
}

pub fn activity(np: &NowPlaying) -> Value {
    let base_state = if !np.artist.trim().is_empty() {
        np.artist
    } else {
        np.album
    };
    let state = if np.paused {
        format!(
            "{} · ⏸",
            if base_state.trim().is_empty() {
                "Simple Player"
            } else {
                base_state
            }
        )
    } else {
        base_state.to_string()
    };
    let mut a = json!({
        "type": 2,
        "details": clip(np.title, "Unknown"),
        "state": clip(&state, "Simple Player"),
        "instance": false,
    });
    if np.duration > 0.0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let start = now - (np.position * 1000.0) as i64;
        a["timestamps"] = json!({ "start": start, "end": start + (np.duration * 1000.0) as i64 });
    }
    if !np.cover_url.is_empty() {
        a["assets"] =
            json!({ "large_image": np.cover_url, "large_text": clip(np.album, "Simple Player") });
    }
    if !np.share_url.is_empty() {
        a["buttons"] = json!([{ "label": crate::tr!("在 Simple Player 收聽", "Listen on Simple Player"), "url": np.share_url }]);
    }
    a
}
