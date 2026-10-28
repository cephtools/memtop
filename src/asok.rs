//! Ceph admin socket client: send one JSON command, read the length-prefixed reply.
//!
//! Wire format (src/common/admin_socket.cc): the request is the command text followed by a NUL byte;
//! the reply is a 4-byte big-endian length and that many bytes of payload. Plain-text commands are
//! wrapped into {"prefix": ...} by old releases but rejected as "invalid json" by new ones, so every
//! command here is sent as JSON.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Debug)]
pub struct AsokError(pub String);

impl std::fmt::Display for AsokError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Run `cmd` (a JSON object with "prefix") against the socket at `path` and return the raw reply.
pub fn call(path: &str, cmd: &str, timeout: Duration) -> Result<String, AsokError> {
    let mut s = UnixStream::connect(path).map_err(|e| AsokError(format!("connect: {}", e)))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    s.write_all(cmd.as_bytes())
        .and_then(|_| s.write_all(b"\0"))
        .map_err(|e| AsokError(format!("send: {}", e)))?;
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr)
        .map_err(|e| AsokError(format!("reply header: {}", e)))?;
    let n = u32::from_be_bytes(hdr) as usize;
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf)
        .map_err(|e| AsokError(format!("reply body: {}", e)))?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if text.starts_with("ERROR:") {
        return Err(AsokError(text.lines().next().unwrap_or("").to_string()));
    }
    Ok(text)
}

/// Like `call`, parsed as JSON.
pub fn call_json(path: &str, cmd: &str, timeout: Duration) -> Result<serde_json::Value, AsokError> {
    let text = call(path, cmd, timeout)?;
    serde_json::from_str(&text)
        .map_err(|e| AsokError(format!("not json: {} ({})", e, &text[..text.len().min(60)])))
}

/// `config set` wants its value as a list ("val" is declared n=N in the command description).
pub fn cmd_set(var: &str, val: &str) -> String {
    serde_json::json!({"prefix": "config set", "var": var, "val": [val]}).to_string()
}

/// `{"prefix": "<prefix>"}` plus optional extra fields, as the socket wants it.
pub fn cmd(prefix: &str, extra: &[(&str, &str)]) -> String {
    let mut m = serde_json::Map::new();
    m.insert("prefix".into(), serde_json::Value::String(prefix.into()));
    for (k, v) in extra {
        m.insert((*k).into(), serde_json::Value::String((*v).into()));
    }
    serde_json::Value::Object(m).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn roundtrip_against_a_fake_socket() {
        let dir = std::env::temp_dir().join(format!("memtop-asok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("osd.0.asok");
        let l = UnixListener::bind(&path).unwrap();
        let p = path.to_string_lossy().into_owned();
        let srv = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut req = Vec::new();
            let mut b = [0u8; 1];
            loop {
                c.read_exact(&mut b).unwrap();
                if b[0] == 0 {
                    break;
                }
                req.push(b[0]);
            }
            assert_eq!(String::from_utf8(req).unwrap(), r#"{"prefix":"version"}"#);
            let reply = br#"{"version":"19.2.0"}"#;
            c.write_all(&(reply.len() as u32).to_be_bytes()).unwrap();
            c.write_all(reply).unwrap();
        });
        let v = call_json(&p, &cmd("version", &[]), Duration::from_secs(2)).unwrap();
        assert_eq!(v["version"], "19.2.0");
        srv.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn error_replies_are_errors() {
        assert!(call(
            "/nonexistent/x.asok",
            &cmd("version", &[]),
            Duration::from_millis(100)
        )
        .is_err());
    }
}
