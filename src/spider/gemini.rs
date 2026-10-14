//! Gemini (gemini://geminiprotocol.net/docs/protocol-specification.gmi):
//! one request a connection, over TLS, on port 1965. A request is the URL
//! and CRLF; a response is a two-digit status, a space, a "meta" line, CRLF
//! and, for status 2x, the document.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lost::urls::{Url, resolve_any};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

use crate::crawl::{Link, Parsed, Protocol, Response, mentions};

/// Where a crawl starts, besides `--link`: home pages, the lists of known
/// capsules Gemini's search engines keep, a bulletin board and an
/// aggregator of new posts. A dead one costs one failed request.
const SEEDS: &[&str] = &[
    "gemini://geminiprotocol.net/",
    "gemini://kennedy.gemi.dev/observatory/known-hosts",
    "gemini://tlgs.one/known-hosts",
    "gemini://bbs.geminispace.org/",
    "gemini://warmedal.se/~antenna/",
    "gemini://mozz.us/",
];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// For each read and write once connected.
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// For a whole response.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
/// Longer documents are cut here.
const MAX_BODY: usize = 5 * 1024 * 1024;
/// A status line is at most `NN `, 1024 bytes of meta and CRLF.
const MAX_HEADER: usize = 1029;

pub struct Gemini {
    tls: Arc<ClientConfig>,
}

impl Gemini {
    pub fn new() -> lost::Result<Gemini> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("can't set up TLS: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth();
        Ok(Gemini { tls: Arc::new(tls) })
    }
}

impl Protocol for Gemini {
    const SCHEME: &'static str = "gemini";
    const SPACE: &'static str = "Geminispace";
    const HOSTS: &'static str = "capsules";

    fn port(&self) -> u16 {
        1965
    }

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn fetch(&self, addr: SocketAddr, url: &Url) -> Result<Response, String> {
        let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| e.to_string())?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|()| tcp.set_write_timeout(Some(IO_TIMEOUT)))
            .map_err(|e| e.to_string())?;
        // Gemini wants the host name sent (SNI); an IPv6 literal goes bare.
        let host = url.host.trim_start_matches('[').trim_end_matches(']');
        let name = ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
        let connection =
            ClientConnection::new(self.tls.clone(), name).map_err(|e| e.to_string())?;
        let mut stream = StreamOwned::new(connection, tcp);
        let started = Instant::now();

        stream
            .write_all(format!("{url}\r\n").as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| e.to_string())?;

        let mut data = Vec::with_capacity(16 * 1024);
        let mut buffer = [0u8; 16 * 1024];
        let header_end = loop {
            if let Some(i) = data.iter().position(|&b| b == b'\n') {
                break i;
            }
            if data.len() > MAX_HEADER {
                return Ok(Response::Gone);
            }
            match read(&mut stream, &mut buffer, started)? {
                0 if data.is_empty() => return Err("closed without an answer".into()),
                0 => break data.len(),
                n => data.extend_from_slice(&buffer[..n]),
            }
        };
        let line = String::from_utf8_lossy(&data[..header_end]);
        let line = line.trim_end_matches('\r');
        let Some((status, meta)) = parse_status(line) else {
            return Ok(Response::Gone);
        };

        Ok(match status {
            10..=19 => Response::Input {
                prompt: meta.to_string(),
                sensitive: status == 11,
            },
            20..=29 => {
                let mime = if meta.is_empty() {
                    "text/gemini; charset=utf-8".to_string()
                } else {
                    meta.to_string()
                };
                if !is_text(&mime) {
                    // Only the type: what it is and where, not its bytes.
                    return Ok(Response::Document { mime, body: None });
                }
                let mut body = data.split_off((header_end + 1).min(data.len()));
                while body.len() < MAX_BODY {
                    match read(&mut stream, &mut buffer, started)? {
                        0 => break,
                        n => body.extend_from_slice(&buffer[..n]),
                    }
                }
                body.truncate(MAX_BODY);
                Response::Document {
                    mime,
                    body: Some(body),
                }
            }
            30..=39 => Response::Redirect(meta.to_string()),
            44 => {
                let seconds = meta.trim().parse::<u64>().unwrap_or(60).clamp(1, 3600);
                Response::SlowDown(Duration::from_secs(seconds))
            }
            40..=49 => Response::TryLater,
            _ => Response::Gone,
        })
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        let text = decode(body, charset(mime).as_deref());
        let kind = mime_type(mime);
        match kind.as_str() {
            "text/gemini" => gemtext(url, &text),
            "text/markdown" => Parsed {
                title: text
                    .lines()
                    .find_map(|l| l.strip_prefix("# "))
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty()),
                links: mentions(&text, Self::SCHEME),
                body: text,
            },
            _ => Parsed {
                title: None,
                links: mentions(&text, Self::SCHEME),
                body: text,
            },
        }
    }
}

