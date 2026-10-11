//! Imports `.lost` files, LOST's own dataset format (ARCHITECTURE.md §9):
//! decides whether a file may be imported (its signer and `import.trust`),
//! then reads it chunk by chunk.

use std::path::Path;

use crate::Result;
use crate::config::Trust;
use crate::lostfile::{self, Identity, PackedChunk, Reader, short_id, to_hex};
use crate::record::Doc;
use crate::store::Store;
use crate::text::format_datetime;

/// Data source name of `.lost` imports in the `imports` table. (The pages
/// keep the sources they came with.)
pub const SOURCE: &str = "lost";

/// Whether the start of a file is a `.lost` file.
pub fn detect(head: &[u8]) -> bool {
    head.starts_with(lostfile::MAGIC)
}

/// What the user is asked before a `.lost` file from someone they don't
/// trust (yet) is imported.
#[derive(Debug, Clone)]
pub struct Question {
    /// The file's name.
    pub file: String,
    pub pages: u64,
    /// The signer's ID (e.g. `3f9a 1c2e 77b0 1d4e`); `None` if unsigned.
    pub signer: Option<String>,
    /// When the file was made, as text.
    pub created: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    No,
    Yes,
    /// Yes, and import this signer's files without asking from now on.
    YesAndTrust,
}

/// Whether a file may be imported.
pub enum Decision {
    Import,
    /// The user said no.
    Declined,
    /// `import.trust` doesn't allow it; why, for the user.
    Refused(String),
}

/// Checks `path` (layout and signature), and decides whether to import it:
/// files signed by you or by someone you trust import; for the others,
/// `trust` decides, calling `ask` when it says to ask.
pub fn approve(
    path: &Path,
    data_dir: &Path,
    trust: Trust,
    ask: &mut dyn FnMut(&Question) -> Answer,
) -> Result<Decision> {
    let reader = Reader::open(path)?;
    if trust == Trust::Any {
        return Ok(Decision::Import);
    }
    let signer = reader.signer();
    if let Some(key) = signer {
        let mine = Identity::default_dir()
            .and_then(|dir| Identity::load(&dir).ok().flatten())
            .is_some_and(|identity| identity.public_key() == key);
        if mine || Store::open(data_dir)?.is_trusted(&to_hex(&key))? {
            return Ok(Decision::Import);
        }
    } else if trust == Trust::SignedOnly {
        return Ok(Decision::Refused(format!(
            "{} isn't signed, and LOST is set to import signed files only",
            reader.name()
        )));
    }
    let question = Question {
        file: reader.name().to_string(),
        pages: reader.pages(),
        signer: signer.as_ref().map(short_id),
        created: format_datetime(reader.header().created),
    };
    Ok(match ask(&question) {
        Answer::No => Decision::Declined,
        Answer::Yes => Decision::Import,
        Answer::YesAndTrust => {
            if let Some(key) = signer {
                Store::open(data_dir)?.trust(&to_hex(&key), "")?;
            }
            Decision::Import
        }
    })
}

/// Imports one `.lost` file, handing each page to `add` (`None` for a line
/// that isn't a usable page), in the file's order. Damaged chunks are
/// skipped; the note says so.
///
/// Chunks are checked, decompressed and parsed on several threads at once
/// (ARCHITECTURE.md §9.1); `add`, which writes to the index, runs on this
/// one, in order. Only a few chunks are ever read ahead, so memory stays
/// bounded however big the file is.
pub fn import(
    path: &Path,
    import_id: u64,
    add: &mut dyn FnMut(Option<Doc>) -> Result<()>,
) -> Result<Option<String>> {
    let mut reader = Reader::open(path)?;
    let count = reader.chunk_count();
    let pages: Vec<u64> = (0..count).map(|i| reader.chunk_pages(i)).collect();
    let (mut damaged, mut missed) = (0usize, 0u64);
    let mut take = |i: usize, docs: Option<Vec<Option<Doc>>>| -> Result<()> {
        match docs {
            Some(docs) => docs.into_iter().try_for_each(&mut *add),
            None => {
                damaged += 1;
                missed += pages[i];
                Ok(())
            }
        }
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS)
        .min(count);
    if threads <= 1 {
        for i in 0..count {
            let packed = reader.read_packed(i)?;
            super::count_read(packed.len() as u64);
            take(i, decode(&reader.packed(i), &packed, import_id))?;
        }
    } else {
        parallel(&mut reader, threads, import_id, &mut take)?;
    }
    Ok((damaged > 0).then(|| {
        format!(
            "{}: {damaged} of its {count} parts {} damaged and skipped ({} pages)",
            reader.name(),
            if damaged == 1 { "was" } else { "were" },
            crate::text::thousands(missed)
        )
    }))
}

/// Threads decoding chunks, at most.
const MAX_THREADS: usize = 8;

/// A chunk's pages as documents (`None` for a line that isn't a usable
/// page), or `None` if the chunk is damaged.
fn decode(chunk: &PackedChunk, packed: &[u8], import_id: u64) -> Option<Vec<Option<Doc>>> {
    let chunk = chunk.decode(packed)?;
    let bad = chunk.bad_lines as usize;
    let mut docs = Vec::with_capacity(chunk.pages.len() + bad);
    docs.extend(chunk.pages.into_iter().map(|p| p.into_doc(import_id)));
    docs.extend(std::iter::repeat_n(None, bad));
    Some(docs)
}

