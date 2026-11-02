//! Minimal blocking HTTP/1.1 client for the soil nodes. They serve a few hundred bytes
//! of JSON over plain HTTP on the LAN, so one TCP connection per poll and no client
//! library keeps the binary and the per-poll cost small.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use plant_icd::node;

const MAX_RESPONSE: usize = 64 * 1024;

pub fn get_status(addr: &str, timeout: Duration) -> io::Result<node::Status> {
    let body = request(addr, "GET", node::STATUS_PATH, "", timeout)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn set_sample_interval(addr: &str, seconds: u32, timeout: Duration) -> io::Result<()> {
    let body = format!(r#"{{"sample_interval_s":{seconds}}}"#);
    request(addr, "PUT", node::CONFIG_PATH, &body, timeout).map(drop)
}

/// Sends one request and returns the body of a 2xx response.
fn request(
    addr: &str,
    method: &str,
    path: &str,
    body: &str,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))?;
    let mut stream = TcpStream::connect_timeout(&sock, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes())?;

    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    let head_len = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_RESPONSE {
            return Err(invalid("headers too large"));
        }
        match stream.read(&mut chunk)? {
            0 => return Err(invalid("connection closed before headers")),
            n => buf.extend_from_slice(&chunk[..n]),
        }
    };

    let head = std::str::from_utf8(&buf[..head_len]).map_err(|_| invalid("non-UTF-8 headers"))?;
    let mut lines = head.split("\r\n");
    let code: u16 = lines
        .next()
        .and_then(|l| l.split_ascii_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| invalid("bad status line"))?;
    let (mut content_len, mut chunked) = (None, false);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k.eq_ignore_ascii_case("content-length") {
            content_len = v.parse::<usize>().ok();
        } else if k.eq_ignore_ascii_case("transfer-encoding") {
            chunked = v.eq_ignore_ascii_case("chunked");
        }
    }

    let mut body = buf.split_off(head_len);
    let done = |b: &[u8]| match (chunked, content_len) {
        (true, _) => b.ends_with(b"0\r\n\r\n"),
        (false, Some(n)) => b.len() >= n,
        (false, None) => false, // read to EOF
    };
    while !done(&body) {
        if body.len() > MAX_RESPONSE {
            return Err(invalid("response too large"));
        }
        match stream.read(&mut chunk)? {
            0 if chunked || content_len.is_some() => return Err(invalid("truncated body")),
            0 => break,
            n => body.extend_from_slice(&chunk[..n]),
        }
    }
    if let Some(n) = content_len.filter(|_| !chunked) {
        body.truncate(n);
    }
    if chunked {
        body = dechunk(&body).ok_or_else(|| invalid("bad chunked encoding"))?;
    }

    if !(200..300).contains(&code) {
        let msg = String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned();
        return Err(io::Error::other(format!("HTTP {code}: {msg}")));
    }
    Ok(body)
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(b.len());
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&b[..eol])
            .ok()?
            .split(';')
            .next()?
            .trim();
        let size = usize::from_str_radix(size_str, 16).ok()?;
        b = &b[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(..size)?);
        b = b.get(size + 2..)?;
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dechunks() {
        assert_eq!(
            dechunk(b"4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n").unwrap(),
            b"Wikipedia"
        );
    }
}
