//! Reader and writer for the `.lost` dataset format (ARCHITECTURE.md §9),
//! and the signing key ("your ID") that signs exports.
//!
//! ```text
//! "LOST" · version (1 byte) · flags (1 byte) · header length (u32 LE)
//! header: JSON
//! chunk 1 … chunk n: zstd frames, each holding JSON Lines (one page a line)
//! footer: JSON — every chunk's offset, size, page count and BLAKE3 hash,
//!         and the ed25519 signature, if signed
//! footer length (u32 LE) · "TSOL"
//! ```
//!
//! The footer comes last, so a file is written in one pass. The signature
//! covers the header and every chunk's place, size, count and hash, so a
//! changed byte anywhere is caught: in the header or footer when the file
//! is opened, in a chunk when that chunk is read. A damaged chunk is
//! skipped on its own; the others still import.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::record::{Doc, url_key};
use crate::text::truncate_at_word;

/// The first bytes of every `.lost` file.
pub const MAGIC: &[u8; 4] = b"LOST";
/// The last bytes of a complete `.lost` file.
const END: &[u8; 4] = b"TSOL";
/// The layout version this code writes and reads.
pub const FORMAT_VERSION: u8 = 1;
/// The version of the page records inside.
pub const SCHEMA_VERSION: u32 = 1;
const FLAG_SIGNED: u8 = 1;
/// Prefix of the signed message, so a LOST signature can't be passed off
/// as a signature of anything else.
const SIGNATURE_CONTEXT: &[u8] = b"LOST dataset signature v1\0";

/// A chunk is closed once it holds this many bytes of JSON, or this many pages.
const CHUNK_BYTES: usize = 8 << 20;
const CHUNK_PAGES: u64 = 10_000;
const ZSTD_LEVEL: i32 = 7;

/// Limits when reading files from elsewhere.
const MAX_HEADER: u64 = 1 << 20;
const MAX_FOOTER: u64 = 64 << 20;
const MAX_CHUNK: u64 = 256 << 20;
const MAX_CHUNK_UNPACKED: u64 = 1 << 30;

/// The file holding the signing key, in LOST's own folder.
pub const KEY_FILE: &str = "signing.key";

/// Data source name for pages that don't say where they came from.
pub const UNKNOWN_SOURCE: &str = "lost";

/// One page in a `.lost` file: a line of JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub url: String,
    /// Data sources, e.g. `["hacker_news"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Plain text.
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// When the page was captured (Unix seconds), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawled: Option<i64>,
}

impl Page {
    pub fn from_doc(doc: Doc) -> Page {
        Page {
            url: doc.url,
            sources: doc.sources,
            title: doc.title,
            body: doc.body,
            links: doc.links,
            crawled: doc.crawled,
        }
    }

    /// The document to index. `None` for an unusable URL.
    pub fn into_doc(self, import_id: u64) -> Option<Doc> {
        let key = url_key(&self.url)?;
        let mut sources: Vec<String> = self
            .sources
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        sources.dedup();
        if sources.is_empty() {
            sources.push(UNKNOWN_SOURCE.to_string());
        }
        Some(Doc {
            key,
            url: self.url,
            sources,
            title: self.title.filter(|t| !t.trim().is_empty()),
            body: truncate_at_word(&self.body, crate::MAX_BODY_BYTES).to_string(),
            links: self.links,
            crawled: self.crawled,
            import_id,
        })
    }
}

/// The header: what the file is.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Header {
    /// When the file was made (Unix seconds).
    #[serde(default)]
    pub created: i64,
    /// What made it, e.g. `LOST 0.2.0`.
    #[serde(default)]
    pub generator: String,
    /// The search the pages were exported with; none for everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The signer's public key, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
    #[serde(default)]
    pub schema: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Footer {
    pages: u64,
    chunks: Vec<ChunkEntry>,
    /// Hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChunkEntry {
    offset: u64,
    size: u64,
    pages: u64,
    /// BLAKE3 of the compressed bytes, hex.
    blake3: String,
}

