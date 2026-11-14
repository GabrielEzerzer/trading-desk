//! A minimal HTTP/1.1 server on 127.0.0.1: one thread per connection, one request per connection.
//! Written by hand rather than on axum so a test can drop a connection before or after a request
//! is applied, or hold the answer past the client's timeout.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// A parsed request.
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    /// The raw query string, exactly as sent (it is what the client signed).
    pub query: String,
    /// Header names in lowercase.
    pub headers: Vec<(String, String)>,
    /// The raw body, exactly as sent.
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The decoded query parameters.
    pub fn params(&self) -> Vec<(String, String)> {
        self.query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (decode(k), decode(v))
            })
            .collect()
    }
}

/// What the server does with a request.
pub(crate) enum Reply {
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        /// Hold the answer this long before writing it.
        delay: Duration,
    },
    /// Close the connection without answering.
    Drop,
}

pub(crate) struct Server {
    pub port: u16,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(handler: Arc<dyn Fn(Request) -> Reply + Send + Sync>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let accept = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_flag.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let handler = handler.clone();
                std::thread::spawn(move || serve(stream, &*handler));
            }
        });
        Server {
            port,
            stop,
            accept: Some(accept),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn serve(stream: TcpStream, handler: &(dyn Fn(Request) -> Reply + Send + Sync)) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(request) = read_request(&stream) else {
        return;
    };
    match handler(request) {
        Reply::Drop => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        Reply::Respond {
            status,
            headers,
            body,
            delay,
        } => {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
            for (n, v) in headers {
                out.push_str(&format!("{n}: {v}\r\n"));
            }
            out.push_str(&format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ));
            let mut out = out.into_bytes();
            out.extend_from_slice(&body);
            let mut stream = stream;
            let _ = stream.write_all(&out);
            let _ = stream.flush();
        }
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (n, v) = h.split_once(':')?;
        headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let len: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body: String::from_utf8(body).ok()?,
    })
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match s.get(i + 1..i + 3).map(|h| u8::from_str_radix(h, 16)) {
                Some(Ok(b)) => {
                    out.push(b);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        403 => "Forbidden",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        _ => "Status",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_form_encoding() {
        assert_eq!(decode("a%2Db+c"), "a-b c");
        assert_eq!(decode("plain"), "plain");
        assert_eq!(decode("end%41"), "endA");
        assert_eq!(decode("bad%zz"), "bad%zz");
        assert_eq!(decode("short%4"), "short%4");
    }
}