/// Reads chunks in order on this thread's helper, decodes them on
/// `threads` workers, and hands them to `take` in order.
fn parallel(
    reader: &mut Reader,
    threads: usize,
    import_id: u64,
    take: &mut dyn FnMut(usize, Option<Vec<Option<Doc>>>) -> Result<()>,
) -> Result<()> {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, sync_channel};
    use std::sync::{Condvar, Mutex};

    type Decoded = std::result::Result<Option<Vec<Option<Doc>>>, String>;
    let count = reader.chunk_count();
    // Chunks read but not yet handed to `take`, at most.
    let window = threads * 2;
    // How many chunks `take` has had; the reader waits for it to catch up.
    let taken = (Mutex::new(0usize), Condvar::new());
    let stop = AtomicBool::new(false);
    let (work_tx, work_rx) =
        sync_channel::<(usize, PackedChunk, std::io::Result<Vec<u8>>)>(threads);
    let work_rx = Mutex::new(work_rx);
    let (done_tx, done_rx) = sync_channel::<(usize, Decoded)>(window);

    std::thread::scope(|scope| -> Result<()> {
        // Reading: in order, a few chunks ahead of `take`.
        let (taken, stop) = (&taken, &stop);
        let reader = &mut *reader;
        scope.spawn(move || {
            for i in 0..count {
                {
                    let (lock, ready) = taken;
                    let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while i >= *done + window && !stop.load(Ordering::Relaxed) {
                        done = ready.wait(done).unwrap_or_else(|e| e.into_inner());
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let packed =
                    reader
                        .read_packed(i)
                        .map_err(|e| match e.downcast::<std::io::Error>() {
                            Ok(io) => *io,
                            Err(e) => std::io::Error::other(e.to_string()),
                        });
                if let Ok(bytes) = &packed {
                    super::count_read(bytes.len() as u64);
                }
                let failed = packed.is_err();
                if work_tx.send((i, reader.packed(i), packed)).is_err() || failed {
                    return;
                }
            }
        });
        // Decoding: whichever chunk comes next.
        for _ in 0..threads {
            let (work_rx, done_tx): (&Mutex<Receiver<_>>, _) = (&work_rx, done_tx.clone());
            scope.spawn(move || {
                loop {
                    let next = work_rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok((i, chunk, packed)) = next else { return };
                    let decoded = packed
                        .map(|bytes| decode(&chunk, &bytes, import_id))
                        .map_err(|e| e.to_string());
                    if done_tx.send((i, decoded)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(done_tx);

        // Taking: in order, whatever order they're decoded in.
        let mut waiting: BTreeMap<usize, Decoded> = BTreeMap::new();
        let mut next = 0;
        let result = (|| -> Result<()> {
            while next < count {
                let decoded = match waiting.remove(&next) {
                    Some(d) => d,
                    None => match done_rx.recv() {
                        Ok((i, d)) if i == next => d,
                        Ok((i, d)) => {
                            waiting.insert(i, d);
                            continue;
                        }
                        Err(_) => return Err("a chunk couldn't be read".into()),
                    },
                };
                take(next, decoded?)?;
                next += 1;
                let (lock, ready) = taken;
                *lock.lock().unwrap_or_else(|e| e.into_inner()) = next;
                ready.notify_all();
            }
            Ok(())
        })();
        // On an error, the helpers stop too: the workers when they can't
        // hand a chunk over, the reader when it sees `stop` — or, waiting to
        // pass a chunk on, once what it's waiting on is drained here.
        stop.store(true, Ordering::Relaxed);
        taken.1.notify_all();
        drop(done_rx);
        while work_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv()
            .is_ok()
        {}
        result
    })
}

#[cfg(test)]
mod parallel_tests {
    use super::import;
    use crate::lostfile::{Page, Writer};

    /// A .lost file of `n` pages (10,000 to a chunk).
    fn file(name: &str, n: u64) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("lost-par-{}-{name}", std::process::id()));
        let out = std::fs::File::create(&path).unwrap();
        let mut writer = Writer::new(std::io::BufWriter::new(out), None, None).unwrap();
        for i in 0..n {
            writer
                .add(&Page {
                    url: format!("https://example.com/{i}"),
                    body: format!("page {i}"),
                    ..Default::default()
                })
                .unwrap();
        }
        writer.finish().unwrap();
        path
    }

    #[test]
    fn chunks_decoded_in_parallel_come_in_order() {
        let path = file("order.lost", 35_000);
        let mut urls = Vec::new();
        let note = import(&path, 7, &mut |doc| {
            let doc = doc.unwrap();
            assert_eq!(doc.import_id, 7);
            urls.push(doc.url);
            Ok(())
        })
        .unwrap();
        assert!(note.is_none());
        assert_eq!(urls.len(), 35_000);
        assert!(
            urls.iter()
                .enumerate()
                .all(|(i, u)| *u == format!("https://example.com/{i}"))
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_damaged_chunk_is_skipped_and_an_error_stops_everything() {
        let path = file("damaged.lost", 35_000);
        let mut bytes = std::fs::read(&path).unwrap();
        let header = u32::from_le_bytes(bytes[6..10].try_into().unwrap()) as usize;
        bytes[10 + header + 50] ^= 1; // in the first chunk
        std::fs::write(&path, &bytes).unwrap();
        let mut n = 0;
        let note = import(&path, 1, &mut |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 25_000);
        assert!(note.unwrap().contains("1 of its 4 parts was damaged"));

        // `add` failing part-way: the error comes back, nothing hangs.
        let mut n = 0;
        let result = import(&path, 1, &mut |_| {
            n += 1;
            if n == 12_000 {
                Err("disk full".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(result.unwrap_err().to_string(), "disk full");
        let _ = std::fs::remove_file(path);
    }
}