/// The message a signature signs: the header, and every chunk's place,
/// size, page count and hash. `None` if a hash isn't valid hex.
fn signed_message(header: &[u8], chunks: &[ChunkEntry]) -> Option<Vec<u8>> {
    let mut message = Vec::with_capacity(SIGNATURE_CONTEXT.len() + 40 + chunks.len() * 56);
    message.extend_from_slice(SIGNATURE_CONTEXT);
    message.extend_from_slice(&blake3_bytes(header));
    message.extend_from_slice(&(chunks.len() as u64).to_le_bytes());
    for c in chunks {
        let hash: [u8; 32] = from_hex(&c.blake3)?.try_into().ok()?;
        message.extend_from_slice(&c.offset.to_le_bytes());
        message.extend_from_slice(&c.size.to_le_bytes());
        message.extend_from_slice(&c.pages.to_le_bytes());
        message.extend_from_slice(&hash);
    }
    Some(message)
}

fn blake3_bytes(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

// ── Writing ─────────────────────────────────────────────────────────────

/// Writes a `.lost` file in one pass: [`Writer::add`] each page, then
/// [`Writer::finish`].
pub struct Writer<'a, W: Write> {
    out: W,
    identity: Option<&'a Identity>,
    header: Vec<u8>,
    /// Bytes written so far.
    position: u64,
    /// JSON Lines of the chunk being filled.
    buffer: Vec<u8>,
    buffered: u64,
    chunks: Vec<ChunkEntry>,
    pages: u64,
}

impl<'a, W: Write> Writer<'a, W> {
    /// Starts the file: signed with `identity`, if given.
    pub fn new(mut out: W, query: Option<&str>, identity: Option<&'a Identity>) -> Result<Self> {
        let header = Header {
            created: crate::store::now(),
            generator: format!("LOST {}", env!("CARGO_PKG_VERSION")),
            query: query.map(str::to_string),
            signer: identity.map(|i| to_hex(&i.public_key())),
            schema: SCHEMA_VERSION,
        };
        let header = serde_json::to_vec(&header)?;
        let flags = if identity.is_some() { FLAG_SIGNED } else { 0 };
        out.write_all(MAGIC)?;
        out.write_all(&[FORMAT_VERSION, flags])?;
        out.write_all(&(header.len() as u32).to_le_bytes())?;
        out.write_all(&header)?;
        Ok(Writer {
            out,
            identity,
            position: 10 + header.len() as u64,
            header,
            buffer: Vec::with_capacity(CHUNK_BYTES + (1 << 20)),
            buffered: 0,
            chunks: Vec::new(),
            pages: 0,
        })
    }

    pub fn add(&mut self, page: &Page) -> Result<()> {
        serde_json::to_writer(&mut self.buffer, page)?;
        self.buffer.push(b'\n');
        self.buffered += 1;
        if self.buffer.len() >= CHUNK_BYTES || self.buffered >= CHUNK_PAGES {
            self.close_chunk()?;
        }
        Ok(())
    }

    /// Pages added so far.
    pub fn pages(&self) -> u64 {
        self.pages + self.buffered
    }

    fn close_chunk(&mut self) -> Result<()> {
        if self.buffered == 0 {
            return Ok(());
        }
        let packed = zstd::bulk::compress(&self.buffer, ZSTD_LEVEL)?;
        self.out.write_all(&packed)?;
        self.chunks.push(ChunkEntry {
            offset: self.position,
            size: packed.len() as u64,
            pages: self.buffered,
            blake3: to_hex(&blake3_bytes(&packed)),
        });
        self.position += packed.len() as u64;
        self.pages += self.buffered;
        self.buffered = 0;
        self.buffer.clear();
        Ok(())
    }

    /// Writes the last chunk, the footer and the signature. Returns the
    /// output, the number of pages and the file's size in bytes.
    pub fn finish(mut self) -> Result<(W, u64, u64)> {
        self.close_chunk()?;
        let signature = match self.identity {
            Some(identity) => {
                let message = signed_message(&self.header, &self.chunks)
                    .ok_or("internal error: a chunk hash isn't hex")?;
                Some(to_hex(&identity.sign(&message)))
            }
            None => None,
        };
        let footer = serde_json::to_vec(&Footer {
            pages: self.pages,
            chunks: std::mem::take(&mut self.chunks),
            signature,
        })?;
        self.out.write_all(&footer)?;
        self.out.write_all(&(footer.len() as u32).to_le_bytes())?;
        self.out.write_all(END)?;
        self.out.flush()?;
        let size = self.position + footer.len() as u64 + 8;
        Ok((self.out, self.pages, size))
    }
}

