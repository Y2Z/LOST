# LOST

**Local Offline Search Tool** — a web search engine that runs entirely on your computer.

The database is stored locally, nothing is ever sent anywhere, and there's no search history.
It keeps finding results with no internet connection at all.



## Usage

#### Open desktop application (GUI)

```console
lost
```

#### Running local web server

```console
lost -s                             # web UI and Web API on http://127.0.0.1:1057
lost -s -a 0.0.0.0 -p 8080          # serve the local network on port 8080
lost -b                             # same as -s, but in the background
lost -w                             # run desktop app and web server together
```

#### Searching straight in the terminal

```console
lost hacker news                    # Search for hacker news website
lost cache:https://slashdot.org     # View stored copy in the terminal
lost -f json rust -site:crates.io   # Output search results formatted as JSON
lost --suggest "rust comp"          # autocomplete
lost --suggest "how to _ catfish"   # autocomplete, fill in the blank
```

#### Working with datasets

```console
lost -d hacker-news.csv           # import a BigQuery export of bigquery-public-data.hacker_news.full
lost -d CC-MAIN-…-00000.warc.wet.gz CC-MAIN-…-00000.warc.wat.gz   # import Common Crawl data
lost --imports                    # list past imports to see what's in the database
lost --remove-import 3            # remove import along with all pages that came from it
lost -e rust.lost rust compiler   # export what a search finds (or everything: lost -e all.lost)
lost -e rust.lost --sign rust     # the same, signed with your ID (exports are unsigned by default)
```

#### Supported search operators:
- `"exact phrase"`
- `"how to * catfish"`
- `-exclude`
- `OR`
- `( )`
- `site:`
- `intitle:`
- `allintitle:`
- `inurl:`
- `allinurl:`
- `intext:`
- `allintext:`
- `filetype:`
- `link:`


## Data

LOST searches through whatever datasets you import.  It comes out of the box with two pages (the front pages of Hacker News and Slashdot), so you could start seeing it in action right away.

### Supported dataset formats

