# LOST

**Local Offline Search Tool** — a search engine for the web that runs entirely on your own device.

The database is stored locally, nothing is ever sent anywhere, and there's no search history.
It keeps finding results with no internet connection at all.

> Status: pre-alpha. The design is in [ARCHITECTURE.md](ARCHITECTURE.md).
> Working so far: importing Hacker News CSV exports and Common Crawl WET/WAT files, exporting and importing `.lost` files (signed, to share with other people), searching from the command line, the web server with its web UI (`lost -s`, then open http://127.0.0.1:1057), and the desktop app (`lost`, or `lost -w` for both).


## Install

Download a package from the [releases](https://github.com/Y2Z/LOST/releases):

- **Debian, Ubuntu, Mint:** `sudo apt install ./lost_…_amd64.deb`
- **Any Linux:** the `.AppImage` — make it executable and run it. Or unpack the tarball and run `sudo sh dist/install.sh`.
- **macOS:** unzip, drag LOST to Applications. The first time, right-click it and choose Open (it isn't signed yet).
- **Windows:** unzip and run `lost.exe`.

To run the web server as a service (Linux, after installing the `.deb` or with `install.sh`):
`sudo systemctl enable --now lost`, then open http://127.0.0.1:1057.

From source: `make && sudo make install` (`sudo make uninstall` to remove).


## Try it

```console
cargo build --release
./target/release/lost -d hacker-news.csv        # import a BigQuery export of bigquery-public-data.hacker_news.full
./target/release/lost -d CC-MAIN-…-00000.warc.wet.gz CC-MAIN-…-00000.warc.wat.gz   # Common Crawl
./target/release/lost rust compiler             # search
./target/release/lost -f json -l 5 rust -site:github.com
./target/release/lost cache:https://example.com/page   # the stored text of one page
./target/release/lost --imports                    # past imports (lost --remove-import 3 removes one)
./target/release/lost -e rust.lost rust compiler   # export what a search finds (or everything: lost -e all.lost)
```

Data is stored in `~/.lost/` (Linux, BSD), `~/Library/Application Support/LOST/` (macOS)
or `%USERPROFILE%\LOST\` (Windows).


## Usage (planned)

```console
lost catfish recipe                 # search from the terminal
lost -l 10,20 -f json catfish       # skip 10, show 20, as JSON
lost -d hn-2024.csv                 # import a dataset
lost                                # desktop app with tray icon
lost -s                             # web UI and Web API on http://127.0.0.1:1057
lost -s -a 0.0.0.0 -p 8080          # serve the local network on port 8080
lost -b                             # same as -s, in the background
lost -w                             # desktop app and web server together
```

Search operators work like the big search engines:
`"exact phrase"`, `"how to * catfish"`, `-exclude`, `OR`, `( )`, `site:`,
`intitle:`, `inurl:`, `intext:` (and `allin…:`), `filetype:`, `link:`.

```console
lost --suggest "rust comp"        # autocomplete
lost --suggest "how to _ catfish" # fill in the blank
```


## Data

LOST searches whatever datasets you import. The first time it runs, it adds two sample pages
(the front pages of Hacker News and Slashdot, `res/data/sample.lost`) so there's something to search;
remove them any time (`lost --remove-import 1`, or Settings → Data).


- Common Crawl (WET and WAT files)
- Hacker News (BigQuery CSV export)
- `.lost` files — LOST's own format, for sharing crawl data with other people

More to come: Gemini, Gopher, .onion, I2P.


## Building

```console
cargo build --release                                           # everything
cargo build --release --no-default-features --features cli      # CLI only
cargo build --release --no-default-features --features cli,server
cargo build --release --no-default-features --features cli,gui
cargo build --release --no-default-features --lib               # library only
make tarball                                                    # packages (see the Makefile)
make deb                                                        #   needs cargo-deb
make appimage                                                   #   needs appimagetool
```

Releases: push a tag like `v0.2.0` (the version in `Cargo.toml`) and GitHub Actions builds every package into a draft release.


## License

[0BSD](LICENSE).

`res/web/mikado.light.min.js` is [Mikado](https://github.com/nextapps-de/mikado), © Nextapps GmbH, Apache-2.0.
