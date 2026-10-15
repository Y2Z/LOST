//! Connecting: straight to a server, or through a proxy (`--proxy`): SOCKS5
//! (Tor's, or I2P's) for any protocol, or an HTTP proxy (I2P's) for HTTP.
//! Through a proxy, host names are never looked up here: the proxy does it,
//! so nothing about the crawl reaches the local DNS.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

#[derive(Debug, Clone, PartialEq)]
pub enum Proxy {
    /// `socks5://HOST:PORT`: any connection, names resolved by the proxy.
    Socks5(String),
    /// `http://HOST:PORT`: HTTP requests, sent to the proxy whole.
    Http(String),
}

impl Proxy {
    pub fn parse(text: &str) -> Result<Proxy, String> {
        let text = text.trim().trim_end_matches('/');
        let (scheme, address) = text.split_once("://").ok_or_else(|| {
            format!("--proxy takes socks5://HOST:PORT or http://HOST:PORT (not \"{text}\")")
        })?;
        if address.is_empty() || !address.contains(':') {
            return Err(format!("--proxy needs a port: {scheme}://HOST:PORT"));
        }
        match scheme.to_ascii_lowercase().as_str() {
            "socks5" | "socks5h" => Ok(Proxy::Socks5(address.to_string())),
            "http" => Ok(Proxy::Http(address.to_string())),
            other => Err(format!(
                "--proxy can be socks5:// or http://, not {other}://"
            )),
        }
    }

    pub fn address(&self) -> &str {
        match self {
            Proxy::Socks5(a) | Proxy::Http(a) => a,
        }
    }

    /// Fails with a message for people if nothing answers at the proxy's
    /// address (Tor or I2P not running yet, say).
    pub fn check(&self) -> Result<(), String> {
        let addr = self.socket_addr()?;
        TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map(drop)
            .map_err(|e| format!("can't reach the proxy at {}: {e}", self.address()))
    }

    fn socket_addr(&self) -> Result<SocketAddr, String> {
        self.address()
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .ok_or_else(|| format!("can't find the proxy {}", self.address()))
    }
}

/// How a host's server is reached.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Route {
    /// At this address, looked up here.
    Direct(SocketAddr),
    /// Through the proxy, by name.
    Proxied,
}

/// A connection to `host:port`, with read and write timeouts set. Through
/// a SOCKS5 proxy it's a tunnel to the server; through an HTTP proxy, a
/// connection to the proxy itself, to send it whole requests.
pub fn connect(
    route: Route,
    proxy: Option<&Proxy>,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, String> {
    let addr = match (route, proxy) {
        (Route::Direct(addr), _) => addr,
        (Route::Proxied, Some(proxy)) => proxy.socket_addr()?,
        (Route::Proxied, None) => return Err("no proxy to go through".into()),
    };
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| e.to_string())?;
    if let (Route::Proxied, Some(Proxy::Socks5(_))) = (route, proxy) {
        socks5(&mut stream, host, port)?;
    }
    Ok(stream)
}

/// SOCKS5 (RFC 1928), no authentication, connecting by name.
fn socks5(stream: &mut TcpStream, host: &str, port: u16) -> Result<(), String> {
    let io = |e: std::io::Error| format!("proxy: {e}");
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.len() > 255 {
        return Err("host name too long".into());
    }
    stream.write_all(&[5, 1, 0]).map_err(io)?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).map_err(io)?;
    if reply != [5, 0] {
        return Err("the proxy wants a login, or isn't a SOCKS5 proxy".into());
    }
    let mut request = vec![5, 1, 0, 3, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).map_err(io)?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).map_err(io)?;
    if head[1] != 0 {
        return Err(match head[1] {
            1 => "proxy: general failure",
            2 => "proxy: not allowed",
            3 => "proxy: network unreachable",
            4 => "proxy: host unreachable",
            5 => "proxy: connection refused",
            6 => "proxy: timed out",
            _ => "proxy: couldn't connect",
        }
        .into());
    }
    // The address the proxy connected from: skipped.
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).map_err(io)?;
            usize::from(len[0])
        }
        _ => return Err("proxy: a reply that makes no sense".into()),
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).map_err(io)?;
    Ok(())
}

/// TLS that accepts any certificate: Gemini capsules sign their own, and a
/// `.onion` address already proves which server it is. The spider only
/// reads public pages, so there's nothing a certificate would protect. The
/// handshake's signatures are still checked, as TLS needs.
pub fn any_certificate() -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("can't set up TLS: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxies() {
        assert_eq!(
            Proxy::parse("socks5://127.0.0.1:9050"),
            Ok(Proxy::Socks5("127.0.0.1:9050".into()))
        );
        assert_eq!(
            Proxy::parse("socks5h://localhost:9050/"),
            Ok(Proxy::Socks5("localhost:9050".into()))
        );
        assert_eq!(
            Proxy::parse("http://127.0.0.1:4444"),
            Ok(Proxy::Http("127.0.0.1:4444".into()))
        );
        assert!(Proxy::parse("127.0.0.1:9050").is_err());
        assert!(Proxy::parse("socks5://localhost").is_err());
        assert!(Proxy::parse("ftp://x:1").is_err());
    }

    #[test]
    fn through_socks5() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = Proxy::Socks5(listener.local_addr().unwrap().to_string());
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut hello = [0u8; 3];
            s.read_exact(&mut hello).unwrap();
            assert_eq!(hello, [5, 1, 0]);
            s.write_all(&[5, 0]).unwrap();
            let mut head = [0u8; 5];
            s.read_exact(&mut head).unwrap();
            assert_eq!(head[..4], [5, 1, 0, 3]);
            let mut name = vec![0u8; usize::from(head[4]) + 2];
            s.read_exact(&mut name).unwrap();
            s.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 70]).unwrap();
            s.write_all(b"hello through the proxy").unwrap();
            String::from_utf8_lossy(&name[..name.len() - 2]).into_owned()
        });
        let mut stream = connect(
            Route::Proxied,
            Some(&proxy),
            "example.onion",
            70,
            Duration::from_secs(5),
        )
        .unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello through the proxy");
        assert_eq!(server.join().unwrap(), "example.onion");
    }
}
