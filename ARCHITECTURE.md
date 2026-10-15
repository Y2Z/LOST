# LOST — Architecture

**LOST** (Local Offline Search Tool) is a search engine for the web that runs entirely on your own device.
The database lives locally, nothing is ever sent anywhere, and it keeps finding results with no internet connection at all.

This document records the design decisions made before the first line of code.
It is the reference for what goes where and why. When the code and this document disagree, fix one of them.

Status: **pre-alpha, design stage.**

---

## 1. Principles

1. **Nothing leaves the device.** LOST makes no network requests of its own: no telemetry, no update checks, no remote fonts or scripts in the web UI. The only network activity is the optional web server answering requests that clients send *to* it.
2. **No search history.** Queries are not logged or stored anywhere (GUI tabs are kept only while the app runs).
3. **Small core, optional everything else.** The search library is the product; CLI, web server and GUI are front ends that can each be compiled out.
4. **The user stays in control of their data.** Every import can be inspected and removed; untrusted datasets can be refused or accepted knowingly.
5. **Efficient, not engaging.** No ads, no ranking tricks to keep people scrolling — just the most relevant results, fast.

---

## 2. Names and constants

| Thing | Value |
|---|---|
| Project | LOST (Local Offline Search Tool) |
| Crate / library | `lost` |
| Binary | `lost` |
| Dataset file extension | `.lost` |
| Default server port | **1057** (leetspeak for L-O-S-T; outside every OS's ephemeral port range, no root needed) |
| Default bind address | `127.0.0.1` |
| License | 0BSD (Zero-Clause BSD) |

---

## 3. Modes of operation

| Command | Mode |
|---|---|
| `lost catfish recipe` | One-shot CLI search; prints results and exits |
| `lost` | GUI (main window + tray icon) |
| `lost -s` | Web server in the foreground (Web API + web UI) |
| `lost -b` | Same as `-s`, detached into the background (daemon) |
| `lost -w` | GUI and web server in one process |
| `lost -d FILE…` | Import one or more datasets |
| `lost -e FILE [QUERY…]` | Export the database, or what a search finds, as a `.lost` file |
| `lost --imports` / `--remove-import ID` | List past imports / remove one and its pages |
| `lost --trusted` / `--untrust ID` | List the people whose `.lost` files import without a question / stop trusting one |
| `lost --new-id` | Make a new ID to sign exports with, after asking (`--yes` skips the question) |

### 3.1 Flags

| Flag | Long form | Argument | Applies to | Meaning |
|---|---|---|---|---|
| `-a` | `--address` | `ADDR` | `-s` `-b` `-w` | Address to bind (default `127.0.0.1`; use `0.0.0.0` to serve the LAN) |
| `-b` | `--background` | — | — | Run the server as a daemon (Unix only) |
| `-d` | `--data` | `PATH` (repeatable) | — | Import dataset file(s) or folder(s) |
| `-e` | `--export` | `FILE` | — | Export the local database as a `.lost` file (§9.3); with a query, only what it finds |
| `-f` | `--format` | `plain` \| `json` | CLI search | Output format (default `plain`) |
| `-h` | `--help` | — | — | Print usage |
| `-i` | `--input` | `FILE` \| `-` | CLI search | Read the query from a file (`-` = stdin) |
| `-l` | `--limit` | `[OFFSET,]COUNT` | CLI search | SQL `LIMIT` semantics (default `50`) |
| `-o` | `--output` | `FILE` \| `-` | CLI search | Write results to a file (`-` = stdout, the default) |
| `-p` | `--port` | `PORT` | `-s` `-b` `-w` | Port to bind (default `1057`) |
| `-s` | `--server` | — | — | Run the web server |
| `-t` | `--no-tray` | — | GUI | Don't show a tray icon; closing the window quits |
| | `--minimized` | — | GUI | Start in the tray (or minimized, without a tray), e.g. when started with the session. If LOST is already open, it's left as it is |
| `-w` | `--with-server` | — | — | GUI plus web server in one process |
| `-V` | `--version` | — | — | Print version |
| | `--suggest` | `TEXT` | — | Print autocomplete suggestions (§7.4), one per line |
| | `--unsigned` | — | `-e` | Don't sign the export (it's signed by default, §9.2) |
| | `--imports` | — | — | List past imports, oldest first: number, local date, source, pages, new, updated, status, files (cut to the terminal's width). `-f json` gives the `imports` rows |
| | `--remove-import` | `ID` | — | Remove import `ID` and the pages whose current version came from it (§6.2), after asking on the terminal; `--yes` skips the question (needed without a terminal). Exit 1 if the answer is no |
| | `--trusted` | — | — | List trusted people: ID, since when, full key. `-f json` gives the `trust` rows |
| | `--untrust` | `ID` | — | Stop trusting someone: the ID as shown (`3f9a 1c2e 77b0 1d4e`), the full key, or enough of its start to be unique |
| | `--new-id` | — | — | A new signing key in place of the old one (asks first, `[y/N]`; `--yes` doesn't; without a terminal, it needs `--yes`) |
| | `--yes` | — | `--remove-import`, `--new-id` | Don't ask first |

### 3.2 `-l` / `limit` semantics

Same as SQL's `LIMIT` (MySQL/SQLite form), in both the CLI and the Web API:

- `-l 10` → the first 10 results (1–10).
- `-l 10,100` → skip 10, return up to 100 (results 11–110).
- Default: `50`. A hard upper bound (configurable) protects the server.

### 3.3 Argument rules

- A query (positional words or `-i`) cannot be combined with `-s`, `-b`, `-w` or `-d`. With `-e` it says what to export.
- `-d` and `-e` cannot be combined with each other or with `-s`, `-b`, `-w`.
- `-b` implies `-s`. `-w` implies the GUI.
- `-f` also applies to `--imports` and `--trusted`. `-f`, `-l`, `-o` otherwise only make sense with a search, `--unsigned` only with `-e`; they are an error otherwise.
- `lost` with no arguments, in an environment with no display (no `DISPLAY` / `WAYLAND_DISPLAY` on Linux/BSD, or GUI not compiled in): print an error explaining why, followed by usage. Exit code 2.
- There's no terminal UI, by design: on a machine without a display, `lost -s` and a text-mode browser (lynx, w3m) give the same thing, through the web UI's script-free pages (§10.3).

### 3.4 CLI output

`plain` — one block per result (title, snippet, URL), separated by blank lines, then the summary:

```
1. Southern Fried Catfish
   …dredge the fillets in seasoned cornmeal and fry until golden…
   https://example.com/recipes/fried-catfish

2. …
   …
   https://…

Found 1,234 results, showing 1–50 (12 ms)
```

The summary line goes to **stderr**, so stdout stays clean for piping into `less`, `grep`, etc. It starts with "Found" so it can't be mistaken for part of the last result.

When stdout is a terminal:

- The query's words are **highlighted in bold red**, like `grep`'s matches (not when `NO_COLOR` is set or `TERM=dumb`).
- **Nothing wraps:** titles, snippets and URLs are each fitted to the terminal width, keeping a 3-column margin on the right to match the indent on the left. Each line is shifted so its first match sits at most 10 characters from the left — unless the match is near the end, in which case the line stays full. Cuts are marked with `…`.
- Matches are highlighted in titles and snippets. (Highlighting in URLs: to be reviewed later.)

Files (`-o`, `> file`), pipes and JSON get the full snippet and no colour.

`json` — exactly the same object the Web API returns for `/api/find` (§10.2), so scripts can switch between the two freely.

Exit codes, like `grep`:

| Code | Meaning |
|---|---|
| `0` | Results found (or the command succeeded) |
| `1` | No results |
| `2` | Error (bad arguments, unreadable file, no display for the GUI, …) |

---

## 4. Instances and inter-process communication

### 4.1 Rules

- **One-shot CLI searches always work**, whatever else is running. They only *read* the search index, which any number of processes may do at once.
- **One GUI per user.** Starting `lost` while the GUI runs asks the running GUI to show and raise its window, then exits.
- **One server per port.** Any number of servers (`-s`, `-b`) or hybrid GUI+servers (`-w`) may run, each on its own port. Starting a second one on a port already in use fails with a clear message: `LOST server already running at 127.0.0.1:1057`.
- A GUI and a separately started server can run side by side.
- A `-w` process holds both the GUI role and its port's server role.

### 4.2 Local sockets

Single-instance detection and IPC use local sockets: Unix domain sockets on Linux, BSD and macOS; named pipes on Windows.

| Role | Socket (Unix) | Windows pipe |
|---|---|---|
| GUI | `$RUNTIME/lost/gui.sock` | `\\.\pipe\lost-<user>-gui` |
| Server on port P | `$RUNTIME/lost/server-P.sock` | `\\.\pipe\lost-<user>-server-P` |

`$RUNTIME` is `$XDG_RUNTIME_DIR` on Linux/BSD (falling back to a per-user temp folder), `$TMPDIR` on macOS, and `/run/lost` for the system service.
Socket files are created readable and writable **by their owner only**, so other users on a shared machine can't send commands.
A stale socket file (left by a crashed process) is detected by a failed connect and replaced.

### 4.3 Protocol

Newline-delimited JSON: one request object per line, one or more response objects per line. Deliberately tiny:

| Command | Purpose |
|---|---|
| `raise` | Show and focus the GUI window |
| `status` | Version, role, port, document count, whether an import is running |
| `quit` | Shut down (the GUI asks first if an import is running) |

`import` and `imports` (to reach the app database through whichever process holds it) come later; for now another process importing makes `lost -d` and the GUI's import stop at once with "Database busy".

Anything that needs the app's redb database while another LOST process holds it goes through this socket (§6.3).

---

## 5. Build features and targets

Cargo features decide what goes into the binary.

| Feature | Adds | Default |
|---|---|---|
| *(none)* | The `lost` library: index, query parser, data sources, `.lost` format | always |
| `cli` | The `lost` binary, argument parsing, CLI output, local-socket IPC | yes |
| `server` | Web server, Web API, embedded web UI, `-b` daemon mode | yes |
| `gui` | egui desktop GUI, tray icon | yes |
| `spider` | `lost-spider`, the crawler (§15): a separate program, the only part of LOST that goes online | **no** |

Examples:

```console
cargo build --release                                            # everything
cargo build --release --no-default-features --features cli       # CLI only
cargo build --release --no-default-features --features cli,server
cargo build --release --no-default-features --features cli,gui
cargo build --release --no-default-features --lib                # library only
cargo build --release --no-default-features --features spider --bin lost-spider   # the crawler only
```

Planned dependencies (to be added as each part is implemented):

| Area | Crate |
|---|---|
| Search index | `tantivy` |
| App state, imports, dedup | `redb` |
| Serialization | `serde`, `serde_json`, `toml` |
| Data sources | `csv`, `flate2` (multi-member gzip), `html-escape` or similar |
| `.lost` format | `zstd`, `blake3`, `ed25519-dalek`, `getrandom` (making the key) |
| Paths, CLI arguments | none — standard library only (`config.rs`, `args.rs`) |
| IPC | standard library Unix sockets; `interprocess` for named pipes on Windows |
| Server | `axum`, `tokio`; `libc` for `-b` on Unix |
| GUI | `eframe` (egui), `rfd` (file picker); tray: `ksni` on Linux/BSD (D-Bus StatusNotifierItem, no GTK), `tray-icon` on Windows/macOS |
| Spider | `rustls` (with `ring`: no CMake needed) for TLS, `ctrlc`, `regex` (`--blacklist`); `flate2` (already the library's) for gzipped HTTP. HTTP and SOCKS5 are its own (`http.rs`, `net.rs`): `ureq` was considered, but it only tunnels through HTTP proxies (CONNECT), while I2P's HTTP proxy is meant to be sent whole requests |

### 5.1 Platforms

- Desktop: Linux, the BSDs, macOS, Windows.
- Packaging (`.github/workflows/release.yml` builds them all for a `v*` tag and attaches them to a draft GitHub release, with `SHA256SUMS`):
  - Linux, x86_64 and aarch64, built on Ubuntu 22.04 so they run on older systems too: `.deb` (cargo-deb, `[package.metadata.deb]`: the program, desktop entry, icons, `/etc/lost/lost.toml` as a conffile, and `lost.service`, not enabled), `.AppImage` (`dist/appimage.sh`) and a tarball installed with `dist/install.sh` (also `make install` from source).
  - macOS: `LOST.app` (`dist/macos-app.sh`, `dist/Info.plist`), one universal binary for Intel and Apple silicon, zipped. Not signed or notarized yet.
  - Windows: `lost.exe` (static C runtime; icon from `build.rs`) in a zip. It's a console program, so the command line works; when it starts the desktop app with a console of its own (from the Start menu or Explorer), it lets go of that console.
  - Icons: `dist/icons/` (PNGs, `.ico`, `.icns`), rendered from `res/images/lost.svg`.
  - FreeBSD port, Docker image, signed macOS and Windows builds later.
- **Mobile and embedded:** the plan is to use the `lost` library from native apps (Kotlin on Android, Swift on iOS), generating bindings with UniFFI. egui may serve Android if needed. Watches, fridges, cars etc. use small datasets — the data always stays on the device itself.

---

## 6. Storage

```
<data dir>/
├── index/        Tantivy index (all searchable documents), stamped with its layout (§6.6)
├── lost.redb     App state: imports, provenance, dedup keys, trust list, GUI state
├── archive/      Planned: raw captures, HTML and all (§6.7)
└── busy.lock     Present while an import or export runs (§6.3)
```

### 6.1 Locations

| | Config | Data |
|---|---|---|
| System service | `/etc/lost/lost.toml` | `/var/lib/lost/.lost/` (`lost.service` sets `HOME=/var/lib/lost`) |
| User (Linux, BSD) | `~/.lost/lost.toml` | `~/.lost/` |
| User (macOS) | `~/Library/Application Support/LOST/lost.toml` | `~/Library/Application Support/LOST/` |
| User (Windows) | `%USERPROFILE%\LOST\lost.toml` | `%USERPROFILE%\LOST\` |

One folder per user holds both config and data. It's created readable by its owner only, since it also holds the signing key (§9.2).

Configuration precedence: command-line flag → the user's `lost.toml` → `/etc/lost/lost.toml` → built-in default.

### 6.2 Search index (Tantivy)

A single Tantivy index holds every document. **Everything is a page**, as in any web search engine: a URL with a title and a plain-text body. **There is exactly one document per URL**: the index is a database of the current web, so it holds one version of each page — text only. Keeping every capture, HTML included, is the job of the capture archive (§6.7, planned), outside the index. When a source brings a page whose URL is already in the database, the merge rules (§6.4) decide which version stays.

Sources that aren't web pages are turned into pages at import time. A Hacker News story becomes its HN item page, whose text is the title, the link, the submission text and the whole discussion (§8.4).

| Field | Type | Stored | Used for |
|---|---|---|---|
| `key` | string (indexed) | yes | `blake3(normalized URL)` (§6.5); one per document |
| `url` | string | yes | The original URL, for display |
| `url_text` | text (tokenized URL) | no | `inurl:`, `allinurl:` |
| `host_rev` | string, multi-valued | no | `site:` — every level of the host, reversed (`com`, `com.example`, `com.example.www`), so `site:` is a single term lookup that includes subdomains |
| `ext` | string | no | `ext:`, `filetype:` (from the URL path) |
| `sources` | string, multi-valued | yes | `common_crawl`, `hacker_news`, … |
| `title` | text | yes | `intitle:`, `allintitle:`, display |
| `body` | text | yes | Plain text; `intext:`, snippets |
| `links` | string, multi-valued | yes | Outgoing links; `link:` with a full URL |
| `link_hosts` | string, multi-valued | no | Every level of every linked host, reversed (like `host_rev`); `link:` with a site |
| `crawled` | date | yes | When the page was captured |
| `import_id` | u64, indexed | yes | The import this version of the page came from |

**Updates.** Tantivy can't modify a document in place, so a merge loads the stored document, applies the rules in §6.4, deletes the old one and adds the result. A merge that changes nothing writes nothing.

**Removing an import** deletes the documents whose `import_id` is that import. Since old versions are never kept, it can't bring back what the import replaced.

**Visibility during imports.** The importer commits periodically, so results appear while a large import is still running.

**Scale.** One index comfortably reaches the hundreds of millions of documents on a server disk. Billions-and-beyond will need several physical indexes searched together (and possibly federated over the LAN); documents can be spread across them by `key`, so the merge rules carry over unchanged.

### 6.3 App database (redb)

| Table | Key → Value |
|---|---|
| `imports` | import id → date, file name(s), file hashes, data source, status, pages read, new / updated / already present (duplicates) / repeated within the import, skipped, notes |
| `keys` | *Planned, for speed:* document `key` → content hash, length, crawl date — enough to decide most merges (§6.4) without loading the document. Until then the importer looks each key up in the index itself. |
| `trust` | public key (hex) → label, date trusted. Only trusted keys are listed; untrusting removes the row. |

redb allows only **one process** to open the file at a time. Rules:

- One-shot CLI searches never open redb.
- Other processes open it only when needed and close it promptly — except an import, which holds it for its whole duration.
- If a process finds it already open, it asks the process that holds it, over the local socket (§4.3).
- **Only one import or export runs at a time, and nothing ever waits for the database.** If another LOST process is importing or exporting, `lost -d`, `lost -e` and the GUI stop immediately with an error that says what's running, e.g. `Database busy: import of hn-2024.csv in progress since 14:02 (PID 1234)`, and the CLI exits with code 2. The running operation is described in a small lock file next to the database (operation, file, PID, start time), held with an OS file lock so a crashed process never leaves it stuck.
- Searches are never blocked by imports or exports.

### 6.4 Merge rules

When an import brings a page for a URL that already has a document:

1. **Same page** — the same capture (equal `crawled` times, e.g. a WET and a WAT record made from the same Common Crawl WARC file), or the same title with one body equal to or contained in the other → keep the fuller one and fill any of its missing fields (`title`, `links`, `crawled`) from the other. (An HN thread imported again with more comments replaces the older copy.)
2. **Different content** → the one with the **newer `crawled` date** wins.
3. **Crawl date unknown on either side** → the one with **more text** wins.

The loser is discarded, never archived. On a tie the existing version stays, so importing the same data twice changes nothing — the import reports those pages as duplicates ("already in the database").

**Size limit.** Text is cut at a word boundary beyond `MAX_BODY_BYTES` (4 MiB) per page. It's a constant in the source, not a setting, because it's a performance trade-off rather than a user preference — and because it's applied once, when a page comes in: a lower setting couldn't shorten pages already stored, nor a higher one bring back what was cut, and `.lost` files would differ by who made them. `lost-spider` uses the same constant: it stops reading an answer there, and cuts the text there too. It can be raised in a future release, or lowered if huge pages slow searches down.

**Import report.** Every import counts its pages as **new**, **updated** (replaced by a fuller or newer version), **already in the database** (duplicates) and **repeated** within the import itself, and says so when the same file was imported before (recognized by its BLAKE3 hash).

### 6.5 URL normalization

Merging only works if every source spells a URL the same way. The **key** is computed from a normalized URL; the **original** URL is kept for display.

- Lowercase scheme and host; remove default ports (`:80`, `:443`).
- `http`/`https` and `www.`/no `www.` are treated as the same page.
- Remove tracking parameters (`utm_*`, `fbclid`, `gclid`, `mc_cid`, `mc_eid`, `ref`, …); keep all other query parameters as they are.
- Remove a trailing slash from the path (except for the root `/`).
- **Fragments:** a plain anchor (`#section-2`) is never sent to the server, so it can't change what a crawler receives — removed. Route-style fragments used by JavaScript apps (`#!/…`, `#/…`) can change the content — kept.
- Percent-escapes are compared case-insensitively (`%d0` = `%D0`).
- Later: a page's own `<link rel="canonical">` (from WAT files), used instead of the crawled URL **only when it points to the same site** — otherwise any page could overwrite another site's document. It needs an alias table first, so that WET records (which don't carry the canonical link) still land on the same document as their WAT record.


### 6.6 Versions

Data written by one release of LOST must be readable by the next, or upgraded by it — never silently misread, and never thrown away without saying so.

- **Search index.** `index/lost-index.json` records the index's **layout** (`INDEX_LAYOUT` in `engine.rs`: its fields and what goes in them, §6.2) and the LOST that last opened it: `{"layout": 1, "written_by": "LOST 0.2.0"}`. Every release that changes the layout raises the number and adds an upgrade step from the layout before (`engine::upgrade`), done in place where possible — adding a field, say — or by rebuilding the index: from the capture archive once there is one (§6.7), otherwise by importing again (the message says so). An index from a **newer** LOST isn't touched; LOST says to update. An index made before stamps existed counts as layout 1. Tantivy's own file format is Tantivy's to keep compatible: upgrading Tantivy across a format break counts as a layout change.
- **App database.** redb tables are added, never repurposed: a new feature brings new tables (`captures`, say) and older ones stay as they are. A table whose rows have to change gets a new name, with an upgrade step that copies the rows over.
- **`.lost` files** carry their format version in the first bytes and a schema version for the page records (§9.1). New page fields are optional, and readers skip fields they don't know, so a file from a newer LOST still imports into an older one, minus what's new.

### 6.7 Capture archive (planned)

For when disks are big enough to keep the web itself, not just its text: LOST as a self-hosted Internet Archive. Nothing here is built yet; it's written down so that nothing built before it stands in its way.

**What it adds.** Every capture of every page, as fetched: HTTP status and headers, and the HTML (or whatever the response was), from WARC files (§8.6) and from future crawlers. Several captures of one URL are all kept, each with its time.

**What it doesn't change.** The search index stays as it is (§6.2): one document per URL, plain text, the newest capture by the merge rules. Raw pages never go into the index — its segments are rewritten as they merge, so every byte there is written several times over, and HTML is ten to twenty times the size of its text. Search stays as fast as it is.

**Layout.**

- `captures`, a redb table: (URL `key`, capture time) → HTTP status, content type, the content's BLAKE3 hash, its length, the import it came from, and where its bytes are. Ordered by key and then time, so all captures of a URL are one range read.
- The bytes, either:
  - **Left in the user's WARC files** (`archive.mode = "reference"`): (file, offset, length). A `.warc.gz` compresses each record separately, so one record can be read without unpacking the rest — this is how the Wayback Machine serves pages (its CDX index points into WARC files). Nothing is copied; the WARC files must stay where they were.
  - **Copied into LOST's own archive** (`archive.mode = "copy"`): `archive/` holds append-only pack files of zstd-compressed records, each stored **once per content hash** — crawls are full of identical pages (the same page fetched again, error pages, boilerplate). The `captures` row then holds (pack, offset, length). Packs are never rewritten in place; removing an import marks its records dead, and a later compaction rewrites packs that are mostly dead.
- `archive.max_size` caps it: past it, imports keep text only, as today.

**What it makes possible.**

- `cache:URL` shows the page as captured — its HTML, made safe (no scripts, no requests to the outside: images and styles come from the archive or not at all) — with a list of every capture of that URL by date, like the Wayback Machine's calendar. Plain text stays as the fallback.
- Rebuilding the search index from the archive when its layout changes (§6.6), or when the text extraction gets better, without the original files.
- `.lost` files that carry HTML: an optional `html` field per page (§9), so archives can be shared the way text is now.

**Removing an import** removes its captures (and, in copy mode, marks the records no other capture uses as dead). The search document of a URL is then rebuilt from its newest remaining capture, if any — unlike today, removing an import can bring back what it replaced.

**Scale.** The archive is append-only and addressed by hash, so it can be spread across disks or machines by hash, independently of the index (§6.2, *Scale*).

---

## 7. Search engine

Two modules, kept independent so either can be inspected, tested or replaced on its own:

- **`src/query.rs`** — parses the search box into an engine-agnostic syntax tree (terms, phrases, operators, boolean structure). No Tantivy types here.
- **`src/engine.rs`** — owns the Tantivy schema and turns the syntax tree into Tantivy queries, runs them, ranks, generates snippets and suggestions.

### 7.1 Operators

| Operator | Example | Implementation | Status |
|---|---|---|---|
| words | `catfish recipe` | All words required (AND), across title, URL and body | ✅ |
| `"…"` | `"fried catfish"` | Phrase query | ✅ |
| `-word` | `search engine -yandex` | Must-not clause | ✅ |
| `-operator:` | `yandex -site:yandex.com` | Negated operator | ✅ |
| `OR`, `\|` | `catfish OR tilapia` | Should clauses | ✅ |
| `AND` | `catfish AND recipe` | Explicit must (same as default) | ✅ |
| `( )` | `(catfish OR tilapia) recipe` | Grouping | ✅ |
| `*` | `"how to * catfish"` | Inside a phrase: terms placed at explicit positions with a gap. As on Google, `*` stands for one or more words: `n` stars in a row match `n` to `n + 2` words (all combinations tried, up to 9 per phrase). Leading and trailing `*` are ignored | ✅ |
| `site:` | `site:example.com` | Term lookup on `host_rev` (includes subdomains) | ✅ |
| `intitle:` | `intitle:catfish`, `intitle:"fried catfish"` | The word or phrase in `title` | ✅ |
| `inurl:` | `inurl:recipes` | … in `url_text` | ✅ |
| `intext:` | `intext:cornmeal` | … in `body` | ✅ |
| `allintitle:` / `allinurl:` / `allintext:` | `allintitle: fried catfish` | Every word after it goes to that field | ✅ |
| `filetype:` / `ext:` | `filetype:pdf` | Term lookup on `ext` (case and a leading dot don't matter) | ✅ |
| `link:` | `link:example.com`, `link:https://example.com/page` | A site (no path): pages linking anywhere on it, via `link_hosts`. A full URL: pages linking to exactly it, via `links`. Data from Common Crawl WAT files | ✅ |
| `cache:` | `cache:https://example.com/page` | Not a search: the stored text of that one page (§7.6) | ✅ |
| `related:` | `related:example.com` | More-like-this, seeded from that site's documents | later |

An operator can be written apart from its value (`intitle: catfish`); one with no value at all is ignored.

**Exclusions alone find nothing.** `-site:news.ycombinator.com` or `-spam -ads` on their own give no results, as on Google and DuckDuckGo — not "everything except".

### 7.2 Ranking

- BM25 with field boosts: title > URL > body.
- Later: a link-based authority score computed from the WAT link graph, stored as a fast field and blended in.

### 7.3 Snippets

Tantivy's snippet generator over `body`, which also reports where the matched words are, so front ends can highlight them.

### 7.4 Autocomplete

Offline means no "what other people searched". Suggestions come from the data itself. Each suggestion is a whole query, ready to search.

- **Completing the word being typed:** a prefix scan of the title and text term dictionaries (FSTs, so it's fast), most common words first. When other words come before it, the 20 most common completions are re-ranked by how many pages contain them *together with* those words, and completions that never appear with them are dropped. So `rust comp` offers `rust compiler` before `rust company`.
- **`_` as a wildcard**, like Google: `how to _ catfish` finds pages with `how to` and `catfish` one word apart (titles and text), and offers the words that actually fill the gap there, most frequent first.
- Nothing is suggested once the last word is finished (followed by a space), or when it's an operator, an exclusion or a phrase.

Available as `lost --suggest TEXT` (one suggestion per line), and in step 4 as `/api/suggest` for the web UI and the GUI.

### 7.5 No date operators

`before:` and `after:` are **not supported**. LOST searches the web as it is, not an archive of articles, and most pages carry no trustworthy publication date — guessing one from meta tags, headers or URLs gives vague, unreliable answers. A `published` field may come back one day, but only for sources with precise timestamps.

### 7.6 `cache:` — a page's stored text

`cache:URL` shows one page as LOST keeps it: title, URL, capture date, and the full plain text (the WET text for Common Crawl, the story and comments for Hacker News). It's "better than nothing" when the site is gone or offline, like a text-mode browser: the HTML (WARC) was never stored, and no outgoing links are shown.

- **A lookup, not a search.** The URL is normalized as on import (§6.5), hashed to the document key and read directly; nothing else is tried. URLs that normalize the same (`http`/`https`, `www.`, a trailing slash, tracking parameters) are one page in LOST, so they find the same copy. A URL without a scheme is taken as `https://`.
- It works on its own: `cache:` with other words, or with nothing after it, is an error that says so.
- **The "Cached" link** after each result's address is off by default: `search.cached_links = true` (Settings → Configuration → Search, *Show “Cached” links*) shows it in the web UI — with and without JavaScript: the server marks the page `<html class="cached-links">`, and the stylesheet hides the links otherwise — and in the app. Never on the command line. Typing `cache:URL`, and the app's right-click "Show cached text", work either way.
- Opened from a result (the "Cached" link, or the app's "Show cached text"), the words of that search are marked in the text, with the same tokenizer as everywhere else.
- **CLI:** `lost cache:URL` prints the title, URL, "Captured DATE · sources", a blank line, then the text. On a terminal, long lines are wrapped at word boundaries (it's for reading, so nothing is cut); to a file or pipe the text is written as stored. `-f json` gives the Web API's object; `-o` works; `-l` is an error. Exit code 1 if the page isn't stored.
- **Web API:** `GET /api/page?url=URL[&hl=QUERY]` (§10.2).
- **The reading view** (web UI, no-JavaScript pages, app): the title, URL and capture date sit in a box of their own — darker, with a border — above the text, so they don't read as part of the page. Where the page came from (its sources) isn't shown there: imports and data sources belong to Settings and the command line.
- **Web UI:** typing `cache:URL`, opening `/?q=cache:URL[&hl=QUERY]`, or clicking a result's "Cached" link shows a reading view in place of the results. Back returns to them. Not stored: "This page isn't in the database." with a link to search `site:` its host.
- **App:** `cache:URL` in a tab shows the same reading view; a result's "Cached" link (after its address, as on the web) and its right-click menu's "Show cached text" open it in a new tab.

---

## 8. Data sources

```
src/datasources.rs          shared logic: detection, record pipeline, helpers
src/datasources/
├── lost.rs                 .lost files (our own format, §9)
├── common_crawl.rs         Common Crawl WET + WAT (never WARC)
└── hacker_news.rs          Hacker News, BigQuery CSV export
```

### 8.1 Contract

Each parser provides:

1. **Detection** — given the first bytes of a file (and its name), say whether it can parse it, with a confidence. `datasources.rs` asks every parser and picks the best match, so users never have to say what a file is. Gzip and zstd are unwrapped before detection.
2. **Parsing** — turn the file into a stream of `Record`s (§8.5).

`datasources.rs` provides everything common: decompression, URL normalization, HTML-to-text cleanup, date parsing, blocklist filtering, batching into the index, dedup bookkeeping, progress reporting, and writing the `imports` row at the end.

### 8.2 Import flow

```
file(s) ─► detect ─► parser ─► Record stream ─► blocklist ─► dedup / merge ─► Tantivy writer
                                                                      │
                                                         redb: keys, imports row
```

One `lost -d` (or one drop in the GUI) can take many files, and each becomes **its own import**, with its own row in `imports` and its own report — except where files only make sense together: the WET and WAT files made from one Common Crawl WARC file (`…-00000.warc.wet.gz` and `…-00000.warc.wat.gz`: one has the text, the other titles and links, merged into the same pages; the WARC itself, `…-00000.warc.gz`, would join them if LOST read WARC files) are one import, and all the Hacker News CSVs are one import (a story and its comments may be in different files). A page belongs to exactly one import, so imports split any finer couldn't be removed cleanly.

Files named directly must be datasets LOST can read; otherwise the import stops with an error. **Files found inside a folder that LOST can't read are skipped**, and the import's notes list them — so `lost -d /media/usb/` imports the CSV files and ignores everything else there. A file that turns out to be broken partway through keeps what was read before the problem, and the notes say where it stopped.

Entry points: `lost -d PATH…` on the CLI, a file picker / drag-and-drop area in the GUI, or the `import` IPC command. **Importing is never exposed through the Web API.**

When finished, a row is added to `imports`: date, file name, data source, record count, and the number of new records absorbed. An import that's interrupted (Ctrl+C, a crash, a full disk) keeps what it had already committed and is recorded as **interrupted**; importing the same file again completes it, since the merge rules (§6.4) make re-importing records harmless.

### 8.3 Common Crawl (v1)

WARC files, plain or gzip-compressed (Common Crawl's `.warc.wet.gz` / `.warc.wat.gz` are read as they are, no need to unpack them). A cut-off file (e.g. a partial download) keeps every complete record and says where it ended.

- **WET** (`WARC-Type: conversion`): the plain text of each page. Common Crawl puts the page's `<title>` on the first line, which becomes the title (when it's 300 characters or less); the rest is the body. `WARC-Target-URI` gives the URL, `WARC-Date` the capture time (`crawled`).
- **WAT** (`WARC-Type: metadata`, JSON): only records describing a successful (2xx) HTML response are used. They give the title, the meta description (used as the body until the page's full text arrives), and the page's outgoing links (`<a href>` only, resolved to absolute `http(s)` URLs, at most 500 per page; for `link:`).
- A WET and a WAT file made from the **same WARC file** describe the same captures — identical capture times — so their records merge into one page (§6.4): the WET text, plus the WAT title and links. They can be imported together (`lost -d folder/`) or one after the other, in either order. Either can be imported alone.
- WARC files (full HTTP responses) are not supported: too large.
- Test data: the start of any WET/WAT file from a recent crawl on `data.commoncrawl.org` (free, no account), e.g. `curl -s URL | zcat | head -c 5000000 > sample.wet`.

### 8.4 Hacker News (v1, implemented first)

Input: CSV exported from BigQuery's public dataset `bigquery-public-data.hacker_news.full`.
Columns are matched by header name, so order doesn't matter: `id`, `type`, `by`, `time`/`timestamp`, `title`, `url`, `text`, `parent`, `score`, `descendants`, `dead`, `deleted`.
BigQuery splits large exports into several files, so the parser accepts a folder or several files as one import.
The full table is big; filtering in BigQuery before exporting (by year, by score) keeps test files small.

Mapping — every live story, job and poll becomes **one page, its HN item page**:

- `url`: `https://news.ycombinator.com/item?id=ID`. So `site:news.ycombinator.com` finds HN pages, and `site:` of the linked site doesn't (the article itself isn't in this data).
- `title`: the story title.
- `body`, plain text, in this order: `Title (example.com)` for link submissions (the linked site's host, without `www.`, as HN shows it; the full link is in `links`), the story's own text (if any), `Submitted by AUTHOR`, then every comment as `AUTHOR: text`, in thread order.
- `links`: the linked URL, if any.
- `dead` and `deleted` items are skipped. HN text is HTML-escaped markup; it's converted to plain text.

Comments only record their direct parent, not the story they belong to, so the import runs in passes over a temporary redb file on disk (the full dataset has tens of millions of comments, too many to hold in memory):

1. Read every item; record each comment's parent and keep the text of live comments.
2. Walk each comment up its parent chain to the root story. Replies to dead or deleted comments attach to the nearest live comment above them.
3. Emit each story as a page with its whole discussion (replies follow their parent; siblings in posting order).

Comments whose story isn't in the import (e.g. at the edges of a time slice, or in another file of a split export imported separately) are skipped, and the import's notes say how many. Import all files of one export together (`lost -d folder/`) so every comment finds its story.

Very large threads are cut at `MAX_BODY_BYTES` (§6.4).

### 8.5 Record

The common shape every parser produces:

| Field | Required |
|---|---|
| `url` | yes |
| `source` | yes |
| `title`, `body`, `links`, `crawled` | no |

### 8.6 Later

- **Common Crawl WARC files** — the pages as fetched (`…-00000.warc.gz`, several times the size of the WET and WAT made from them). Text, title and links come from the HTML itself, so the WET and WAT aren't needed; the raw responses go to the capture archive (§6.7). A WARC file and its WET and WAT are one import (§8.2).
- **Gemini** — crawled with `lost-spider gemini` (§15), which saves `.lost` files; dumps from other crawlers later.
- **Gopher** — crawled with `lost-spider gopher` (§15.5).
- **.onion** and **I2P** sites — crawled with `lost-spider onion` and `lost-spider i2p` (§15.6); dumps later.
- **I2P** — the XL-I2P CSV dataset from IEEE DataPort, once obtained.
- More web dumps as they turn up.

### 8.7 Sample pages

`res/data/sample.lost` (two pages: the front pages of Hacker News and Slashdot) is built into the `lost` program (`include_bytes!` in `main.rs`, handed to `cli::run`; the library doesn't carry it). When the program opens a data folder that has no index yet — the first time ever — it imports the sample as import #1 (source LOST, file `sample.lost`), with no trust question. Removing that import removes the pages for good; it only comes back with a new, empty data folder.

---

## 9. The `.lost` dataset format

A portable file for sharing crawl data between LOST users — over BitTorrent, S3, the LAN or a USB stick. Not tied to Tantivy's internal format, so it survives Tantivy upgrades. Reader, writer and signing key: `src/lostfile.rs`; importing: `src/datasources/lost.rs`.

### 9.1 Layout

```
┌───────────────────────────────────────────────────────────────┐
│ "LOST" · format version (1 byte, now 1) · flags (1 byte:      │
│   bit 0 = signed) · header length (u32 LE)                    │
│ header (JSON): created, generator, query (if exported with    │
│   one), signer public key (hex, if signed), schema version    │
├───────────────────────────────────────────────────────────────┤
│ chunk 1: one zstd frame of JSON Lines, one page a line        │
│ chunk 2: …                                                    │
│ …                                                             │
├───────────────────────────────────────────────────────────────┤
│ footer (JSON): page count; per chunk: offset, size, page      │
│   count, BLAKE3 of its compressed bytes; signature (hex)      │
│ footer length (u32 LE) · "TSOL"                               │
└───────────────────────────────────────────────────────────────┘
```

- A page line: `{"url", "sources": [...], "title", "body", "links": [...], "crawled"}` — `url` required, the rest optional; unknown fields are ignored, so later versions can add some. Pages that name no source get `lost`.
- Chunks close at 8 MB of JSON or 10,000 pages, compressed at zstd level 7. They're independent: a damaged chunk is skipped on its own (the import's notes say how many pages that cost), and chunks are decoded in parallel on import — checked, decompressed and parsed on up to 8 threads, while one thread reads them in order and another hands their pages to the index in order. At most twice as many chunks as threads are in memory at once, so a file of any size imports in bounded memory, and the result is the same as reading it chunk by chunk.
- The footer comes last, so the file is written in one pass. A file that doesn't end in a footer and `TSOL` is reported as incomplete ("was the download cut short?").
- Opening a file checks the layout: chunks fill the space between header and footer, in order, and their page counts add up.
- Limits for files from elsewhere: header 1 MB, footer 64 MB, chunk 256 MB packed and 1 GB unpacked.
- Exports are written to `NAME.part` next to the target and renamed when complete; a failed or cancelled export leaves nothing behind. `lost-spider` writes straight into its file instead, keeping it complete as it grows (§9.4).

### 9.2 Signing, trust and anonymity

- **Your ID** is an ed25519 key pair made on the first signed export (`signing.key` in the per-user folder, §6.1, mode 0600). The public key is the identity — a pseudonym, never a name — shown as its first 16 hex digits: `3f9a 1c2e 77b0 1d4e`. Settings → Configuration shows it under *Require signature*, with *New ID…*; `lost --new-id` does the same. A new ID replaces the key for good (written next to it, then moved over it). The old ID is added to the trusted people ("your earlier ID"), so files signed with it still import without a question; files already shared keep showing the old ID, and people who trust it are asked about files with the new one.
- **Exports are signed by default.** The GUI has a "Sign it" checkbox, ticked every time (the choice isn't remembered); the CLI has `--unsigned`.
- The signature covers a fixed context string, the BLAKE3 hash of the header, and every chunk's offset, size, page count and hash. So a changed byte is caught wherever it is: in the header or footer when the file is opened ("changed after it was signed … not imported"), in a chunk when that chunk's hash doesn't match.
- **Importing:** files signed by you, or by someone in the `trust` table, are imported without a question. For the others, `import.trust` decides:

  | `import.trust` | Unsigned file | Signed by someone new |
  |---|---|---|
  | `ask` (default) | ask | ask |
  | `signed-only` | refused | ask |
  | `any` | import | import |

- **Asking:** the GUI asks in a small window over Settings ("Import “data.lost”? It has 50,000 pages, signed by 3f9a 1c2e 77b0 1d4e. You haven't imported a file from them before."): *Import*, *Import and trust them*, *Don't import* (also Esc). The CLI asks on the terminal (`Import data.lost (50,000 pages), signed by 3f9a 1c2e 77b0 1d4e (not trusted yet)? [y/N]`, then `Trust 3f9a 1c2e 77b0 1d4e from now on? [y/N]`). Without a terminal (scripts), the answer is no, with a message saying how to import anyway.
- Questions come before the busy lock is taken, so nothing is held while the user thinks. Saying no to everything imports nothing and isn't an error (CLI exit code 1).
- Trusted people are listed in Settings → Configuration, each with a Remove button, and by `lost --trusted` (`lost --untrust ID` removes one).
- Any import can be removed afterwards (§6.2), so a bad dataset is never permanent.
- A blocklist filters records at import time. **After v1**; draft format, to be discussed then:

  ```
  # comment
  casino.example                 # domain, including all subdomains
  https://example.net/spam/      # URL prefix
  /\b(casino|viagra)\b/          # regular expression on the URL
  title:/free bitcoin/           # regular expression on the title
  body:/some phrase/             # regular expression on the text
  0.0.0.0 tracker.example        # hosts-file lines work too, so existing blocklists can be reused
  ```
- Darknet dumps in particular can contain illegal material; the blocklist hook exists so users can filter it before it ever reaches their index.

### 9.3 Export

- `lost -e FILE` exports every page; `lost -e FILE QUERY…` (or `-i`) only the pages the search finds, with all operators. The query is kept in the header.
- The GUI's Settings → Data has an Export section under the table of past imports: the "Sign it" checkbox and *Export all* — or, with imports ticked in the table, *Export selected* (their pages, in one file; `Selection::Imports`). It asks where to save (`lost-pages-2026-10-10-signed.lost`, or without `-signed` when it isn't), showing a green progress bar under the section's title while it runs, then the result. The command line exports everything or a search's pages.
- Pages are written in index order (not ranked), as stored: URL, sources, title, text, links, crawl date. Import ids aren't exported.
- Exporting takes the busy lock (§6.3), so it never runs alongside an import.
- A fresh install has two sample pages to search (§8.7).

### 9.4 Writing as it goes, and continuing

For `lost-spider` (§15), whose files grow for hours and must survive being stopped (`Writer::checkpoint`, `Writer::append`):

- **Checkpoints.** After every chunk, and at least every 5 minutes (closing a smaller chunk), the footer and signature are written after the chunks so far, so the file is complete as it stands: it can be imported, or continued. The next chunk is written over that footer. Only a crash between writing a chunk and its new footer — a moment — leaves the file incomplete.
- **Continuing.** The footer is read and the file cut where it starts; new chunks go after the old ones, and the new footer lists them all. The header keeps its `created`, `generator` and `query`; its `signer` and the signed flag become whoever continues it (or unsigned), and the new signature covers every chunk, old ones included. Continuing someone's file makes it yours, as importing and re-exporting their pages would. If the header changes size (an unsigned file signed now, or the other way round), the file is written again as `NAME.part`, copied back over the old one through the handle that holds its lock, and the part file removed (if the copy fails, the part file is a complete copy and the error says where); the file is never replaced by another, so nothing else can slip in between reading and writing it. Otherwise nothing moves. A damaged chunk is left as it is.
- A file being written is locked (an OS file lock, gone when the program ends), so two programs can't write it at once.
- Nothing about a crawl is stored in the file besides its pages: the next run works out the rest from them (§15.3).

---

## 10. Web server

### 10.1 Behaviour

- `lost -s` serves on `127.0.0.1:1057` by default; `-a` and `-p` change the address and port.
- `lost -b` starts `lost -s` again as a detached process in its own session (so closing the terminal doesn't stop it), checks that it came up, prints its PID and returns. Its messages go to `server.log` in the data folder; stop it with `kill PID`. Unix only; on Windows it exits with a message (a Windows service can come later).
- A port that's already taken (say, by another LOST server) is reported as such. The local-socket single-instance rules (§4) come with the GUI (step 5), which needs them to raise its window.
- Ctrl+C or SIGTERM stops the server cleanly.
- **Queries are never logged.** The server prints only its address on start and a line when it stops.
- **systemd and Docker run `lost -s` in the foreground**, not `-b`. A unit file ships in `dist/lost.service`.
- All web assets are compiled into the binary (`include_str!`), so the server is a single file.
- Every response carries a strict Content-Security-Policy (scripts, styles and requests only from the LOST server itself; no framing), `Referrer-Policy: no-referrer` and `X-Content-Type-Options: nosniff`, so clicking a result tells the site nothing about the search.

### 10.2 Web API

| Endpoint | Purpose |
|---|---|
| `GET /api/find?q=QUERY&l=[OFFSET,]COUNT` | Search; `l` has the same SQL semantics as the CLI's `-l` (default `search.default_limit`, at most `server.max_results`). A bad `l` gives `400` with `{"error": …}` |
| `GET /api/suggest?q=TEXT` | Autocomplete (§7.4): a JSON array of up to 8 whole-query suggestions |
| `GET /api/page?url=URL&hl=QUERY` | One page's stored text (`cache:`, §7.6): `{"url", "title", "sources", "crawled", "text", "title_matches", "text_matches"}`, the matches being QUERY's words (`hl` is optional). `404` with `{"error": …}` if it isn't stored |
| `GET /api/status` | `{"version": …, "documents": …}` |

`/api/find` response (also the CLI's `-f json` output):

```json
{
  "query": "catfish recipe",
  "offset": 0,
  "count": 50,
  "total": 1234,
  "took_ms": 12,
  "results": [
    {
      "url": "https://example.com/recipes/fried-catfish",
      "title": "Southern Fried Catfish",
      "snippet": "…dredge the fillets in seasoned cornmeal…",
      "sources": ["common_crawl"],
      "crawled": "2025-02-14",
      "title_matches": [[21, 28]],
      "snippet_matches": [[49, 56]]
    }
  ]
}
```

`title_matches` and `snippet_matches` say where the query's words appear, as `[start, end)` offsets in **characters** (Unicode code points), so every front end highlights the same words: the terminal in colour, the web UI with `<mark>`, the GUI with styled text. They're left out when empty. The positions are computed by the engine with the same tokenizer that built the index, for every word and phrase that isn't excluded.

`total` may be an estimate for very large result sets. No CORS headers by default. No import endpoint.

### 10.3 Web UI

- Served at `/`. A single page that never reloads: vanilla JS with **Mikado** for rendering.
- Looks and feels like DuckDuckGo: a centred start page, then a header bar with results below. Searches as you type (after a short pause), with suggestions and infinite scrolling (20 results per request, loaded via `l` offsets as you scroll). `/` focuses the search box.
- Dark by default, light when the system prefers it. Works at phone width.
- No page changes, only transformations: when results appear, the logo and search box glide from the start page into the header bar (logo shrinking into the top-left corner) while the results fade in; clicking the logo (or going Back to `/`) plays it in reverse. About 0.3 s, done with the Web Animations API by measuring each element before and after the layout switch (FLIP), so an interrupted change continues from wherever things are. A link straight to results opens on them without animating; with *reduce motion* set in the system, nothing animates.
- `/` is the start page and `/?q=…` the results page, even with an empty query: emptying the box keeps the results layout and `/?q=`, with nothing below the bar. Only the logo (or Back to `/`) returns to the start page.
- **Suggestions** are pills on one line beside the search box, or, in narrower windows, on a line of their own under it, starting where the box does, that's always there, even empty, so nothing moves. Pills that don't fit aren't shown (they wrap onto a hidden second line); they never cover the results, and they're hidden while the search box isn't in use. ↓/↑ pick one, Enter or a click searches it (and Ctrl+Z takes it back), Esc lets go. None on the start page: they begin once it has turned into results. Results line up with the search box whatever the logo's size.
- Once scrolled about 100 px (even with a single page), a round grey "back to the top" button fades in at the bottom right, so there's always a sign of where you are (macOS hides idle scroll bars); it scrolls smoothly to the top (animated by `lost.js` rather than the browser's smooth scrolling, which can stop short when results arrive mid-way; scrolling by hand takes over). A ring around it, halfway in colour between the button and its arrow, is the scroll bar's position: drawn clockwise from 12 o'clock, full only at the very bottom. The dash lengths are worked out in `lost.js` from the circle's own radius, not with SVG `pathLength`/`non-scaling-stroke`, which Safari applies differently.
- **Room for the rest:** below the loaded results, empty space stands in for those not loaded yet (their count times the average height of a result so far), so the scroll bar shows the whole list from the start and barely moves as results arrive. Dragging it far down loads everything up to there, in requests of up to 100, with the dots held in the middle of the window meanwhile; the page number in the URL follows the estimate. Room is kept for at most 5,000 results (250 pages); past that the list simply grows, and the ring counts the remaining results as if they were there.
- The query is kept in the URL (`?q=`) so searches can be bookmarked and shared; Back and Forward move between searches.
- **Pages:** results are numbered in pages of 20. While scrolling, the URL says which page is at the top of the window (`?q=…&p=4`; no `p` for page 1), updated in place so scrolling adds no history entries. Opening such a link loads pages 1 to 4 (in requests of up to 100 results) and scrolls to page 4, so scrolling up still works; a `p` past the end opens on the last page; `p` is capped at 250. Back to an earlier search returns to the page it was left on. The pages without JavaScript use the same `p`, so links work in both.
- **Loading:** three bouncing dots, where the results will appear while a search runs (any previous results dimmed below them), and below the results while more load. They appear only after 0.15 s, so quick answers don't flash them; with *reduce motion* they pulse instead of bouncing.
- Loads nothing from outside the LOST server — no CDNs, no web fonts. Result links use `rel="noopener noreferrer"`.
- Matches are highlighted with `<mark>`, placed from `title_matches`/`snippet_matches` into text that's HTML-escaped first, so page text can never inject markup. Only `http(s)`, `gemini`, `gopher` and `ftp` links are clickable.
- Mikado templates are written by hand in `lost.js` (the light build has no template compiler).
- Kept plain on purpose: it's a search engine, so less is more. (It grew from the dark stylesheet of the earlier C++ prototype.)
- **Without JavaScript** (`src/html.rs`): the JavaScript UI never asks the server for `/?q=…` (it changes the address itself), so a real request for it — a form sent without JavaScript, Lynx or w3m, a link opened in a new tab, a reload — gets a page of plain HTML and the same stylesheet, with no scripts at all: the bar (logo, search box, Find button; no suggestions), then that page's 20 results, each with its "Cached" link, and under them, when there's more than one page, "Looooost" as on Google: an "o" for each of up to ten pages around this one (its number under it, the current one in green), "L" for the previous page and "st" for the next. `/?q=cache:URL&hl=…` gives the stored page's text the same way. A stored page that isn't there answers `404`, a bad `cache:` `400`. The start page (`/`) is the JavaScript UI's, whose form has a Find button inside `<noscript>`, so it only shows without JavaScript. Everything from the database is escaped; `<mark>` is placed from the engine's ranges.

---

## 11. GUI

Built with egui (`eframe` 0.36, glow renderer rather than the default wgpu: a smaller, quicker build), in `src/gui.rs`. eframe calls the app's `logic` before every frame and also while the window is hidden or minimized, when nothing is drawn; events from the tray, other processes and background threads are handled there, so they work whatever the window's state. egui's own Ctrl+Q is turned off: LOST's asks first while an import or export runs.

- **Main window:** the tab bar, the search box, and the current tab's results. Nothing else, so searching stays uncluttered.
  - **Tabs** are rounded rectangles (the same corners as buttons) sharing the bar's width equally on one line (they get as narrow as it takes, never wrapping), each with its × inside (none on a lone empty tab: there's always one), titled with the query and its result count: `catfish (8)`; the count stays while more results load. "+" sits at the right, beside ☰. Tabs live in memory only: quitting, a crash or a power cut forgets them, so searches stay quick, throwaway actions rather than something to keep.
  - **Suggestions** are pills on one line beside the search box, or, in a narrower window, on a line of their own under it that's always there (so nothing moves as they come and go). Only as many as fit are shown; they never wrap or cover the results, and they're hidden while the search box isn't in use. ↓/↑ pick one, Enter searches it.
  - **Results:** searched as you type (after 0.2 s), matches highlighted, 20 more whenever the end of the list comes near. Every result is drawn at the same height (a one-line title, a snippet of up to three lines, the URL, cut with "…"), so only those in view are drawn and long lists scroll smoothly. Clicking a title opens the page in the default browser; right-click to copy the link or show its stored text (`cache:`, §7.6) in a new tab. No tooltips on results.
- **Hamburger menu** (☰) at the right end of the tab bar: Settings, About, Quit.
- **Settings window** (wide enough for every column of past imports, its least width too), two pages, as tabs like the main window's, half the width each:
  - **Configuration:** the settings in `lost.toml` as a form (number fields, text fields with a folder picker — the data directory shows its default path when empty — a drop-down, a checkbox), grouped as Search, Web server and Desktop app on the left; Storage and Import (*Require signature*: Always, Never, Ask every time; *Your ID* with *New ID…*; and the people you trust, each with a Remove button that asks first in a small window of its own — "Stop trusting 3f9a 1c2e 77b0 1d4e?") on the right. There's no Save button: a change is saved as soon as nothing is being typed or dragged (a field loses the keyboard, a number is let go, a box is ticked), writing the user's `lost.toml` whole, every setting with a comment, after checking it (if that fails, the reason shows at the top). The search and import settings apply at once; the web server's, the data directory and the tray icon take effect on a restart, and each says so under it ("Takes effect after LOST restarts.") while what's saved differs from what LOST started with — set it back and the note goes.
  - **Data:** how many pages are in the database and how much disk they take ("There are currently 1,234 pages in the database, taking up 126 MB": the index and the app database, in powers of 1,000); **Import**, as wide as the window (drop files or folders, or pick them; a line under the buttons says what went wrong, if anything); **Past imports**, a table as wide as the window: a checkbox, *File* (taking the width left; a page icon, 📄, then the file names, cut to fit — all of them and the import's notes on hover), *Type* (the data source), *Imported on*, *Page count* ("21,528 (6,247 new, replaced or updated 15,036)"; "517 (all new)", "517 (no new)", the replaced part left out when none were) and *Status*: a thin green bar. The imports of a run appear in the table as soon as the files are sorted into imports (§8.2), each with its bar growing as its files are read (`datasources::bytes_read`, against their size); they can't be ticked until the run is over. A finished import's bar is full; a failed one says *failed*, an interrupted one *interrupted*. The details — what started, counts, notes, errors — go to stderr, for anyone looking into a problem from a terminal. **Remove**, above the table at the right, works on the ticked imports after asking in a small window of its own ("Remove “hn-2024.csv”? Its 1,234 pages will be removed from the database.", or "Remove 3 imports?"): it deletes every page whose current version came from them — pages they updated too, since earlier versions aren't kept; pages a later import has updated since stay — and their rows. It takes the busy lock like an import. **Export**, under the table and as wide as the window, saves everything or the ticked imports (§9.3). A `.lost` file from someone not trusted yet is asked about in a small window of its own (§9.2).
- File dialogs (rfd) run on a thread of their own, so the windows keep drawing while one is open; one at a time, the buttons that open them are disabled meanwhile.
- Dates and times are shown in local time (via `jiff`, from the system's time zone); everything stored is UTC. An export shows its progress as a percentage ("Progress: 15.5%").
- While the Settings window is open, what it shows is re-read every 3 seconds (past imports, the database's size, who's trusted), so imports from the command line show up by themselves. There's no Refresh button.
- **About:** a small window of its own: version, the web UI's address with `-w`, licence.
- **Tray icon** with a menu: Hide (or Show, when hidden), Quick search, New search, Settings, About, Quit. On Linux and BSD it's `ksni` (StatusNotifierItem over D-Bus: KDE, XFCE, Cinnamon, LXQt, and GNOME with the AppIndicator extension, as on Ubuntu), so no GTK is needed; on Windows and macOS, `tray-icon`. Clicking the icon shows or hides the window (Linux, Windows). Where no tray is available, closing the window quits.
- **Quick search popup** (`src/quick.rs`): a small borderless window, always on top, with a search box and the first 8 results (title and URL, matches highlighted). While the main window shows, it's a window of the app's own, drawn along with the main window. While the main window is hidden (in the tray, or minimized: that sends it to the tray first), eframe can't make or draw another window — it stopped with "the user callback was never called" — so the main window itself becomes the popup: borderless, always on top, at the popup's size and place, showing only the popup; when it closes, the main window gets its decorations, size and place back and is hidden again (or shown, for Ctrl+Enter). The same goes for Settings and About opened from the tray while the main window is hidden: the main window stands in for them, with their title and size (`State::begin_stand_in`); showing the main window gives them windows of their own again. Why it's done this way, from eframe 0.36's code: while a window is off screen (hidden, minimized, or shown a moment ago and not mapped yet), eframe still runs and draws it now and then — from a place in its event loop where it can't create windows, so a new window of the app's own fails there and egui panics ("the user callback was never called"); LOST skips such a frame and draws the window with the next one (`gui::separate`, which also keeps that panic's message quiet). Drawing a window that isn't on screen also mustn't wait for the screen, so vsync is off (`glow_options.vsync = false`; with it, the main window as the popup froze). And the popup's search box takes the keyboard only once the window has the focus: asking for text input earlier, on a window not yet on screen, can hang under XWayland. Last, the main window's size and place are kept by LOST itself, in `window.txt` in the data folder, not by eframe — which would also have kept the popup's size, had LOST stopped while the main window stood in for it. As a process of its own, it froze under XWayland: its first X11 requests never got an answer. ↓/↑ pick one, Enter (or a click) opens it in the browser, Ctrl+Enter shows the search in a new tab of the main window, Esc or clicking elsewhere closes it.
  - Windows and macOS: a left click on the tray icon opens it right under (or over) the icon; a right click opens the menu.
  - Linux and BSD: most trays (StatusNotifierItem) don't say where the icon is, so it opens from the tray menu's "Quick search" item, near the top of the screen. (A global shortcut would need X11-only key grabbing; not done.)
- **Closing the main window** hides it to the tray; with `-t` (or no tray) it quits. Wayland lets no app hide, show or focus its own window, so on Linux and BSD the window runs through XWayland (X11) whenever it's there (`LOST_WAYLAND=1` keeps it on Wayland, without hiding).
- **Quitting asks for confirmation while an import or export is running** (from the window, `Ctrl+Q` or the tray menu), saying what will be interrupted. Otherwise it quits right away.
- Only the window's size and position are remembered between runs, in `gui.ron` in the data folder (eframe's own storage, so an import running in another process, which holds the app database, never blocks it).
- Dropping files or folders on either window imports them and opens Settings → Data to show the progress.
- Settings → Data: nothing changes size while an import, export or removal runs. Progress shows in the table's bars and the Export section's bar; the details go to stderr.
- The file dialogs open where the last of their kind left off: Choose files… and Choose a folder… in the folder the last import came from (dropped files count too), the export's in the folder the last export went to. Kept in `gui.ron` with the window's size, so it lasts between runs.
- `lost -w` runs the web server on a thread of the app's process (holding both roles, §4.1); About shows its address, or why it stopped. Ctrl+C (or SIGTERM) in its terminal stops both: the server stops, then tells the app to quit (which asks first if an import or export is running).

Keyboard shortcuts:

| Shortcut | Action |
|---|---|
| `Ctrl+F`, `F11` | Toggle fullscreen |
| `Esc` | Leave fullscreen, or hide to tray |
| `Ctrl+Q` | Quit |
| `Ctrl+T` | New tab |
| `Ctrl+W` | Close tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |

(The first three are carried over from the C++ prototype; the tab shortcuts are new.)

---

## 12. Configuration

`lost.toml` (all keys optional):

```toml
[server]
address = "127.0.0.1"
port = 1057
max_results = 1000        # largest COUNT the API will return at once

[storage]
data_dir = "/var/lib/lost"

[search]
default_limit = 50
cached_links = false      # a "Cached" link after each result's address (web UI, app)

[import]
trust = "ask"             # "ask" | "signed-only" | "any" (§9.2)

[gui]
tray = true
```

A commented example ships in `dist/lost.toml`.

---

## 13. Source layout

```
lost/
├── ARCHITECTURE.md
├── Cargo.toml
├── LICENSE
├── README.md
├── dist/
│   ├── lost.desktop          Linux desktop entry
│   ├── lost.service          systemd unit
│   └── lost.toml             example config
├── res/
│   ├── images/               icon
│   └── web/                  web UI assets (embedded into the binary)
└── src/
    ├── lib.rs                library entry point and public API
    ├── main.rs               binary entry point            [cli]
    ├── args.rs               argument parsing               [cli]
    ├── cli.rs                mode dispatch                  [cli]
    ├── output.rs             plain / json result output     [cli]
    ├── instance.rs           single instance, local-socket IPC [cli]
    ├── config.rs             lost.toml, platform paths
    ├── text.rs               HTML to text, truncation, dates
    ├── warc.rs               WARC record reader (WET, WAT)
    ├── urls.rs               URL normalization, site:/ext: fields
    ├── record.rs             records, documents, merge rules (§6.4)
    ├── query.rs              search syntax parser
    ├── engine.rs             search engine (Tantivy)
    ├── store.rs              app database (redb)
    ├── lostfile.rs           .lost format reader / writer
    ├── datasources.rs        data source detection and shared helpers
    ├── datasources/
    │   ├── lost.rs
    │   ├── common_crawl.rs
    │   └── hacker_news.rs
    ├── server.rs             web server and Web API         [server]
    ├── daemon.rs             -b background mode (Unix)      [server]
    ├── gui.rs                egui GUI and tray              [gui]
    └── spider/               lost-spider, its own program   [spider]
        ├── main.rs           command line, the output file, Ctrl-C
        ├── crawl.rs          queue, politeness, robots.txt, blacklist
        ├── net.rs            connecting: straight, SOCKS5, HTTP proxies; TLS
        ├── http.rs           HTTP/1.1 and HTML, for the web networks
        ├── gemini.rs         Gemini: TLS, responses, gemtext
        ├── gopher.rs         Gopher: selectors, menus
        ├── onion.rs          .onion sites through Tor; Ahmia's banned list
        └── i2p.rs            I2P sites through a router; address books
```

### 13.1 Library API (sketch)

The same surface every front end uses, and what other programs embed:

```rust
let lost = lost::Lost::open(data_dir)?;
let results = lost.search("catfish recipe", lost::Limit::new(0, 50))?;
let suggestions = lost.suggest("catf")?;
let report = lost.import(&["hn-2024.csv"], lost::Trust::Ask, ask_callback, progress_callback)?;
let (identity, _) = lost::Identity::load_or_create(&key_dir)?;
lost.export("my-crawl.lost", Some("site:example.com"), Some(&identity), progress_callback)?;
lost.trusted()?; lost.untrust(key)?;
lost.remove_import(import_id)?;
```

---

## 14. Roadmap

### v1

1. ✅ **Core vertical slice.** `Record`, Tantivy schema, basic query parsing (words, phrases, `-`, `OR`, `site:`), Hacker News import, redb `imports`, busy lock, CLI search with `-l`, `-f`, `-i`, `-o`. → First end-to-end test with real data.
2. ✅ **Common Crawl** WET + WAT import, URL normalization and the merge rules (§6.4).
3. ✅ **All v1 operators**, autocomplete, match highlighting.
4. ✅ **Web server**: Web API, Mikado web UI, `-a`/`-p`, `-b`, systemd unit. (Local-socket single instance moves to step 5.)
5. ✅ **GUI**: local-socket single instance (§4), main window with tabs, tray, quick search popup, drag-and-drop imports, removing imports, Settings, shortcuts, `-w`, `-t`.
6. ✅ **`.lost` format**: export (everything, a search, or chosen imports), import, signing, trust.
7. ✅ **Packaging**: `.deb`, AppImage, tarball with `install.sh`, macOS `.app`, Windows zip; CI and a release workflow (§5.1). Sample pages on first run (§8.7), the index's layout stamp (§6.6).

### Later

- ✅ `lost-spider gemini`, `gopher`, `onion` and `i2p` (§15).
- `related:`; link-based ranking.
- Multiple physical indexes for very large deployments; LAN federation.
- Mobile apps via UniFFI bindings; Docker image; FreeBSD rc script; Windows service.
- WARC import and the capture archive (§6.7): every capture kept, HTML included; `cache:` showing the page as captured, with a calendar of captures.

---

## 15. lost-spider

A crawler for the networks LOST can't get dumps of. It saves what it finds as a signed `.lost` file, which LOST imports like any other. LOST never goes online (§1); the spider is the one part that does, so it's kept apart:

- **Built only on request:** the `spider` feature isn't in the default set (`cargo build --release --no-default-features --features spider --bin lost-spider`, `cargo install lost --features spider`, or `make spider`). The packages don't include it, so installing LOST never means something crawls in the background.
- **Its own code:** `src/spider/` holds the binary's own modules (`main.rs`, `crawl.rs` for what every network shares, one file per network). It uses the library — `lostfile` for writing and signing, `urls`, `Identity` — and nothing in the library or the `lost` binary knows it exists. Its dependencies are optional and only it uses them.
- No GUI.

### 15.1 Command line

```
lost-spider NETWORK [-o PATH] [-c] [--link URL]… [--links FILE] [--dont-crawl]
                    [--search PHRASE]… [--searches FILE] [--blacklist FILE] [--delay SECONDS]
                    [--host-limit N] [--proxy URL] [-t SECONDS]
```

`NETWORK` is `gemini` (§15.4), `gopher` (§15.5), `onion` (§15.6) or `i2p` (§15.7); the options are the same for each.

| Option | What it does |
|---|---|
| `-o`, `--output PATH` | A `.lost` file, or a folder to put one in. Default name: `lost-crawl-data-NETWORK-YYYYMMDD-HHMMSS.lost` (UTC), in the current folder. An existing file isn't touched: ` (1)`, ` (2)`… goes before `.lost` |
| `-c`, `--continue` | Carry on with exactly the `-o` file (§9.4, §15.3), whoever signed it; started under that name if it doesn't exist |
| `--link URL`, `--links FILE` | Crawled first, even if nothing links to them (`example.org` means `gemini://example.org/`, or `gopher://…` for Gopher). A file has one a line; blank lines and `#` comments are skipped |
| `--dont-crawl` | Fetch only the `--link` URLs (following their redirects) and none of the links on them: an easy way to add certain pages to a file, and nothing else. No seeds, and with `-c` nothing from the file's pages; a search box among them gets the `--search` phrases, and the results pages are saved. Needs `--link` or `--links` |
| `--search PHRASE`, `--searches FILE` | Typed into the search boxes found (§15.2) |
| `--blacklist FILE` | URLs and pages to skip, this run: the draft blocklist format of §9.2 (domains, URL beginnings, hosts-file lines, `/regex/` on the URL, `title:/regex/`, `body:/regex/`). Skipped pages aren't saved and their links aren't followed |
| `--delay SECONDS` | Between two requests to the same server; default 1, decimals allowed |
| `--host-limit N` | Pages saved per host at most, counting those already in the file; default: no limit |
| `--proxy URL` | Go through a proxy (`net.rs`): `socks5://HOST:PORT` for any network (Tor's is `socks5://127.0.0.1:9050`; host names are resolved by the proxy, so nothing reaches the local DNS, and Gemini and Gopher on `.onion` addresses become reachable), or `http://HOST:PORT` for I2P's HTTP proxy, which is sent whole requests. `onion` uses Tor's by default, `i2p` `http://127.0.0.1:4444`. Checked before anything else: nothing answering there stops the spider with a message (is Tor running?) |
| `-t`, `--timeout SECONDS` | How long to wait for a server to connect, or to send the next bit of an answer; a whole answer gets four times that. Defaults: Gopher 10 s, Gemini 15 s, either through a proxy 30 s (Tor adds a few seconds a hop), `.onion` 60 s, I2P 120 s (reaching a site for the first time can take a minute or more while the router builds tunnels) |

- Nothing given on the command line is stored, so `-c` needs the options again.
- Signed with your ID (§9.2), made if there's none yet ("Saving to NAME, signed as 3f9a 1c2e 77b0 1d4e."); unsigned only if LOST's folder can't be found.
- Progress goes to stderr (the pages counted are all the file's, with `-c` too): `Crawling Geminispace for 2 hours, 5 minutes, 9 seconds… 1,234 pages saved · 56 capsules · 7,890 links waiting` (the time is this run's) — `Scraping Geminispace…` with `--dont-crawl`, since it only fetches what it's given — redrawn every second on a terminal, a line a minute otherwise.
- Ctrl-C (or SIGTERM, SIGHUP) stops: requests under way are abandoned, pages already received are written, and the file is finished and signed. A second Ctrl-C quits at once, leaving the file as of its last checkpoint. The last lines say how to continue and how to import.

### 15.2 Crawling (`crawl.rs`)

- **Start:** the `--link` URLs first, then the network's seeds, then (with `-c`) the links of the file's pages. With `--dont-crawl`, only the `--link` URLs (§15.1).
- **Looking up hosts:** at most half the workers look up host names while there are servers ready to be asked, so a run that starts with thousands of new hosts (as a continued one does) gets to crawling at once.
- **Politeness:** one request at a time per server *IP address* — a host serving thousands of capsules as subdomains is one server — `--delay` apart; up to 32 requests at once to different servers. Through a proxy nothing is looked up, and every host is a server of its own. A host here is its scheme, name and port: `http://x` and `https://x` are two, each with its own robots.txt. A "slow down" answer (Gemini's 44) is waited out before asking again.
- **robots.txt** is fetched from every host before anything else, every run. The rules of the groups for `*`, `indexer`, `archiver` (Gemini's "virtual user agents") and `lost-spider` apply; the longest matching rule wins (`Allow` on a tie); `*` and a final `$` work as in Google's rules. No robots.txt (a "not found", or any answer that isn't text) means everything is allowed. A robots.txt that can't be fetched (no connection, a timeout, "try later") is asked for again before anything else on that host; if it never comes, the host is left alone for the rest of the run: nothing is fetched anywhere without its rules. Every page, queued before the rules came or not, is checked against them just before it's fetched.
- **Internationalized host names** (`café.example`) are kept as written, lowercased, in the saved pages, and sent in punycode (`xn--caf-dma.example`: looked up, in TLS's SNI, in the request); both spellings are the same host for politeness, robots.txt, `--dont-crawl`, `--host-limit` and `--blacklist`. The conversion is `urls::ascii_host`: RFC 3492, without IDNA's Unicode mapping tables.
- **Once each:** URLs that normalize alike (§6.5) are fetched once a run. A page with the same text as one saved already isn't saved, nor its links followed; a blacklisted page's text doesn't count, so a copy elsewhere is still saved. A redirect to another spelling of the same URL (`/dir` to `/dir/`) is followed even though the two normalize alike.
- **Skipped URLs:** longer than 1024 bytes, more than 30 path segments or one segment 3 times or more (pages that generate themselves), host names that can't be one, other schemes, `--blacklist` matches (URLs before they're asked, then titles and text, also of files saved by their link alone and of pages that are only followed, like search results and address books), hosts at `--host-limit`.
- **Failures:** redirects are followed up to 5 in a row. A failed request (no connection, a TLS failure, a timeout) is tried once more; a host that fails 3 in a row is left alone for the rest of the run, as is one whose name doesn't resolve. "Try later" answers are tried twice more.
- **Search boxes:** only with `--search`. A box gets each phrase if its prompt or path says search, query, find, keyword… and nothing like comment, post, message, reply, name, mail, password… — a guestbook looks just like a search box, and the spider never writes anything anywhere. Password boxes (Gemini's 11) never. The results pages' links are followed; the pages aren't saved. robots.txt still applies: TLGS, for one, disallows its search page, so it isn't searched.
- **Files** with a known extension (images, audio, video, archives, PDFs, fonts, …) are saved from the link alone: the URL, the link's text (or the file name) as title, no text, and the server isn't asked. Other responses that aren't text are saved the same way, after reading only the response's first line.
- **Pages:** a page's text is cut at `MAX_BODY_BYTES` (§6.4), as an import would. A page with no text (an empty file, say) is still saved, with its title, like a file saved by its link. `url`, `sources` (the network: `["gemini"]`, `["onion"]`, …), `title` (the page's own, else the link's text, else the file name, else the host), `body`, `links` (all of them, any scheme), `crawled`.

### 15.3 Continuing

With `-c`, the file's pages are read first: their URLs count as fetched, their texts' hashes as seen, pages per host are counted for `--host-limit`, and every link that isn't a page yet is queued — newest first, since the last pages' links are where the crawl stopped, while older links still not crawled are mostly ones that failed before. URLs fetched before but never saved (redirects, errors, unreachable hosts, duplicates, search results) are asked again — after weeks or months, that's what's wanted. robots.txt, slow-downs and failure counts start fresh. A damaged chunk is skipped while reading (its pages may be crawled again).

### 15.4 Gemini (`gemini.rs`)

- TLS 1.2 or 1.3 (rustls with ring), with SNI. Any certificate is accepted: Gemini capsules sign their own, and only public pages are read; the handshake's signatures are still checked.
- A request is the URL and CRLF; a response, `NN META` and CRLF. Timeouts per `-t` (15 s; 60 s for a whole response); an answer is read up to `MAX_BODY_BYTES` (4 MiB, §6.4). Through a SOCKS5 `--proxy` (only). A connection closed without TLS's close_notify ends a response normally (many servers do that).
- `text/gemini`: the title is the first `#` heading (else the first heading of any level); the body is every line's text without the markup — a `=>` line's text, or its URL if it has none; preformatted blocks as they are; links are the `=>` lines, resolved against the page, plus `gemini://` URLs written in the text. `text/markdown`: the title from the first `# ` line. Other `text/*`: the text as it is. UTF-8, or Windows-1252 when the server says Latin-1 or Windows-1252 (as browsers do: 0x80–0x9F are curly quotes, dashes, € …).
- Seeds: geminiprotocol.net, the known-hosts lists of Kennedy and TLGS, the BBS at bbs.geminispace.org, the Antenna aggregator and mozz.us.

### 15.5 Gopher (`gopher.rs`)

- Plain TCP on port 70, no TLS. A request is the selector and CRLF (for a search, the selector, a tab and the words); the answer is the item, until the server closes the connection. Timeouts per `-t` (10 s; 40 s in all); an item is read up to `MAX_BODY_BYTES` (4 MiB, §6.4): a server that sends forever is cut off there. Through a SOCKS5 `--proxy` (only).
- URLs (RFC 4266): the path is `/`, the item type, then the selector, percent-encoded (`gopher://host/0/notes/a%20b.txt`); a bare host is its main menu. Status lines say `gopherholes`.
- **Fetched:** text files (type `0`), menus (`1`), search results (`7`, with `--search`), HTML (`h`). **Saved by their link alone:** BinHex, DOS files, uuencoded files, binaries, GIFs, images, PNGs, sounds, video, documents and PDFs (`4 5 6 9 g I p s ; < d P`). **Skipped:** telnet, CSO phone books, mirrors (`+`) and types without an agreed meaning.
- **Menus** are pages: every line's display text, info lines (`i`) included; links are every item there's something to save from, plus `gopher://` URLs written in info lines. `h` items whose selector is `URL:…` are links to the web: kept in `links`, not crawled. A menu's title is the link's text, else the selector's last part, else the host.
- **Text files** lose their closing `.` line. **HTML** becomes text as in the web imports, its `<title>` the title, its `<a href>` links (and `gopher://` URLs in its text) the links; only `gopher://` ones are crawled.
- A server's error menu (a first line of type `3`) instead of the item counts as "not found". Text that isn't UTF-8 is read as Windows-1252: much of Gopherspace predates UTF-8.
- **robots.txt** is the text file with the selector `robots.txt` (Veronica-2's convention); its rules are matched against selectors, as `/` and the selector.
- **Search boxes** (`7`) are searches by definition: one gets the `--search` phrases unless its text or selector says it's for writing something (a guestbook, a comment…).
- Seeds: gopher.floodgap.com (home of Veronica-2, Gopherspace's search engine) and its list of servers, sdf.org, bitreich.org, tilde.team, gopher.quux.org.

### 15.6 The web through Tor and I2P (`http.rs`, `onion.rs`, `i2p.rs`)

Both are ordinary websites reached through a proxy, so they share an HTTP client and the reading of HTML:

- **HTTP/1.1** (`http.rs`): `GET`, `Connection: close`, `Accept-Encoding: gzip`; chunked bodies put back together; `User-Agent: lost-spider/VERSION`. Through a SOCKS5 proxy the request goes down a tunnel; through an HTTP proxy it names the whole URL (`GET http://site.i2p/page HTTP/1.1`). `https://` works over SOCKS5, accepting any certificate (a `.onion` address already proves which server it is), not through an HTTP proxy.
- **Answers:** 2xx is a document — text (`text/*`, XHTML; `text/html` when the server doesn't say) read up to `MAX_BODY_BYTES` (4 MiB), anything else saved by its link alone without reading it; 301/302/303/307/308 a redirect; 429 and 503 with `Retry-After` a slow-down; other 5xx "try later"; anything else "not found".
- **HTML:** the title from `<title>`, the text from `text::html_to_text` (which leaves out scripts, styles, comments and SVG, and turns blocks into blank lines), the links from `<a href>`, `<area href>`, `<frame src>` and `<iframe src>`, resolved against the page, each with its text, plus `http(s)://` URLs written in the text (lists of sites often are just that). Windows-1252 when the type or a `<meta>` near the start says Latin-1 or Windows-1252 and the bytes aren't UTF-8. A chunked body cut off mid-chunk, or claiming a chunk bigger than memory, keeps what arrived. Other text: as it is, with `http(s)://` URLs written in it as links.
- robots.txt, the blacklist, search boxes, `-c`, `--dont-crawl` and the rest work as for the other networks.

**`.onion`** (`lost-spider onion`):

- Through Tor's SOCKS5 proxy (`socks5://127.0.0.1:9050` unless `--proxy` says otherwise; an HTTP proxy is refused). Tor resolves the names. Only v3 addresses (56 base32 letters and digits, subdomains allowed) are crawled: the older ones were switched off in 2021.
- **Ahmia's list of banned sites, first.** Ahmia, the onion search engine, publishes the onion sites it bans for child abuse material, as MD5 hashes of their names (`/blacklist/banned/` on its onion site). The spider fetches it through Tor before crawling — saying "Waiting for Tor to connect…" and trying every 10 seconds for up to 5 minutes, since Tor reaches nothing until it has connected to the network — adds it to the blacklist (`Blacklist::ban_hashed`; a site is its last two labels, `name.onion`), and doesn't start without it. Images and other files are never downloaded on any network — saved by their link alone — so what's saved is text.
- Seeds: Ahmia's onion site and its list of the onion sites it knows (`/onions/`; robots.txt keeps crawlers out of `/address/`, the same list as a page).
- Default timeout 60 s.

**I2P** (`lost-spider i2p`):

- Through an I2P router: its HTTP proxy (`http://127.0.0.1:4444` unless `--proxy` says otherwise), or i2pd's SOCKS5 proxy. Only `http://` on `.i2p` hosts is crawled.
- **Its own address book.** A router finds `.i2p` names in its address book, which one that has just started hasn't downloaded yet (i2pd fetches it a few minutes in), so every name would fail. Instead, every address book the spider reads (`name.i2p=destination` lines) tells it where each site is: its `.b32.i2p` address is the base32 of the SHA-256 of the destination's bytes (I2P's base64, `-~` for `+/`), which the router reaches without any name. Sites are asked for by that address and saved under their names; names from nowhere else go to the router as they are.
- **Waiting for the router:** before crawling, the spider asks for the first seed (i2pd's address book, by `.b32.i2p` address) until it comes, up to 10 minutes, saying it's waiting — a router reaches nothing until it has built tunnels.
- The router answers for sites it can't reach, with pages of its own: a 5xx, or a 4xx page that talks about its address book or jump services, counts as the site being unreachable (§15.2's failures), not as one of its pages.
- **Address books:** a text file that's mostly `name.i2p=destination` lines (an I2P `hosts.txt`) isn't saved; each name in it becomes a site to crawl (`http://name.i2p/`).
- Seeds: the address books routers start with, by `.b32.i2p` address — i2pd's (`addressbook.defaulturl` in its source) and Java I2P's (the I2P project's) — then those i2pd's sample configuration subscribes to (reg.i2p, identiguy.i2p, stats.i2p's new hosts), notbob.i2p's list of sites that are up, and the I2P project's site. More with `--links`.
- Default timeout 120 s: a site reached for the first time can take a minute or more while the router builds tunnels.

Darknets in particular host material no one wants in a dataset: `--blacklist`'s `title:/…/` and `body:/…/` rules keep pages out by what they say, and their links aren't followed.

---

## 16. Open questions

None at the moment. (The icon, once one, is settled: `res/images/lost.svg`, "LOST" in Inter ExtraBold as outlines on a dark rounded square; `lost-64.rgba` for the window and tray, `dist/icons/` for packages.)