// ── Reading ─────────────────────────────────────────────────────────────

/// An open `.lost` file whose layout and signature have been checked.
pub struct Reader {
    file: File,
    name: String,
    header: Header,
    footer: Footer,
    signer: Option<[u8; 32]>,
}

/// The pages of one chunk.
pub struct Chunk {
    pub pages: Vec<Page>,
    /// Lines that aren't a page.
    pub bad_lines: u64,
}

impl Reader {
    /// Opens a `.lost` file and checks it: its layout, and its signature if
    /// it's signed. Fails with a message for the user if it's not a `.lost`
    /// file, is cut short, or was changed after it was signed.
    pub fn open(path: &Path) -> Result<Reader> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let mut file =
            File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
        let len = file.metadata()?.len();
        let incomplete = || -> crate::Error {
            format!("{name} is incomplete (was the download cut short?)").into()
        };
        let damaged = || -> crate::Error { format!("{name} is damaged").into() };

        let mut prefix = [0u8; 10];
        if len < 10 {
            return Err(if len >= 4 {
                incomplete()
            } else {
                not_lost(&name)
            });
        }
        file.read_exact(&mut prefix)?;
        if &prefix[..4] != MAGIC {
            return Err(not_lost(&name));
        }
        if prefix[4] != FORMAT_VERSION {
            return Err(if prefix[4] > FORMAT_VERSION {
                format!("{name} was made by a newer version of LOST; update LOST to import it")
                    .into()
            } else {
                damaged()
            });
        }
        let flags = prefix[5];
        let header_len = u64::from(u32::from_le_bytes([
            prefix[6], prefix[7], prefix[8], prefix[9],
        ]));
        if header_len > MAX_HEADER {
            return Err(damaged());
        }
        let data_start = 10 + header_len;
        if data_start + 8 > len {
            return Err(incomplete());
        }
        let mut header_bytes = vec![0u8; header_len as usize];
        file.read_exact(&mut header_bytes)?;
        let header: Header = serde_json::from_slice(&header_bytes).map_err(|_| damaged())?;

        let mut trailer = [0u8; 8];
        file.seek(SeekFrom::Start(len - 8))?;
        file.read_exact(&mut trailer)?;
        if &trailer[4..] != END {
            return Err(incomplete());
        }
        let footer_len = u64::from(u32::from_le_bytes([
            trailer[0], trailer[1], trailer[2], trailer[3],
        ]));
        if footer_len > MAX_FOOTER || data_start + footer_len + 8 > len {
            return Err(damaged());
        }
        let footer_start = len - 8 - footer_len;
        let mut footer_bytes = vec![0u8; footer_len as usize];
        file.seek(SeekFrom::Start(footer_start))?;
        file.read_exact(&mut footer_bytes)?;
        let footer: Footer = serde_json::from_slice(&footer_bytes).map_err(|_| damaged())?;

        // The chunks fill the space between header and footer, in order.
        let mut next = data_start;
        let mut pages = 0u64;
        for c in &footer.chunks {
            if c.offset != next || c.size == 0 || c.size > MAX_CHUNK {
                return Err(damaged());
            }
            next = c.offset + c.size;
            pages = pages.saturating_add(c.pages);
        }
        if next != footer_start || pages != footer.pages {
            return Err(damaged());
        }

        let signed = flags & FLAG_SIGNED != 0;
        let signer = match (signed, &header.signer, &footer.signature) {
            (false, None, None) => None,
            (true, Some(key), Some(signature)) => {
                let key: [u8; 32] = from_hex(key)
                    .and_then(|k| k.try_into().ok())
                    .ok_or_else(damaged)?;
                let signature: [u8; 64] = from_hex(signature)
                    .and_then(|s| s.try_into().ok())
                    .ok_or_else(damaged)?;
                let message = signed_message(&header_bytes, &footer.chunks).ok_or_else(damaged)?;
                if !verify(&key, &message, &signature) {
                    return Err(format!(
                        "{name} was changed after it was signed, so it can't be trusted; not imported"
                    )
                    .into());
                }
                Some(key)
            }
            _ => return Err(damaged()),
        };