/// One read, as Gemini servers end responses: many close the connection
/// without TLS's goodbye, which counts as the end here, not an error.
fn read(stream: &mut impl Read, buffer: &mut [u8], started: Instant) -> Result<usize, String> {
    if started.elapsed() > TOTAL_TIMEOUT {
        return Err("too slow".into());
    }
    match stream.read(buffer) {
        Ok(n) => Ok(n),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
            ) =>
        {
            Ok(0)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// `20 text/gemini` → (20, "text/gemini"). `None` if it isn't a status line.
fn parse_status(line: &str) -> Option<(u8, &str)> {
    let digits = line.get(..2)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = &line[2..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some((digits.parse().ok()?, rest.trim()))
}

/// `text/gemini; charset=utf-8; lang=en` → `text/gemini`.
fn mime_type(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_text(mime: &str) -> bool {
    mime_type(mime).starts_with("text/")
}

fn charset(mime: &str) -> Option<String> {
    mime.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    })
}

/// UTF-8 unless the server says Latin-1; anything else is read as UTF-8,
/// with what doesn't fit replaced.
fn decode(bytes: &[u8], charset: Option<&str>) -> String {
    match charset {
        Some(
            "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "l1" | "windows-1252" | "cp1252",
        ) => bytes.iter().map(|&b| b as char).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// A gemtext document as a page: the first `#` heading is the title (or
/// the first heading of any level); the text keeps every line's words and
/// drops the markup (`=>`, `#`, `*`, `>`), and preformatted blocks stay
/// as they are. Links are every `=>` line, plus `gemini://` URLs written
/// in the text.
fn gemtext(base: &Url, text: &str) -> Parsed {
    let mut lines: Vec<&str> = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    let mut title: Option<&str> = None;
    let mut any_heading: Option<&str> = None;
    let mut preformatted = false;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("```") {
            preformatted = !preformatted;
            continue;
        }
        if preformatted {
            lines.push(line);
            links.extend(mentions(line, Gemini::SCHEME));
            continue;
        }
        if let Some(rest) = line.strip_prefix("=>") {
            let rest = rest.trim();
            let (target, label) = match rest.split_once(char::is_whitespace) {
                Some((target, label)) => (target, label.trim()),
                None => (rest, ""),
            };
            if target.is_empty() {
                continue;
            }
            if let Some(url) = resolve_any(base, target) {
                links.push(Link {
                    url: url.to_string(),
                    label: (!label.is_empty()).then(|| label.to_string()),
                });
            }
            lines.push(if label.is_empty() { target } else { label });
        } else if line.starts_with('#') {
            let heading = line.trim_start_matches('#').trim();
            if heading.is_empty() {
                continue;
            }
            if !line.starts_with("##") {
                title = title.or(Some(heading));
            }
            any_heading = any_heading.or(Some(heading));
            lines.push(heading);
        } else if let Some(item) = line.strip_prefix("* ") {
            lines.push(item.trim());
        } else if let Some(quote) = line.strip_prefix('>') {
            lines.push(quote.trim());
            links.extend(mentions(quote, Gemini::SCHEME));
        } else {
            lines.push(line);
            links.extend(mentions(line, Gemini::SCHEME));
        }
    }

    Parsed {
        title: title.or(any_heading).map(str::to_string),
        body: lines.join("\n").trim().to_string(),
        links,
    }
}

/// Accepts any certificate. Gemini capsules sign their own (trust on first
/// use, if anything), and the spider only reads public pages, so there's
/// nothing to protect by checking them. The handshake's signatures are still
/// checked, as TLS needs.
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
    fn status_lines() {
        assert_eq!(parse_status("20 text/gemini"), Some((20, "text/gemini")));
        assert_eq!(parse_status("20"), Some((20, "")));
        assert_eq!(parse_status("31 gemini://x/"), Some((31, "gemini://x/")));
        assert_eq!(parse_status("2 x"), None);
        assert_eq!(parse_status("200 OK"), None);
        assert_eq!(parse_status("HTTP/1.1 400"), None);
    }

    #[test]
    fn mime_types() {
        let m = "text/gemini; charset=ISO-8859-1; lang=en";
        assert_eq!(mime_type(m), "text/gemini");
        assert_eq!(charset(m).as_deref(), Some("iso-8859-1"));
        assert!(is_text(m));
        assert!(!is_text("image/png"));
        assert_eq!(
            decode(&[0x63, 0x61, 0x66, 0xe9], Some("iso-8859-1")),
            "café"
        );
    }

    #[test]
    fn gemtext_pages() {
        let base = Url::parse("gemini://example.org/gemlog/index.gmi").unwrap();
        let text = "\
## Not the title
# My gemlog
Welcome! See also gemini://friend.net/hello.gmi, it's nice.
=> post-1.gmi First post
=> gemini://other.org/
=>   https://example.com/   The web
* an item
> a quote
```ascii art
 => not a link
```
";
        let page = gemtext(&base, text);
        assert_eq!(page.title.as_deref(), Some("My gemlog"));
        assert_eq!(
            page.body,
            "Not the title\nMy gemlog\nWelcome! See also gemini://friend.net/hello.gmi, it's nice.\n\
             First post\ngemini://other.org/\nThe web\nan item\na quote\n => not a link"
        );
        let links: Vec<(&str, Option<&str>)> = page
            .links
            .iter()
            .map(|l| (l.url.as_str(), l.label.as_deref()))
            .collect();
        assert_eq!(
            links,
            [
                ("gemini://friend.net/hello.gmi", None),
                ("gemini://example.org/gemlog/post-1.gmi", Some("First post")),
                ("gemini://other.org/", None),
                ("https://example.com/", Some("The web")),
            ]
        );
    }
}