- Common Crawl (WET and WAT files)
- Hacker News (BigQuery CSV export)
- `.lost` files — LOST's own format
- Gemini, Gopher, `.onion` and I2P — crawled with `lost-spider` (see [below](#crawling-gemini-gopher-onion-and-i2p))


### Datasets

#### Common Crawl

[Common Crawl](https://commoncrawl.org/) is a 501(c)(3) American non-profit organization that crawls the internet and provides a free, open-source archive of web data to the public.  Their datasets are available for free to anyone [here](https://commoncrawl.org/get-started).  The combined full size of their latest dataset is over 20TiB.

Here's one of the ways to obtain a small piece of their data:
```console
curl -s https://data.commoncrawl.org/crawl-data/CC-MAIN-2026-39/wet.paths.gz | zcat | head -c 500000000 > sample.wet
curl -s https://data.commoncrawl.org/crawl-data/CC-MAIN-2026-39/wat.paths.gz | zcat | head -c 500000000 > sample.wat
```

#### Hacker News

The [Hacker News](https://news.ycombinator.com) public dataset is available on [Google BigQuery](https://console.cloud.google.com/bigquery?p=bigquery-public-data&d=hacker_news&page=dataset).  The dataset is part of the Google Cloud Marketplace Public Datasets and contains over 50 million rows (stories, comments, job posts) derived directly from the official Hacker News Firebase API.  Merging this dataset into your LOST database will allow you to search through pages of Hacker News locally.  The full size exceeds 50GiB.

Sample SQL command to select a chunk of data:
```sql
SELECT * FROM `bigquery-public-data.hacker_news.full` WHERE timestamp >= '2026-01-01' AND timestamp < '2026-10-10';
```

#### LOST files

LOST is capable of importing and exporting `.lost` dataset files, which makes it possible to freely and safely exchange crawl data with other LOST users.  They aren't signed unless you ask (`lost -e FILE --sign`, or the "Sign it" checkbox): a signature lets others tell which files are yours and keeps your database from getting corrupted or spammed, but it also says who made the file.  Both exports and imports can be made either via CLI (see [Usage](#working-with-datasets)) or GUI (Settings → Data).

#### Crawling: Gemini, Gopher, .onion and I2P

LOST itself never goes online.  For [Geminispace](https://geminiprotocol.net/), Gopherspace, `.onion` sites and I2P sites there's `lost-spider`, a separate crawler that isn't built or installed by default:

```console
cargo install lost --features spider               # or: make spider
lost-spider gemini                                 # crawl all of Geminispace
lost-spider gemini --link example.org -o my.lost   # start at a capsule nothing links to
lost-spider gemini --link example.org --dont-crawl # only that page, nothing else
lost-spider gemini -c -o my.lost                   # carry on where it stopped
lost-spider gopher                                 # crawl all of Gopherspace
lost-spider onion                                  # .onion sites, through Tor (127.0.0.1:9050)
lost-spider i2p                                    # I2P sites, through an I2P router (127.0.0.1:4444)
lost-spider gemini --proxy socks5://127.0.0.1:9050 # Gemini through Tor
lost -d my.lost                                    # import what it found
```

It obeys robots.txt, asks each server for one thing a second (`--delay`), and saves everything into one `.lost` file, unsigned unless you ask for `--sign`.  The file is complete even if you stop it with Ctrl-C.  See `lost-spider --help` for search boxes, blacklists, proxies and limits.

`onion` needs [Tor](https://www.torproject.org/) running, and `i2p` an I2P router ([i2pd](https://i2pd.website/) or [Java I2P](https://geti2p.net/)).  I2P is slow: reaching a site for the first time can take a minute or more, so its timeout is 120 seconds by default (`-t` changes it, for any network), and on a router (or Tor) that has just started, `lost-spider` waits a few minutes until it can reach sites.  Images and other files are never downloaded, only their links.  Before crawling `.onion` sites, `lost-spider` gets [Ahmia](https://ahmia.fi/)'s list of onion sites banned for child abuse material and skips them; it won't start without that list.  Darknets host all sorts of things, so it's worth keeping a `--blacklist` with `title:/…/` and `body:/…/` rules for what you don't want in your dataset.


### Database

The local database is stored by default either in `~/.lost/` (Linux, BSD), `~/Library/Application Support/LOST/` (macOS),
or `%LOCALAPPDATA%\LOST\` (Windows, i.e. `C:\Users\NAME\AppData\Local\LOST\`).


## Building

#### Everything
```console
cargo build --release
```

#### Only CLI
```console
cargo build --release --no-default-features --features cli
```

#### CLI & web server
```console
cargo build --release --no-default-features --features cli,server
```

#### CLI & GUI
```console
cargo build --release --no-default-features --features cli,gui
```

#### Bare library
```console
cargo build --release --no-default-features --lib
```

#### Spider (crawler)
```console
cargo build --release --no-default-features --features spider --bin lost-spider
```

#### Packages
```console
make tarball     # (see the Makefile)
make deb         # needs cargo-deb
make appimage    # needs appimagetool
```



## Installation

Download a package from the [releases](https://github.com/Y2Z/LOST/releases):

- **Debian, Ubuntu, Mint:** `sudo apt install ./lost_…_amd64.deb`
- **Any Linux:** the `.AppImage` — make it executable and run it. Or unpack the tarball and run `sudo sh dist/install.sh`.
- **macOS:** unzip, drag LOST to Applications. The first time, right-click it and choose Open (it isn't signed yet).
- **Windows:** unzip and run `lost.exe`.

To run the web server as a service (Linux, after installing the `.deb` or with `install.sh`):
`sudo systemctl enable --now lost`, then open http://127.0.0.1:1057.

From source: `make && sudo make install` (`sudo make uninstall` to remove).


<!-- Releases: push a tag like `v0.2.0` (the version in `Cargo.toml`) and GitHub Actions builds every package into a draft release. -->



## License

[0BSD](LICENSE).

`res/web/mikado.light.min.js` is [Mikado](https://github.com/nextapps-de/mikado), © Nextapps GmbH, Apache-2.0.