        Ok(Reader {
            file,
            name,
            header,
            footer,
            signer,
        })
    }

    /// The file's name, for messages.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Pages in the file.
    pub fn pages(&self) -> u64 {
        self.footer.pages
    }

    /// Public key of whoever signed the file (checked), if it's signed.
    pub fn signer(&self) -> Option<[u8; 32]> {
        self.signer
    }

    pub fn chunk_count(&self) -> usize {
        self.footer.chunks.len()
    }

    /// Pages in chunk `i`, as the footer says.
    pub fn chunk_pages(&self, i: usize) -> u64 {
        self.footer.chunks[i].pages
    }

    /// Reads chunk `i`; `None` if it's damaged (its bytes don't match the
    /// hash in the footer, or don't decompress).
    pub fn read_chunk(&mut self, i: usize) -> Result<Option<Chunk>> {
        let packed = self.read_packed(i)?;
        Ok(self.packed(i).decode(&packed))
    }

    /// Chunk `i`'s bytes as stored, not checked yet: [`PackedChunk::decode`]
    /// does that. (Reading is quick; decoding can run on other threads.)
    pub fn read_packed(&mut self, i: usize) -> Result<Vec<u8>> {
        let entry = &self.footer.chunks[i];
        let mut packed = vec![0u8; entry.size as usize];
        self.file.seek(SeekFrom::Start(entry.offset))?;
        self.file.read_exact(&mut packed)?;
        Ok(packed)
    }

    /// What decoding chunk `i` needs to know about it.
    pub fn packed(&self, i: usize) -> PackedChunk {
        let entry = &self.footer.chunks[i];
        PackedChunk {
            blake3: entry.blake3.to_ascii_lowercase(),
            pages: entry.pages,
        }
    }
}

/// A chunk's hash and page count, from the footer: enough to check and
/// decode its bytes anywhere.
#[derive(Debug, Clone)]
pub struct PackedChunk {
    blake3: String,
    pages: u64,
}

impl PackedChunk {
    /// Checks and decodes a chunk's bytes; `None` if they don't match the
    /// hash in the footer, or don't decompress.
    pub fn decode(&self, packed: &[u8]) -> Option<Chunk> {
        if to_hex(&blake3_bytes(packed)) != self.blake3 {
            return None;
        }
        let mut text = Vec::new();
        let unpacked = zstd::stream::read::Decoder::new(packed)
            .and_then(|d| d.take(MAX_CHUNK_UNPACKED + 1).read_to_end(&mut text));
        if unpacked.is_err() || text.len() as u64 > MAX_CHUNK_UNPACKED {
            return None;
        }
        let mut chunk = Chunk {
            pages: Vec::with_capacity(self.pages.min(CHUNK_PAGES) as usize),
            bad_lines: 0,
        };
        for line in text.split(|&b| b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Page>(line) {
                Ok(page) => chunk.pages.push(page),
                Err(_) => chunk.bad_lines += 1,
            }
        }
        Some(chunk)
    }
}

fn not_lost(name: &str) -> crate::Error {
    format!("{name} isn't a .lost file").into()
}

// ── Your ID: the signing key ───────────────────────────────────────────

/// The key that signs your exports. Its public half is your ID: a
/// pseudonym, never a name (ARCHITECTURE.md §9.2).
pub struct Identity {
    key: ed25519_dalek::SigningKey,
}

impl Identity {
    /// Where the key is kept: LOST's own folder (next to lost.toml).
    pub fn default_dir() -> Option<PathBuf> {
        crate::config::user_dir()
    }

