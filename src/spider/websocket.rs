//! WebSocket (RFC 6455), as much as a Nostr client needs: a connection to a
//! relay (`ws://` or `wss://`, straight or through a SOCKS5 proxy) and text
//! messages both ways. No extensions (so no compression) and no
//! subprotocols; pings are answered.

use std::io::{ErrorKind, Read, Write};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lost::urls::Url;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};

use crate::net::{self, Proxy, Route};

/// A message longer than this ends the connection: no Nostr event is that
/// big (relays refuse them far sooner).
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
const MAX_HEADERS: usize = 16 * 1024;

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

pub struct WebSocket {
    stream: Box<dyn Stream>,
    /// Read, not yet taken.
    buffer: Vec<u8>,
}

impl WebSocket {
    /// Connects to `url` and asks for a WebSocket. `tls` is for `wss://`.
    pub fn connect(
        url: &Url,
        proxy: Option<&Proxy>,
        timeout: Duration,
        tls: Option<&Arc<ClientConfig>>,
    ) -> Result<WebSocket, String> {
        let secure = match url.scheme.as_str() {
            "wss" => true,
            "ws" => false,
            other => return Err(format!("{other}:// isn't a WebSocket")),
        };
        let host = url.host.trim_start_matches('[').trim_end_matches(']');
        let port = url.port.unwrap_or(if secure { 443 } else { 80 });
        let route = match proxy {
            Some(Proxy::Socks5(_)) => Route::Proxied,
            Some(Proxy::Http(_)) => {
                return Err("a relay can't be reached through an HTTP proxy".into());
            }
            None => Route::Direct(
                (host, port)
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut a| a.next())
                    .ok_or_else(|| format!("can't find {host}"))?,
            ),
        };
        let tcp = net::connect(route, proxy, host, port, timeout)?;
        let stream: Box<dyn Stream> = if secure {
            let tls = tls.ok_or("no TLS for wss://")?;
            let name = ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
            let connection = ClientConnection::new(tls.clone(), name).map_err(|e| e.to_string())?;
            Box::new(StreamOwned::new(connection, tcp))
        } else {
            Box::new(tcp)
        };
        let mut socket = WebSocket {
            stream,
            buffer: Vec::new(),
        };
        socket.handshake(url, Instant::now() + timeout * 2)?;
        Ok(socket)
    }

    /// A WebSocket over `stream`, already open (for tests).
    #[cfg(test)]
    fn over(stream: impl Read + Write + Send + 'static) -> WebSocket {
        WebSocket {
            stream: Box::new(stream),
            buffer: Vec::new(),
        }
    }

    fn handshake(&mut self, url: &Url, deadline: Instant) -> Result<(), String> {
        let mut key = [0u8; 16];
        getrandom::fill(&mut key).map_err(|e| e.to_string())?;
        let host = match url.port {
            Some(port) => format!("{}:{port}", url.host),
            None => url.host.clone(),
        };
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\
             User-Agent: lost-spider/{}\r\n\r\n",
            url.path,
            base64(&key),
            env!("CARGO_PKG_VERSION"),
        );
        self.stream
            .write_all(request.as_bytes())
            .and_then(|()| self.stream.flush())
            .map_err(|e| e.to_string())?;
        let end = loop {
            if let Some(i) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if self.buffer.len() > MAX_HEADERS {
                return Err("an answer that isn't a WebSocket's".into());
            }
            self.read_more(deadline)?;
        };
        let head = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
        self.buffer.drain(..end);
        let status = head.split_whitespace().nth(1).unwrap_or("");
        if status != "101" {
            return Err(match status {
                "" => "an answer that isn't HTTP".to_string(),
                status => format!("not a relay (HTTP {status})"),
            });
        }
        Ok(())
    }

    /// Sends a text message.
    pub fn send(&mut self, text: &str) -> Result<(), String> {
        self.send_frame(0x1, text.as_bytes())
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        // Every frame a client sends is masked.
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n <= 0xffff => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mut mask = [0u8; 4];
        getrandom::fill(&mut mask).map_err(|e| e.to_string())?;
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream
            .write_all(&frame)
            .and_then(|()| self.stream.flush())
            .map_err(|e| e.to_string())
    }

    /// The next text message, by `deadline`. Pings are answered on the way;
    /// the relay closing the connection is an error.
    pub fn receive(&mut self, deadline: Instant) -> Result<String, String> {
        let mut message: Vec<u8> = Vec::new();
        let mut in_message = false;
        loop {
            let (fin, opcode, payload) = self.frame(deadline)?;
            match opcode {
                0x0 if in_message => message.extend_from_slice(&payload),
                0x1 | 0x2 if !in_message => {
                    message = payload;
                    in_message = true;
                }
                0x8 => return Err("closed by the relay".into()),
                0x9 => {
                    self.send_frame(0xA, &payload)?;
                    continue;
                }
                0xA => continue,
                _ => return Err("a WebSocket frame that makes no sense".into()),
            }
            if message.len() > MAX_MESSAGE {
                return Err("a message too long".into());
            }
            if fin {
                return String::from_utf8(message).map_err(|_| "a message that isn't text".into());
            }
        }
    }

    /// One frame: whether it's the message's last, its opcode, its payload.
    fn frame(&mut self, deadline: Instant) -> Result<(bool, u8, Vec<u8>), String> {
        self.fill(2, deadline)?;
        let (first, second) = (self.buffer[0], self.buffer[1]);
        let masked = second & 0x80 != 0;
        let (length, mut at) = match second & 0x7f {
            126 => {
                self.fill(4, deadline)?;
                (
                    u64::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]])),
                    4,
                )
            }
            127 => {
                self.fill(10, deadline)?;
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&self.buffer[2..10]);
                (u64::from_be_bytes(bytes), 10)
            }
            n => (u64::from(n), 2),
        };
        if length > MAX_MESSAGE as u64 {
            return Err("a message too long".into());
        }
        let length = length as usize;
        let mut mask = None;
        if masked {
            self.fill(at + 4, deadline)?;
            mask = Some([
                self.buffer[at],
                self.buffer[at + 1],
                self.buffer[at + 2],
                self.buffer[at + 3],
            ]);
            at += 4;
        }
        self.fill(at + length, deadline)?;
        let mut payload: Vec<u8> = self.buffer.drain(..at + length).skip(at).collect();
        if let Some(mask) = mask {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        Ok((first & 0x80 != 0, first & 0x0f, payload))
    }

    /// Reads until at least `n` bytes are waiting.
    fn fill(&mut self, n: usize, deadline: Instant) -> Result<(), String> {
        while self.buffer.len() < n {
            self.read_more(deadline)?;
        }
        Ok(())
    }

    fn read_more(&mut self, deadline: Instant) -> Result<(), String> {
        if Instant::now() >= deadline {
            return Err("timed out".into());
        }
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("the relay hung up".into()),
                Ok(n) => {
                    self.buffer.extend_from_slice(&chunk[..n]);
                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Err("timed out".into());
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// A connection let go says goodbye (whatever happens then doesn't matter).
impl Drop for WebSocket {
    fn drop(&mut self) {
        let _ = self.send_frame(0x8, &1000u16.to_be_bytes());
    }
}

/// Base64, standard alphabet, padded (for Sec-WebSocket-Key).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn base64_of_keys() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // RFC 6455's example key.
        assert_eq!(base64(b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
    }

    /// A server's frame: unmasked, or masked (which servers mustn't do,
    /// but it's read anyway).
    fn frame(fin: bool, opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
        let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
        let bit = if mask.is_some() { 0x80 } else { 0 };
        match payload.len() {
            n if n < 126 => out.push(bit | n as u8),
            n if n <= 0xffff => {
                out.push(bit | 126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(bit | 127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        match mask {
            Some(mask) => {
                out.extend_from_slice(&mask);
                out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            }
            None => out.extend_from_slice(payload),
        }
        out
    }

    /// Reads one masked client frame: its opcode and payload.
    fn client_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).unwrap();
        assert!(head[1] & 0x80 != 0, "client frames are masked");
        let length = match head[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                stream.read_exact(&mut b).unwrap();
                usize::from(u16::from_be_bytes(b))
            }
            127 => {
                let mut b = [0u8; 8];
                stream.read_exact(&mut b).unwrap();
                u64::from_be_bytes(b) as usize
            }
            n => usize::from(n),
        };
        let mut mask = [0u8; 4];
        stream.read_exact(&mut mask).unwrap();
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload).unwrap();
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
        (head[0] & 0x0f, payload)
    }

    #[test]
    fn talking_to_a_relay() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let long = "x".repeat(70_000);
        let long_copy = long.clone();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            // The answer comes with the first frame right behind it.
            let mut answer = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                               Connection: Upgrade\r\n\r\n"
                .to_vec();
            answer.extend(frame(true, 0x1, b"[\"NOTICE\",\"hi\"]", None));
            s.write_all(&answer).unwrap();
            let (opcode, text) = client_frame(&mut s);
            assert_eq!(
                (opcode, text.as_slice()),
                (0x1, b"[\"REQ\",\"a\",{}]".as_slice())
            );
            // A ping in the middle of a message in two parts, then a long
            // one, masked.
            s.write_all(&frame(false, 0x1, b"[\"EOSE\",", None))
                .unwrap();
            s.write_all(&frame(true, 0x9, b"ping!", None)).unwrap();
            s.write_all(&frame(true, 0x0, b"\"a\"]", None)).unwrap();
            let (opcode, payload) = client_frame(&mut s);
            assert_eq!((opcode, payload.as_slice()), (0xA, b"ping!".as_slice()));
            s.write_all(&frame(true, 0x1, long_copy.as_bytes(), Some([1, 2, 3, 4])))
                .unwrap();
            s.write_all(&frame(true, 0x8, &1000u16.to_be_bytes(), None))
                .unwrap();
            request
        });
        let url = Url::parse(&format!("ws://127.0.0.1:{port}/relay")).unwrap();
        let mut socket = WebSocket::connect(&url, None, Duration::from_secs(5), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(socket.receive(deadline).unwrap(), "[\"NOTICE\",\"hi\"]");
        socket.send("[\"REQ\",\"a\",{}]").unwrap();
        assert_eq!(socket.receive(deadline).unwrap(), "[\"EOSE\",\"a\"]");
        assert_eq!(socket.receive(deadline).unwrap(), long);
        assert_eq!(
            socket.receive(deadline),
            Err("closed by the relay".to_string())
        );
        let request = server.join().unwrap();
        assert!(request.starts_with("GET /relay HTTP/1.1\r\n"), "{request}");
        assert!(request.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")));
        assert!(request.contains("\r\nUpgrade: websocket\r\n"));
        assert!(request.contains("\r\nSec-WebSocket-Version: 13\r\n"));
        let key = request
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap();
        assert_eq!(key.len(), 24);
    }

    #[test]
    fn not_a_relay() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).unwrap();
            s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let url = Url::parse(&format!("ws://127.0.0.1:{port}/")).unwrap();
        let error = WebSocket::connect(&url, None, Duration::from_secs(5), None)
            .err()
            .unwrap();
        assert_eq!(error, "not a relay (HTTP 404)");
        server.join().unwrap();
        // And the other end of a frame stream: a frame too long is refused
        // before it's read.
        let mut huge = vec![0x81, 127];
        huge.extend_from_slice(&(u64::MAX).to_be_bytes());
        let mut socket = WebSocket::over(std::io::Cursor::new(huge));
        assert_eq!(
            socket.receive(Instant::now() + Duration::from_secs(1)),
            Err("a message too long".to_string())
        );
    }
}