    /// The key in `dir`, if one has been made.
    pub fn load(dir: &Path) -> Result<Option<Identity>> {
        let path = dir.join(KEY_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("can't read {}: {e}", path.display()).into()),
        };
        let seed: [u8; 32] = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .and_then(from_hex)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| format!("{} isn't a LOST signing key", path.display()))?;
        Ok(Some(Identity {
            key: ed25519_dalek::SigningKey::from_bytes(&seed),
        }))
    }

    /// The key in `dir`, made first if there isn't one. The flag says
    /// whether it was just made.
    pub fn load_or_create(dir: &Path) -> Result<(Identity, bool)> {
        if let Some(identity) = Identity::load(dir)? {
            return Ok((identity, false));
        }
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| format!("can't make a signing key: {e}"))?;
        crate::config::ensure_dir(dir)?;
        let path = dir.join(KEY_FILE);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let text = format!(
            "# LOST signing key. Keep it private: whoever has it can sign files as you.\n{}\n",
            to_hex(&seed)
        );
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .and_then(|_| file.sync_all())
                    .map_err(|e| format!("can't save {}: {e}", path.display()))?;
                Ok((
                    Identity {
                        key: ed25519_dalek::SigningKey::from_bytes(&seed),
                    },
                    true,
                ))
            }
            // Made by another LOST at the same moment: use that one.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Identity::load(dir)?
                .map(|identity| (identity, false))
                .ok_or_else(|| format!("can't read {}", path.display()).into()),
            Err(e) => Err(format!("can't save {}: {e}", path.display()).into()),
        }
    }

    /// Replaces the key in `dir` with a new one (or makes the first), and
    /// returns it. The old one is gone for good: files signed with it stay
    /// signed by the old ID.
    pub fn replace(dir: &Path) -> Result<Identity> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| format!("can't make a signing key: {e}"))?;
        crate::config::ensure_dir(dir)?;
        let path = dir.join(KEY_FILE);
        let partial = dir.join(format!("{KEY_FILE}.part"));
        let _ = std::fs::remove_file(&partial);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let text = format!(
            "# LOST signing key. Keep it private: whoever has it can sign files as you.\n{}\n",
            to_hex(&seed)
        );
        options
            .open(&partial)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|e| format!("can't save {}: {e}", path.display()))?;
        Ok(Identity {
            key: ed25519_dalek::SigningKey::from_bytes(&seed),
        })
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// The ID as people see it, e.g. `3f9a 1c2e 77b0 1d4e`.
    pub fn id(&self) -> String {
        short_id(&self.public_key())
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        let signature: ed25519_dalek::Signature = self.key.sign(message);
        signature.to_bytes()
    }
}

fn verify(key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(key) else {
        return false;
    };
    let signature = ed25519_dalek::Signature::from_bytes(signature);
    key.verify_strict(message, &signature).is_ok()
}

/// A public key as people see it: its first 16 hex digits in groups of
/// four, e.g. `3f9a 1c2e 77b0 1d4e`.
pub fn short_id(key: &[u8; 32]) -> String {
    let hex = to_hex(&key[..8]);
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The same, from the hex form stored in the trust table.
pub fn short_id_hex(key: &str) -> String {
    match from_hex(key).and_then(|k| <[u8; 32]>::try_from(k).ok()) {
        Some(key) => short_id(&key),
        None => key.chars().take(16).collect(),
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 15)] as char);
    }
    out
}

pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let (pairs, odd) = text.trim().as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    pairs
        .iter()
        .map(|&[high, low]| Some(digit(high)? << 4 | digit(low)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        assert_eq!(to_hex(&[0, 1, 0xab, 0xff]), "0001abff");
        assert_eq!(from_hex("0001ABff"), Some(vec![0, 1, 0xab, 0xff]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        let key = [
            0x3f, 0x9a, 0x1c, 0x2e, 0x77, 0xb0, 0x1d, 0x4e, 9, 9, 9, 9, 9, 9, 9, 9, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(short_id(&key), "3f9a 1c2e 77b0 1d4e");
        assert_eq!(short_id_hex(&to_hex(&key)), "3f9a 1c2e 77b0 1d4e");
    }
}

/// Round trips through real files.
#[cfg(test)]
mod file_tests {
    use super::*;

    /// The pages the program imports the first time it runs.
    #[test]
    fn sample_file_reads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("res/data/sample.lost");
        let mut reader = Reader::open(&path).unwrap();
        assert_eq!(reader.pages(), 2);
        assert!(reader.signer().is_none());
        let chunk = reader.read_chunk(0).unwrap().unwrap();
        assert_eq!(chunk.bad_lines, 0);
        let urls: Vec<&str> = chunk.pages.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://news.ycombinator.com/", "https://slashdot.org/"]
        );
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lost-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn page(i: usize) -> Page {
        Page {
            url: format!("https://example.com/{i}"),
            sources: vec!["test".into()],
            title: Some(format!("Page {i}")),
            body: format!("Text of page {i}. ").repeat(20),
            links: vec!["https://example.org/".into()],
            crawled: Some(1_700_000_000 + i as i64),
        }
    }

    fn write(path: &Path, pages: usize, identity: Option<&Identity>) -> u64 {
        let file = std::io::BufWriter::new(File::create(path).unwrap());
        let mut writer = Writer::new(file, Some("example"), identity).unwrap();
        for i in 0..pages {
            writer.add(&page(i)).unwrap();
        }
        let (_, n, size) = writer.finish().unwrap();
        assert_eq!(n, pages as u64);
        assert_eq!(size, std::fs::metadata(path).unwrap().len());
        size
    }

    fn find(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap()
    }

    fn read_all(reader: &mut Reader) -> (Vec<Page>, usize) {
        let mut pages = Vec::new();
        let mut damaged = 0;
        for i in 0..reader.chunk_count() {
            match reader.read_chunk(i).unwrap() {
                Some(chunk) => pages.extend(chunk.pages),
                None => damaged += 1,
            }
        }
        (pages, damaged)
    }

    #[test]
    fn unsigned_round_trip() {
        let path = temp("plain.lost");
        write(&path, 25_000, None);
        let mut reader = Reader::open(&path).unwrap();
        assert_eq!(reader.pages(), 25_000);
        assert_eq!(reader.chunk_count(), 3);
        assert!(reader.signer().is_none());
        assert_eq!(reader.header().query.as_deref(), Some("example"));
        let (pages, damaged) = read_all(&mut reader);
        assert_eq!(damaged, 0);
        assert_eq!(pages.len(), 25_000);
        assert_eq!(pages[12_345], page(12_345));
    }

    #[test]
    fn signed_files_catch_changes() {
        let dir = temp("keys");
        let (identity, created) = Identity::load_or_create(&dir).unwrap();
        assert!(created);
        let (again, created) = Identity::load_or_create(&dir).unwrap();
        assert!(!created);
        assert_eq!(identity.public_key(), again.public_key());

        let path = temp("signed.lost");
        write(&path, 15_000, Some(&identity));
        let reader = Reader::open(&path).unwrap();
        assert_eq!(reader.signer(), Some(identity.public_key()));

        let bytes = std::fs::read(&path).unwrap();
        // A changed byte in a chunk: that chunk is skipped, the rest reads.
        let header_len = u32::from_le_bytes(bytes[6..10].try_into().unwrap()) as usize;
        let mut changed = bytes.clone();
        changed[10 + header_len + 50] ^= 1;
        let tampered = temp("tampered.lost");
        std::fs::write(&tampered, &changed).unwrap();
        let mut reader = Reader::open(&tampered).unwrap();
        let (pages, damaged) = read_all(&mut reader);
        assert_eq!((pages.len(), damaged), (5_000, 1));

        // A changed header (a different query): the signature fails.
        let at = find(&bytes, b"example");
        let mut changed = bytes.clone();
        changed[at] = b'E';
        std::fs::write(&tampered, &changed).unwrap();
        let error = Reader::open(&tampered).err().unwrap().to_string();
        assert!(error.contains("changed after it was signed"), "{error}");

        // Cut short.
        std::fs::write(&tampered, &bytes[..bytes.len() - 100]).unwrap();
        let error = Reader::open(&tampered).err().unwrap().to_string();
        assert!(error.contains("incomplete"), "{error}");

        // Someone else's key in the header: the signature fails.
        let other = Identity {
            key: ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
        };
        let mine = to_hex(&identity.public_key());
        let theirs = to_hex(&other.public_key());
        let mut changed = bytes.clone();
        let at = find(&bytes, mine.as_bytes());
        changed[at..at + 64].copy_from_slice(theirs.as_bytes());
        std::fs::write(&tampered, &changed).unwrap();
        assert!(Reader::open(&tampered).is_err());

        // Not a .lost file at all.
        std::fs::write(&tampered, b"url,title\n").unwrap();
        let error = Reader::open(&tampered).err().unwrap().to_string();
        assert!(error.contains("isn't a .lost file"), "{error}");
    }

    #[test]
    fn pages_become_docs() {
        let mut p = page(1);
        p.sources = vec![" ".into()];
        p.title = Some("  ".into());
        let doc = p.into_doc(9).unwrap();
        assert_eq!(doc.sources, vec![UNKNOWN_SOURCE]);
        assert_eq!(doc.title, None);
        assert_eq!(doc.import_id, 9);
        let back = Page::from_doc(doc);
        assert_eq!(back.url, "https://example.com/1");
        assert!(
            Page {
                url: "not a url".into(),
                ..Page::default()
            }
            .into_doc(1)
            .is_none()
        );
    }
}
