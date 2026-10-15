//! Desktop app, built with egui (ARCHITECTURE.md §11).
//!
//! - Main window: tabs (pills with their result count) with the ☰ menu, a
//!   search box with suggestions beside or under it, and the results of the
//!   current tab: searched as you type, matches highlighted, more loaded as
//!   you scroll. Only the results in view are drawn, so long lists scroll
//!   smoothly.
//! - "Settings" window: Configuration (lost.toml, as a form) and Data
//!   (pages, importing, past imports).
//! - "About", a small window of its own.
//! - A tray icon (ksni on Linux and BSD, tray-icon on Windows and macOS).
//!   With it, closing the window keeps LOST running in the tray; without it
//!   (`-t`, or no tray on this desktop), closing quits. Quitting asks first
//!   while an import is running.
//! - Tabs live in memory only: quitting (or a crash) forgets them. Only the
//!   window's size and position are remembered.
//!
//! Hiding: Wayland lets no app hide, show or focus its own window, so on
//! Linux and BSD the window runs through XWayland (X11) when it's there,
//! where all three work. `LOST_WAYLAND=1` keeps it on Wayland.
//!
//! Searches, suggestions and imports run on background threads and report
//! back through one channel; each search is tagged so a late answer for an
//! old query is dropped.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Key, Layout, Modifiers, RichText, Sense, TextFormat,
    ViewportCommand,
    text::{LayoutJob, TextWrapping},
};
use serde_json::json;

use crate::config::Config;
use crate::datasources::Progress;
use crate::datasources::lost::{Answer, Question};
use crate::engine::{CachedPage, Hit, SearchResults};
use crate::instance::{self, Listener};
use crate::output::thousands;
use crate::output::{local_date, local_datetime};
use crate::query::Limit;
use crate::store::{ImportRow, TrustRow};
use crate::text::format_bytes;
use crate::{Declined, Identity, Lost, Result};

/// Results per request while scrolling.
const PAGE: usize = 20;
/// Wait after the last keystroke before searching, and before suggesting.
const SEARCH_DELAY: Duration = Duration::from_millis(200);
const SUGGEST_DELAY: Duration = Duration::from_millis(80);
const SUGGESTIONS: usize = 8;
/// Load more when the end of the list is this close (in points).
const LOAD_AHEAD: f32 = 600.0;
/// Every result is drawn the same height: a one-line title, a snippet of
/// at most this many lines, and the URL.
const SNIPPET_LINES: usize = 3;
/// Suggestions go beside the search box when the window is at least this
/// wide, and on a line of their own under it otherwise.
const SUGGESTIONS_BESIDE: f32 = 760.0;

const TITLE_SIZE: f32 = 17.0;
const SNIPPET_SIZE: f32 = 14.5;
pub(crate) const URL_SIZE: f32 = 12.5;
const PILL_SIZE: f32 = 14.0;
const TAB_HEIGHT: f32 = 26.0;
/// The About window's size.
const ABOUT_SIZE: [f32; 2] = [380.0, 330.0];
/// The Settings window's size; the width is also its least width: every
/// column of past imports fits.
const SETTINGS_SIZE: [f32; 2] = [980.0, 620.0];
/// The export's progress bar, under the Export section's title.
const EXPORT_BAR_WIDTH: f32 = 320.0;
/// Space between the columns of past imports.
const TABLE_SPACING: f32 = 14.0;
/// The Status column of past imports: a progress bar, or a word.
const STATUS_WIDTH: f32 = 120.0;
/// Progress bars, and an export that went well.
const PROGRESS_GREEN: Color32 = Color32::from_rgb(0x7e, 0xe7, 0x87);
/// How often the Settings window re-reads what it shows.
const SETTINGS_REFRESH: Duration = Duration::from_secs(3);
/// Tabs share the bar's width equally, on one line, however narrow.
const MIN_TAB_WIDTH: f32 = 8.0;

/// The Linux and BSD tray runs on a thread of its own; the others need
/// the window's event loop.
const TRAY_THREAD: bool = cfg!(all(unix, not(target_os = "macos")));

/// The eye, 64×64 RGBA, for the window, the taskbar and the tray.
pub(crate) const ICON_RGBA: &[u8] = include_bytes!("../res/images/lost-64.rgba");
pub(crate) const ICON_SIZE: u32 = 64;

/// What the app is started with.
pub struct Options {
    /// Show a tray icon (`gui.tray` in lost.toml, off with `-t`).
    pub tray: bool,
    /// `--minimized`: start in the tray (or, without one, minimized).
    pub minimized: bool,
    pub data_dir: PathBuf,
    /// The configuration in effect, edited in Settings → Configuration.
    pub config: Config,
    /// The user's lost.toml, where Settings saves it.
    pub config_path: Option<PathBuf>,
    pub default_limit: usize,
    pub max_results: usize,
    /// With `-w`: the web server's address, and its error if it stopped.
    pub server: Option<ServerInfo>,
}

pub struct ServerInfo {
    pub url: String,
    pub error: Arc<Mutex<Option<String>>>,
}

/// What egui says when eframe couldn't make a window (see [`separate`]).
const NOT_DRAWN: &str = "the user callback was never called";

/// Draws windows of their own (Settings, About, the quick search popup).
///
/// While the main window is off screen — hidden in the tray, or shown a
/// moment ago and not mapped yet — eframe draws it from a place where it
/// can't make new windows, and egui then panics ("the user callback was
/// never called"). Such a frame is skipped: the window comes with the next
/// one, once the main window is on screen.
fn separate(draw: impl FnOnce()) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(draw));
}

/// Runs the app until it quits.
pub fn run(lost: Arc<Lost>, options: Options, listener: Listener) -> Result<()> {
    // The panic `separate` expects isn't news: everything else still is.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("");
        if !message.contains(NOT_DRAWN) {
            report(info);
        }
    }));

    let mut state = State::new(lost, options, listener);
    let native = native_options(&state.options);
    let app: &mut State = &mut state;
    eframe::run_native(
        "LOST",
        native,
        Box::new(move |cc| {
            app.attach(cc);
            Ok(Box::new(Window(app)))
        }),
    )
    .map_err(|e| format!("can't open the window: {e}").into())
}

fn native_options(options: &Options) -> eframe::NativeOptions {
    let icon = egui::IconData {
        rgba: ICON_RGBA.to_vec(),
        width: ICON_SIZE,
        height: ICON_SIZE,
    };
    // The main window's size and place from last time (see `Geometry`).
    let last = Geometry::load(&options.data_dir);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(last.as_ref().map_or(egui::vec2(920.0, 720.0), |g| g.size))
        .with_maximized(last.as_ref().is_some_and(|g| g.maximized));
    if let Some(at) = last.and_then(|g| g.position) {
        viewport = viewport.with_position(at);
    }
    eframe::NativeOptions {
        viewport: viewport
            .with_title("LOST")
            .with_app_id("lost")
            .with_min_inner_size([420.0, 320.0])
            .with_icon(Arc::new(icon))
            .with_drag_and_drop(true)
            // --minimized without a tray: minimized right after it opens
            // (see `attach`; there's no starting minimized in egui).
            .with_visible(!(options.minimized && options.tray)),
        // Where the file dialogs were (see Window::save). The window's size
        // and place are kept by LOST itself: eframe would also keep the
        // quick search popup's, while the main window stands in for it.
        persistence_path: Some(options.data_dir.join("gui.ron")),
        persist_window: false,
        // Off: eframe also draws the window while it isn't on screen yet
        // (just shown, or minimized), and with vsync, swapping buffers then
        // waits for a screen it isn't on — for ever. LOST only draws when
        // something changes, so nothing is lost.
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        event_loop_builder: x11_please(),
        ..Default::default()
    }
}

/// On Wayland an app can't hide, show or focus its own window, which the
/// tray needs, so the window goes through XWayland when there is one.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn x11_please() -> Option<eframe::EventLoopBuilderHook> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    if !set("DISPLAY") || set("LOST_WAYLAND") {
        return None;
    }
    Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11;
        builder.with_x11();
    }))
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
pub(crate) fn x11_please() -> Option<eframe::EventLoopBuilderHook> {
    None
}

/// Wakes the UI from other threads, whichever window is open (if any).
#[derive(Clone, Default)]
struct Waker(Arc<Mutex<Option<egui::Context>>>);

impl Waker {
    fn wake(&self) {
        if let Ok(ctx) = self.0.lock()
            && let Some(ctx) = ctx.as_ref()
        {
            ctx.request_repaint();
        }
    }

    fn set(&self, ctx: Option<egui::Context>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = ctx;
        }
    }
}

// ── State ───────────────────────────────────────────────────────────────

/// Things that happen elsewhere: other threads, the tray, other processes.
enum Event {
    Searched {
        tab: u64,
        generation: u64,
        append: bool,
        result: std::result::Result<SearchResults, String>,
    },
    Suggested {
        tab: u64,
        text: String,
        items: Vec<String>,
    },
    /// A tab's `cache:` page: found, not stored (`None`), or an error.
    Page {
        tab: u64,
        generation: u64,
        result: std::result::Result<Option<CachedPage>, String>,
    },
    ImportProgress(Progress),
    Imported(std::result::Result<Vec<ImportRow>, String>),
    /// An import asks whether to import a `.lost` file; the answer goes back.
    Ask(Question, Sender<Answer>),
    /// An export's progress: (pages written, pages in all).
    ExportProgress(u64, u64),
    /// An export finished: what it did, for the user.
    Exported(std::result::Result<String, String>),
    /// A file dialog closed: what was picked (nothing if cancelled).
    Picked(Dialog, Vec<PathBuf>),
    /// An import was removed: (its id, pages removed).
    /// For each import: (id, pages removed), or what went wrong.
    Removed(Vec<std::result::Result<(u64, u64), String>>),
    /// Another `lost` asked for the window.
    Raise,
    /// Tray: show or hide the window.
    ToggleWindow,
    /// Tray: the window, with a fresh search.
    NewSearch,
    /// Tray: the Settings window.
    Settings,
    /// Tray: the About window.
    About,
    /// Tray: the quick search popup; where the tray icon is (x, y, width,
    /// height in physical pixels), when the tray says.
    QuickSearch(Option<[f32; 4]>),
    /// Tray or another process: quit (asking first if an import runs).
    Quit,
}

struct Tab {
    id: u64,
    /// What's in the search box.
    query: String,
    /// The query the results are for.
    searched: String,
    /// When the box last changed, while a search is still to come.
    edited: Option<Instant>,
    suggest_at: Option<Instant>,
    generation: u64,
    loading: bool,
    /// A new search is running; the results shown are the old ones.
    replacing: bool,
    /// `total` is known for `searched` (shown in the tab's title).
    counted: bool,
    results: Vec<Hit>,
    total: usize,
    took_ms: u64,
    error: Option<String>,
    suggestions: Vec<String>,
    selected: Option<usize>,
    scroll_to_top: bool,
    /// For `cache:URL`: the stored page, once looked up.
    page: Option<Stored>,
    /// For `cache:URL` opened from a result: the search whose words to mark.
    highlight: String,
}

/// What a `cache:` lookup found.
enum Stored {
    Found(Box<CachedPage>),
    Missing,
}

/// What a click on a result asks for.
enum ResultAction {
    /// Open the page in the browser.
    Open(String),
    /// Show its stored text (`cache:`) in a new tab.
    Cached(String),
}

impl Tab {
    fn new(id: u64) -> Tab {
        Tab {
            id,
            query: String::new(),
            searched: String::new(),
            edited: None,
            suggest_at: None,
            generation: 0,
            loading: false,
            replacing: false,
            counted: false,
            results: Vec::new(),
            total: 0,
            took_ms: 0,
            error: None,
            suggestions: Vec::new(),
            selected: None,
            scroll_to_top: false,
            page: None,
            highlight: String::new(),
        }
    }

    /// "catfish (8)": the query, shortened, and how many results it found.
    fn title(&self) -> String {
        let q = self.searched.as_str();
        if q.is_empty() {
            return "New search".to_string();
        }
        let mut title: String = q.chars().take(22).collect();
        if q.chars().count() > 22 {
            title.push('…');
        }
        if self.counted {
            title.push_str(&format!(" ({})", thousands(self.total as u64)));
        }
        title
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Configuration,
    Data,
}

#[derive(Default)]
struct ImportState {
    /// Files of the import running now.
    files: Vec<String>,
    /// Records the running import has read.
    records: u64,
    started: Option<Instant>,
    /// The imports of the run going on now, shown in the table as they go.
    run: Vec<RunRow>,
    /// What went wrong with the last import, under the Import box's buttons.
    message: Option<String>,
    /// What went wrong removing imports, beside the table's heading.
    remove_error: Option<String>,
    /// The `imports` table, when it could be read.
    history: Option<std::result::Result<Vec<ImportRow>, String>>,
    /// Imports ticked in the table: for Remove and Export.
    selected: std::collections::BTreeSet<u64>,
    /// Asking whether to remove these imports: (ids, pages that would go,
    /// the file names, when it's one).
    confirm_remove: Option<(Vec<u64>, u64, String)>,
    /// The imports being removed.
    removing: Vec<u64>,
    /// Bytes the database takes on disk (read with the past imports).
    size: Option<u64>,
    /// A `.lost` file waits for the user: import it or not?
    question: Option<(Question, Sender<Answer>)>,
}

/// One import of the run going on now, in the table of past imports.
struct RunRow {
    planned: crate::datasources::Planned,
    /// Its row in the app database, once it has started.
    row: Option<ImportRow>,
    state: RunState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RunState {
    Waiting,
    Running,
    Done,
    Failed,
}

/// Kinds of message written to stderr (`log`), for anyone looking into a
/// problem from a terminal.
#[derive(Clone, Copy)]
enum LogKind {
    Info,
    /// Something finished.
    Done,
    Error,
    /// Detail for the line above it.
    Note,
}

/// The file dialogs, each opened on a thread of its own so the windows
/// keep drawing (and the system doesn't call LOST unresponsive) while
/// one is open.
#[derive(Debug, Clone)]
enum Dialog {
    ImportFiles,
    ImportFolder,
    /// Where to save an export: of these imports' pages (none: everything),
    /// signed or not.
    Export {
        name: String,
        imports: Vec<u64>,
        sign: bool,
    },
    DataDir,
}

#[derive(Default)]
struct ExportState {
    running: bool,
    done: u64,
    total: u64,
    /// Sign the file. Unticked every time: the choice isn't remembered, so
    /// a file is only signed when you mean it to be.
    sign: bool,
    /// What the last export did, under its button.
    message: Option<std::result::Result<String, String>>,
    /// Your ID, once you have a signing key.
    id: Option<String>,
}

/// Sizes measured while drawing, used on the next frame: boxes of equal
/// height, rows of buttons centred.
#[derive(Default)]
struct Measured {
    /// The Import box: its contents' height, and its outer height.
    boxes: [f32; 2],
    /// The Export section's height, under the table.
    export: f32,
    /// Rows of buttons, by name.
    rows: std::collections::HashMap<&'static str, f32>,
}

/// lost.toml, as edited in Settings → Configuration.
#[derive(Default)]
struct ConfigForm {
    loaded: bool,
    config: Config,
    /// `storage.data_dir` as typed (empty: the default).
    data_dir: String,
    /// What was last loaded or saved, to tell whether anything changed.
    saved: String,
    /// The same, as a configuration: what the restart notes compare with.
    saved_config: Config,
    /// The last version a save was tried with (not tried again unchanged).
    tried: String,
    /// Why the last save failed.
    error: Option<String>,
}

impl ConfigForm {
    fn load(&mut self, config: &Config) {
        self.loaded = true;
        self.config = config.clone();
        self.data_dir = config
            .storage
            .data_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        self.saved = self.current().to_toml();
        self.saved_config = self.current();
        self.tried = self.saved.clone();
        self.error = None;
    }

    /// The form's values as a configuration.
    fn current(&self) -> Config {
        let mut config = self.config.clone();
        let dir = self.data_dir.trim();
        config.storage.data_dir = (!dir.is_empty()).then(|| PathBuf::from(dir));
        config.server.address = config.server.address.trim().to_string();
        config
    }
}

struct State {
    lost: Arc<Lost>,
    options: Options,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    waker: Waker,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    /// The search box had focus last frame; suggestions show only then.
    search_focused: bool,
    /// The pointer was over the suggestions last frame (a click on one takes
    /// focus from the box before it lands).
    pills_hovered: bool,
    /// Close the Settings window (its Close button).
    focus_search: bool,
    settings: Option<Page>,
    /// Bring the Settings window to the front the next time it's drawn.
    raise_settings: bool,
    /// When the Settings window last read what it shows.
    settings_read: Option<Instant>,
    about: bool,
    /// The quick search popup, while it's open.
    quick: Option<crate::quick::Popup>,
    /// While the main window is hidden, it stands in for the window asked
    /// for (see `quick.rs`): which, and its own size and place to go back to.
    stand_in: Option<(StandIn, Geometry)>,
    /// The main window's size and place, as last seen on screen (not while
    /// it's the popup): what's kept for next time.
    geometry: Option<Geometry>,
    /// Not before then: just back from being the popup, the window may
    /// still have the popup's size for a moment.
    geometry_after: Option<Instant>,
    import: ImportState,
    importing: Arc<AtomicBool>,
    export: ExportState,
    /// People whose signed files import without a question.
    trusted: Option<std::result::Result<Vec<TrustRow>, String>>,
    measured: Measured,
    /// A file dialog to open once this frame is drawn.
    want_dialog: Option<Dialog>,
    /// A file dialog is open (one at a time).
    dialog_open: bool,
    /// Where the file dialogs open: the folder the last import came from,
    /// and the one the last export went to (kept between runs).
    import_dir: Option<PathBuf>,
    export_dir: Option<PathBuf>,
    config: ConfigForm,
    confirm_quit: bool,
    /// The configuration LOST started with: settings that only take effect
    /// on a restart are compared with it.
    started: Config,
    /// Asking whether to make a new ID (Settings → Configuration).
    confirm_new_id: bool,
    /// Asking whether to stop trusting this key (hex).
    confirm_untrust: Option<String>,
    /// What went wrong making one, under the ID.
    new_id_error: Option<String>,
    quitting: bool,
    /// The window is shown (not hidden to the tray).
    visible: bool,
    fullscreen: bool,
    tray_wanted: bool,
    tray: Option<tray::Tray>,
    icon: Option<egui::TextureHandle>,
    title: String,
}

impl State {
    fn new(lost: Arc<Lost>, options: Options, listener: Listener) -> State {
        let started = options.config.clone();
        // --minimized with a tray: hidden there from the start.
        let start_hidden = options.minimized && options.tray;
        let (tx, rx) = mpsc::channel();
        let waker = Waker::default();
        let importing = Arc::new(AtomicBool::new(false));

        // Other `lost` processes: "raise" shows this window.
        {
            let (tx, waker, lost, importing) =
                (tx.clone(), waker.clone(), lost.clone(), importing.clone());
            listener.serve(Arc::new(move |command, _| {
                let reply = match command {
                    "raise" => {
                        let _ = tx.send(Event::Raise);
                        json!({ "ok": true })
                    }
                    "quit" => {
                        let _ = tx.send(Event::Quit);
                        json!({ "ok": true })
                    }
                    "status" => json!({
                        "ok": true,
                        "version": env!("CARGO_PKG_VERSION"),
                        "role": "gui",
                        "documents": lost.document_count(),
                        "importing": importing.load(Ordering::Relaxed),
                    }),
                    other => instance::unknown(other),
                };
                waker.wake();
                reply
            }));
        }

        // The Linux tray runs on its own thread; the others need the
        // window's event loop, so they're made in `attach`.
        let tray_wanted = options.tray;
        let tray = if tray_wanted && TRAY_THREAD {
            tray::Tray::create(tx.clone(), waker.clone())
        } else {
            None
        };

        let mut state = State {
            lost,
            options,
            tx,
            rx,
            waker,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            search_focused: true,
            pills_hovered: false,
            focus_search: true,
            settings: None,
            raise_settings: false,
            settings_read: None,
            about: false,
            quick: None,
            stand_in: None,
            geometry: None,
            geometry_after: None,
            import: ImportState::default(),
            importing,
            export: ExportState::default(),
            trusted: None,
            measured: Measured::default(),
            want_dialog: None,
            dialog_open: false,
            import_dir: None,
            export_dir: None,
            config: ConfigForm::default(),
            confirm_quit: false,
            started,
            confirm_new_id: false,
            confirm_untrust: None,
            new_id_error: None,
            quitting: false,
            visible: !start_hidden,
            fullscreen: false,
            tray_wanted,
            tray,
            icon: None,
            title: String::new(),
        };
        state.add_tab();
        state
    }

    /// The window has opened.
    fn attach(&mut self, cc: &eframe::CreationContext<'_>) {
        self.waker.set(Some(cc.egui_ctx.clone()));
        if let Some(storage) = cc.storage {
            self.import_dir = storage.get_string(IMPORT_DIR_KEY).map(PathBuf::from);
            self.export_dir = storage.get_string(EXPORT_DIR_KEY).map(PathBuf::from);
        }
        // Ctrl+Q is ours: it asks first while an import or export runs.
        cc.egui_ctx.options_mut(|o| o.quit_shortcuts.clear());
        if self.tray_wanted && self.tray.is_none() && !TRAY_THREAD {
            self.tray_wanted = false;
            self.tray = tray::Tray::create(self.tx.clone(), self.waker.clone());
        }
        if self.options.minimized && !self.options.tray {
            cc.egui_ctx
                .send_viewport_cmd(ViewportCommand::Minimized(true));
        }
        // Started with --minimized: in the tray from the start.
        if !self.visible
            && let Some(tray) = &self.tray
        {
            tray.set_shown(false);
        }
    }

    fn add_tab(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab::new(id));
        self.tabs.len() - 1
    }

    fn new_tab(&mut self) {
        self.active = self.add_tab();
        self.focus_search = true;
    }

    /// A blank tab to type in: this one if it's blank already.
    fn new_search(&mut self) {
        if !self.tabs[self.active].query.trim().is_empty() {
            self.new_tab();
        }
        self.focus_search = true;
    }

    fn close_tab(&mut self, index: usize) {
        if self.tabs.len() == 1 {
            // The last tab is emptied rather than closed.
            let id = self.tabs[0].id;
            self.tabs[0] = Tab::new(id);
        } else {
            self.tabs.remove(index);
            if self.active > index || self.active == self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        }
        self.focus_search = true;
    }

    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    // ── Searching ─────────────────────────────────────────────────────

    fn search(&mut self, index: usize, append: bool) {
        let tab = &mut self.tabs[index];
        if !append {
            tab.searched = tab.query.trim().to_string();
            tab.generation += 1;
            tab.error = None;
            tab.edited = None;
            if tab.searched.is_empty() {
                tab.results.clear();
                tab.total = 0;
                tab.loading = false;
                tab.replacing = false;
                return;
            }
            tab.replacing = !tab.results.is_empty();
            tab.counted = false; // the count was for the previous query
            tab.page = None;
            // `cache:URL`: one stored page instead of results.
            if let Some(url) = crate::query::cache_url(&tab.searched) {
                tab.results.clear();
                tab.total = 0;
                tab.replacing = false;
                tab.scroll_to_top = true;
                let url = match url {
                    Ok(url) => url,
                    Err(e) => {
                        tab.loading = false;
                        tab.error = Some(sentence(&e));
                        return;
                    }
                };
                tab.loading = true;
                let (id, generation) = (tab.id, tab.generation);
                let highlight = (!tab.highlight.is_empty()).then(|| tab.highlight.clone());
                let (lost, tx, waker) = (self.lost.clone(), self.tx.clone(), self.waker.clone());
                std::thread::spawn(move || {
                    let result = lost
                        .page(&url, highlight.as_deref())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Event::Page {
                        tab: id,
                        generation,
                        result,
                    });
                    waker.wake();
                });
                return;
            }
        }
        tab.loading = true;
        let (id, generation, query) = (tab.id, tab.generation, tab.searched.clone());
        let offset = if append { tab.results.len() } else { 0 };
        self.spawn_search(id, generation, query, Limit::new(offset, PAGE), append);
    }

    /// Searches on a thread of its own; the answer comes back as an event
    /// tagged with the tab's `id` and `generation`.
    fn spawn_search(&self, id: u64, generation: u64, query: String, limit: Limit, append: bool) {
        let (lost, tx, waker) = (self.lost.clone(), self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let result = lost.search(&query, limit).map_err(|e| e.to_string());
            let _ = tx.send(Event::Searched {
                tab: id,
                generation,
                append,
                result,
            });
            waker.wake();
        });
    }

    /// Opens the quick search popup (or brings it back), under or over the
    /// tray icon when its position is known, else near the top of the
    /// screen.
    fn open_quick(&mut self, ctx: &egui::Context, icon: Option<[f32; 4]>) {
        if let Some(popup) = &mut self.quick {
            popup.raise();
            ctx.request_repaint();
            return;
        }
        let (ppp, monitor, minimized) = ctx.input(|i| {
            (
                i.viewport().native_pixels_per_point.unwrap_or(1.0),
                i.viewport().monitor_size,
                i.viewport().minimized == Some(true),
            )
        });
        // Nothing is drawn while the main window is minimized, the popup
        // included: it goes to the tray instead (where the popup came from).
        if minimized {
            self.hide_window(ctx);
        }
        let [w, h] = crate::quick::SIZE;
        let position = match (icon, monitor) {
            (Some([x, y, iw, ih]), Some(m)) => {
                let (x, y, iw, ih) = (x / ppp, y / ppp, iw / ppp, ih / ppp);
                let left = (x + iw / 2.0 - w / 2.0).clamp(0.0, (m.x - w).max(0.0));
                // A tray at the top (macOS, some Linux panels): below it;
                // at the bottom (Windows): above it.
                let top = if y < m.y / 2.0 {
                    y + ih + 4.0
                } else {
                    (y - h - 4.0).max(0.0)
                };
                Some(egui::pos2(left, top))
            }
            (None, Some(m)) => Some(egui::pos2(((m.x - w) / 2.0).max(0.0), m.y * 0.18)),
            _ => None,
        };
        self.quick = Some(crate::quick::Popup::new(self.lost.clone(), position));
        if !self.visible {
            // No window of its own can be made now: the main one becomes it.
            self.begin_stand_in(ctx, StandIn::Quick, crate::quick::SIZE.into(), position);
        }
        ctx.request_repaint();
    }

    /// The main window, while hidden, standing in for `kind` (the quick
    /// search popup, Settings or About): windows of their own can't be made
    /// then (see `separate`). Another one it stood in for is closed.
    fn begin_stand_in(
        &mut self,
        ctx: &egui::Context,
        kind: StandIn,
        size: egui::Vec2,
        position: Option<egui::Pos2>,
    ) {
        let geometry = match self.stand_in.take() {
            Some((old, geometry)) => {
                if old != kind {
                    self.close_stood_in(old);
                }
                geometry
            }
            None => {
                let geometry = self.geometry.unwrap_or(Geometry {
                    size: egui::vec2(920.0, 720.0),
                    position: None,
                    maximized: false,
                });
                if geometry.maximized {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                }
                geometry
            }
        };
        let popup = kind == StandIn::Quick;
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Decorations(!popup));
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if popup {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        }));
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        if let Some(position) = position {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(position));
        }
        ctx.send_viewport_cmd(ViewportCommand::Title(
            match kind {
                StandIn::Quick => "LOST quick search",
                StandIn::Settings => "LOST Settings",
                StandIn::About => "About LOST",
            }
            .to_string(),
        ));
        self.title.clear(); // the main window's title goes back when it shows
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.stand_in = Some((kind, geometry));
    }

    /// Closes what the main window stood in for (only the popup when the
    /// main window shows: Settings and About then get windows of their own).
    fn close_stood_in(&mut self, kind: StandIn) {
        match kind {
            StandIn::Quick => self.quick = None,
            StandIn::Settings => {
                if self.settings.is_some() {
                    self.save_config();
                    self.config.loaded = false;
                    self.answer(Answer::No);
                    self.settings = None;
                }
            }
            StandIn::About => self.about = false,
        }
    }

    /// The main window, back from standing in for another: hidden, as it
    /// was, with its size and place.
    fn end_stand_in(&mut self, ctx: &egui::Context) {
        if let Some((kind, geometry)) = self.stand_in.take() {
            if kind == StandIn::Quick {
                self.quick = None;
            }
            self.geometry_after = Some(Instant::now() + Duration::from_secs(1));
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            ctx.send_viewport_cmd(ViewportCommand::Decorations(true));
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(geometry.size));
            if let Some(at) = geometry.position {
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(at));
            }
            if geometry.maximized {
                ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
            }
        }
    }

    /// The main window standing in for another: only that one is drawn.
    fn stand_in_ui(&mut self, ui: &mut egui::Ui, kind: StandIn) {
        let ctx = &ui.ctx().clone();
        let close = ctx.input(|i| i.viewport().close_requested());
        if close {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        }
        match kind {
            StandIn::Quick => {
                let outcome = match &mut self.quick {
                    Some(popup) => popup.window(ui),
                    None => Some(crate::quick::Outcome::Close),
                };
                if let Some(outcome) = outcome {
                    self.end_stand_in(ctx);
                    self.quick_outcome(ctx, outcome);
                }
            }
            StandIn::Settings => {
                if std::mem::take(&mut self.raise_settings) {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                if let Some(mut page) = self.settings {
                    self.settings_refresh(ctx, page);
                    egui::CentralPanel::default().show(ui, |ui| self.settings_body(ui, &mut page));
                    let dropped = dropped_paths(ctx);
                    drop_hint(ctx);
                    self.settings_done(page, !close, dropped);
                }
                if self.settings.is_none() {
                    self.end_stand_in(ctx);
                }
            }
            StandIn::About => {
                if close {
                    self.about = false;
                }
                if self.about {
                    egui::CentralPanel::default().show(ui, |ui| self.about_body(ui));
                } else {
                    self.end_stand_in(ctx);
                }
            }
        }
    }

    /// What the popup asked for, once it's closed.
    fn quick_outcome(&mut self, ctx: &egui::Context, outcome: crate::quick::Outcome) {
        match outcome {
            crate::quick::Outcome::Close => {}
            crate::quick::Outcome::Open(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            crate::quick::Outcome::InLost(query) => self.show_search(ctx, query),
        }
    }

    /// Shows `query` in a new tab (unless this one is empty) and the window.
    fn show_search(&mut self, ctx: &egui::Context, query: String) {
        if !self.tabs[self.active].query.trim().is_empty() {
            self.new_tab();
        }
        let i = self.active;
        self.tabs[i].query = query;
        self.search(i, false);
        self.show_window(ctx);
    }

    fn suggest(&mut self, index: usize) {
        let tab = &self.tabs[index];
        let text = tab.query.clone();
        if text.trim().is_empty() {
            return;
        }
        let (id, lost, tx, waker) = (
            tab.id,
            self.lost.clone(),
            self.tx.clone(),
            self.waker.clone(),
        );
        std::thread::spawn(move || {
            let items = lost.suggest(&text, SUGGESTIONS).unwrap_or_default();
            let _ = tx.send(Event::Suggested {
                tab: id,
                text,
                items,
            });
            waker.wake();
        });
    }

    // ── Importing ─────────────────────────────────────────────────────

    fn start_import(&mut self, paths: Vec<PathBuf>) {
        self.open_settings(Page::Data);
        if paths.is_empty() {
            return;
        }
        if self.importing.load(Ordering::Relaxed) {
            self.import.message = Some("Another import is running; wait for it to finish.".into());
            return;
        }
        if self.export.running {
            self.import.message = Some("An export is running; wait for it to finish.".into());
            return;
        }
        if !self.import.removing.is_empty() {
            self.import.message =
                Some("Imports are being removed; wait for that to finish.".into());
            return;
        }
        // The dialogs open here next time: where these files are (for a
        // folder, where it is, so it can be picked again).
        if let Some(parent) = paths[0].parent() {
            self.import_dir = Some(parent.to_path_buf());
        }
        self.importing.store(true, Ordering::Relaxed);
        self.import.files = paths.iter().map(|p| p.display().to_string()).collect();
        self.import.records = 0;
        self.import.started = Some(Instant::now());
        self.import.run.clear();
        self.import.message = None;
        let names: Vec<String> = paths
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string())
            })
            .collect();
        self.log(
            LogKind::Info,
            format!("Importing {}…", crate::text::files_summary(&names)),
        );

        let trust = self.options.config.trust();
        let (lost, tx, waker) = (self.lost.clone(), self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            // Questions go to the window, and this thread waits for the answer.
            let mut ask = |question: &Question| {
                let (reply, answer) = mpsc::channel();
                let _ = tx.send(Event::Ask(question.clone(), reply));
                waker.wake();
                answer.recv().unwrap_or(Answer::No)
            };
            let mut last = Instant::now();
            let mut progress = |p: Progress| {
                // Counts only now and then; the rest every time.
                if matches!(p, Progress::Records(_)) {
                    if last.elapsed() < Duration::from_millis(100) {
                        return;
                    }
                    last = Instant::now();
                }
                let _ = tx.send(Event::ImportProgress(p));
                waker.wake();
            };
            let result = match lost.import(&paths, trust, &mut ask, &mut progress) {
                // Said no: nothing to report.
                Err(e) if e.downcast_ref::<Declined>().is_some() => Ok(Vec::new()),
                other => other.map_err(|e| e.to_string()),
            };
            let _ = tx.send(Event::Imported(result));
            waker.wake();
        });
    }

    /// Answers the waiting question about a `.lost` file.
    fn answer(&mut self, answer: Answer) {
        if let Some((_, reply)) = self.import.question.take() {
            let _ = reply.send(answer);
        }
    }

    // ── File dialogs ──────────────────────────────────────────────────

    /// Opens a file dialog on a thread of its own; what's picked comes
    /// back as `Event::Picked`.
    fn open_dialog(&mut self, dialog: Dialog) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        // Where the same kind of dialog was last time, if it's still there.
        let dir = match &dialog {
            Dialog::ImportFiles | Dialog::ImportFolder => self.import_dir.clone(),
            Dialog::Export { .. } => self.export_dir.clone(),
            Dialog::DataDir => None,
        }
        .filter(|d| d.is_dir());
        let (tx, waker) = (self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let new = || match &dir {
                Some(dir) => rfd::AsyncFileDialog::new().set_directory(dir),
                None => rfd::AsyncFileDialog::new(),
            };
            let picked: Vec<PathBuf> = block_on(async {
                let handles = match &dialog {
                    Dialog::ImportFiles => new()
                        .set_title("Import into LOST")
                        .pick_files()
                        .await
                        .unwrap_or_default(),
                    Dialog::ImportFolder => new()
                        .set_title("Import a folder into LOST")
                        .pick_folder()
                        .await
                        .into_iter()
                        .collect(),
                    Dialog::Export { name, .. } => new()
                        .set_title("Export from LOST")
                        .set_file_name(format!("{name}.lost"))
                        .add_filter("LOST dataset", &["lost"])
                        .save_file()
                        .await
                        .into_iter()
                        .collect(),
                    Dialog::DataDir => rfd::AsyncFileDialog::new()
                        .set_title("LOST data directory")
                        .pick_folder()
                        .await
                        .into_iter()
                        .collect(),
                };
                handles.iter().map(|h| h.path().to_path_buf()).collect()
            });
            let _ = tx.send(Event::Picked(dialog, picked));
            waker.wake();
        });
    }

    /// What a closed file dialog picked.
    fn picked(&mut self, dialog: Dialog, paths: Vec<PathBuf>) {
        self.dialog_open = false;
        if paths.is_empty() {
            return; // cancelled
        }
        match dialog {
            Dialog::ImportFiles | Dialog::ImportFolder => self.start_import(paths),
            Dialog::Export { imports, sign, .. } => {
                let mut path = paths[0].clone();
                if path.extension().is_none_or(|e| e != "lost") {
                    let mut with = path.into_os_string();
                    with.push(".lost");
                    path = PathBuf::from(with);
                }
                self.export_dir = path.parent().map(Path::to_path_buf);
                self.start_export(path, imports, sign);
            }
            Dialog::DataDir => self.config.data_dir = paths[0].display().to_string(),
        }
    }

    // ── Exporting ─────────────────────────────────────────────────────

    /// Exports to `path` on a thread of its own: everything, or the pages of
    /// `imports`.
    fn start_export(&mut self, path: PathBuf, imports: Vec<u64>, sign: bool) {
        if self.working() {
            self.export.message = Some(Err(
                "Something else is running; wait for it to finish.".into()
            ));
            return;
        }
        self.export.message = None;
        self.export.running = true;
        self.export.done = 0;
        self.export.total = 0;
        self.export.sign = false; // unticked again next time
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.log(
            LogKind::Info,
            match imports.len() {
                0 => format!("Exporting everything to {name}…"),
                1 => format!("Exporting import #{} to {name}…", imports[0]),
                n => format!("Exporting {n} imports to {name}…"),
            },
        );

        let (lost, tx, waker) = (self.lost.clone(), self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let run = || -> Result<String> {
                let identity = if sign {
                    let dir = Identity::default_dir()
                        .ok_or("There's no home folder to keep your signing key in.")?;
                    Some(Identity::load_or_create(&dir)?.0)
                } else {
                    None
                };
                let mut progress = |done, total| {
                    let _ = tx.send(Event::ExportProgress(done, total));
                    waker.wake();
                };
                let selection = if imports.is_empty() {
                    crate::Selection::All
                } else {
                    crate::Selection::Imports(&imports)
                };
                let export = lost.export(&path, selection, identity.as_ref(), &mut progress)?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Ok(format!(
                    "Saved {} page{} to {name} ({}).",
                    thousands(export.pages),
                    if export.pages == 1 { "" } else { "s" },
                    format_bytes(export.bytes)
                ))
            };
            let result = run().map_err(|e| sentence(&e.to_string()));
            let _ = tx.send(Event::Exported(result));
            waker.wake();
        });
    }

    /// Reads who's trusted (not while an import holds the app database).
    fn load_trusted(&mut self) {
        if !self.importing.load(Ordering::Relaxed) {
            match self.lost.trusted() {
                Ok(rows) => self.trusted = Some(Ok(rows)),
                // Another LOST may hold the app database for a moment: keep
                // what's shown.
                Err(_) if matches!(self.trusted, Some(Ok(_))) => {}
                Err(e) => self.trusted = Some(Err(e.to_string())),
            }
        }
    }

    fn load_history(&mut self) {
        self.import.size = Some(self.lost.disk_usage());
        self.export.id = Identity::default_dir()
            .and_then(|dir| Identity::load(&dir).ok().flatten())
            .map(|identity| identity.id());
        // The app database is busy for the whole of an import or removal.
        if !self.importing.load(Ordering::Relaxed) && self.import.removing.is_empty() {
            match self.lost.imports() {
                Ok(rows) => {
                    // Ticks for imports that are gone go too.
                    self.import
                        .selected
                        .retain(|id| rows.iter().any(|r| r.id == *id));
                    self.import.history = Some(Ok(rows));
                }
                // Another LOST may hold it (an import from the command line):
                // keep what's shown.
                Err(_) if matches!(self.import.history, Some(Ok(_))) => {}
                Err(e) => self.import.history = Some(Err(e.to_string())),
            }
        }
        self.settings_read = Some(Instant::now());
    }

    /// Opens the Settings window on `page`, or brings it to the front if
    /// it's already open behind other windows.
    fn open_settings(&mut self, page: Page) {
        if page == Page::Data && self.settings != Some(Page::Data) {
            self.load_history();
        }
        if self.settings.is_none() {
            self.load_trusted();
            self.export.sign = false;
        }
        self.settings = Some(page);
        self.raise_settings = true;
    }

    // ── Window and quitting ───────────────────────────────────────────

    fn show_window(&mut self, ctx: &egui::Context) {
        // Settings or About it stood in for get windows of their own.
        self.end_stand_in(ctx);
        self.visible = true;
        if let Some(tray) = &self.tray {
            tray.set_shown(true);
        }
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.visible = false;
        if let Some(tray) = &self.tray {
            tray.set_shown(false);
        }
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    /// An import or export is running.
    fn busy(&self) -> bool {
        self.importing.load(Ordering::Relaxed) || self.export.running
    }

    /// Quits, unless an import or export is running: then asks first.
    fn request_quit(&mut self, ctx: &egui::Context) {
        if self.busy() {
            self.confirm_quit = true;
            if !self.visible {
                self.show_window(ctx);
            }
        } else {
            self.quit(ctx);
        }
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.end_stand_in(ctx);
        self.quitting = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    // ── Events from elsewhere ─────────────────────────────────────────

    /// Applies an event; `ctx` is the open window, if any.
    fn apply(&mut self, event: Event, ctx: Option<&egui::Context>) {
        match event {
            Event::Searched {
                tab,
                generation,
                append,
                result,
            } => {
                let Some(i) = self.tab_index(tab) else { return };
                let t = &mut self.tabs[i];
                if t.generation != generation {
                    return; // the query has changed since
                }
                t.loading = false;
                t.replacing = false;
                match result {
                    Ok(r) => {
                        t.total = r.total;
                        t.took_ms = r.took_ms;
                        t.counted = true;
                        if append {
                            if r.results.is_empty() {
                                t.total = t.results.len(); // nothing more after all
                            }
                            t.results.extend(r.results);
                        } else {
                            t.results = r.results;
                            t.scroll_to_top = true;
                        }
                    }
                    Err(e) => t.error = Some(e),
                }
            }
            Event::Page {
                tab,
                generation,
                result,
            } => {
                let Some(i) = self.tab_index(tab) else { return };
                let t = &mut self.tabs[i];
                if t.generation != generation {
                    return;
                }
                t.loading = false;
                match result {
                    Ok(Some(page)) => t.page = Some(Stored::Found(Box::new(page))),
                    Ok(None) => t.page = Some(Stored::Missing),
                    Err(e) => t.error = Some(e),
                }
            }
            Event::Suggested { tab, text, items } => {
                let Some(i) = self.tab_index(tab) else { return };
                let t = &mut self.tabs[i];
                if t.query == text {
                    t.suggestions = items.into_iter().filter(|s| s != text.trim()).collect();
                    t.selected = None;
                }
            }
            Event::ImportProgress(p) => self.import_progress(p),
            Event::Ask(question, reply) => {
                // Shown in Settings → Data.
                self.import.question = Some((question, reply));
                self.open_settings(Page::Data);
                if let Some(ctx) = ctx
                    && !self.visible
                {
                    self.show_window(ctx);
                }
            }
            Event::Picked(dialog, paths) => self.picked(dialog, paths),
            Event::ExportProgress(done, total) => {
                self.export.done = done;
                self.export.total = total;
            }
            Event::Exported(result) => {
                self.export.running = false;
                match &result {
                    Ok(m) => self.log(LogKind::Done, m),
                    Err(e) => self.log(LogKind::Error, e),
                }
                self.export.message = Some(result);
                self.load_history();
            }
            Event::Removed(results) => {
                self.import.removing.clear();
                self.import.remove_error = None;
                for result in results {
                    match result {
                        Ok((id, pages)) => self.log(
                            LogKind::Done,
                            format!(
                                "Removed import #{id}: {} page{}.",
                                thousands(pages),
                                if pages == 1 { "" } else { "s" }
                            ),
                        ),
                        Err(e) => {
                            self.log(LogKind::Error, sentence(&e));
                            self.import.remove_error = Some(sentence(&e));
                        }
                    }
                }
                self.load_history();
                for i in 0..self.tabs.len() {
                    if !self.tabs[i].searched.is_empty() {
                        self.search(i, false);
                    }
                }
            }
            Event::Imported(result) => {
                self.importing.store(false, Ordering::Relaxed);
                self.import.started = None;
                self.import.question = None;
                match result {
                    Ok(rows) => {
                        for row in rows {
                            self.log(
                                LogKind::Done,
                                format!(
                                    "Imported {} as import #{}: {} page{} — {} new, {} replaced older, {} already there.",
                                    row.files.join(", "),
                                    row.id,
                                    thousands(row.records),
                                    if row.records == 1 { "" } else { "s" },
                                    thousands(row.new),
                                    thousands(row.updated),
                                    thousands(row.unchanged),
                                ),
                            );
                            for note in row.notes {
                                self.log(LogKind::Note, note);
                            }
                        }
                    }
                    Err(e) => {
                        self.log(LogKind::Error, sentence(&e));
                        self.import.message = Some(sentence(&e));
                    }
                }
                // The table shows the app database again, this run's imports in it.
                self.import.run.clear();
                self.load_history();
                self.load_trusted();
                // Results may have changed: search the open tabs again.
                for i in 0..self.tabs.len() {
                    if !self.tabs[i].searched.is_empty() {
                        self.search(i, false);
                    }
                }
            }
            Event::Raise
            | Event::ToggleWindow
            | Event::NewSearch
            | Event::Settings
            | Event::About
            | Event::QuickSearch(_)
            | Event::Quit => {
                let Some(ctx) = ctx else { return };
                match event {
                    Event::Raise => self.show_window(ctx),
                    Event::ToggleWindow => {
                        if self.visible {
                            self.hide_window(ctx);
                        } else {
                            self.show_window(ctx);
                        }
                    }
                    Event::NewSearch => {
                        self.new_search();
                        self.show_window(ctx);
                    }
                    // While the main window is hidden, it stands in for these.
                    Event::Settings => {
                        self.open_settings(Page::Configuration);
                        if !self.visible {
                            self.begin_stand_in(ctx, StandIn::Settings, SETTINGS_SIZE.into(), None);
                        }
                    }
                    Event::About => {
                        self.about = true;
                        if !self.visible {
                            self.begin_stand_in(ctx, StandIn::About, ABOUT_SIZE.into(), None);
                        }
                    }
                    Event::QuickSearch(icon) => self.open_quick(ctx, icon),
                    _ => self.request_quit(ctx),
                }
            }
        }
    }

    // ── Keyboard ──────────────────────────────────────────────────────

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (prev, next, new, close, quit, full, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab),
                i.consume_key(Modifiers::CTRL, Key::Tab),
                i.consume_key(Modifiers::COMMAND, Key::T),
                i.consume_key(Modifiers::COMMAND, Key::W),
                i.consume_key(Modifiers::COMMAND, Key::Q),
                i.consume_key(Modifiers::COMMAND, Key::F)
                    || i.consume_key(Modifiers::NONE, Key::F11),
                i.key_pressed(Key::Escape),
            )
        });
        let n = self.tabs.len();
        if next {
            self.active = (self.active + 1) % n;
            self.focus_search = true;
        }
        if prev {
            self.active = (self.active + n - 1) % n;
            self.focus_search = true;
        }
        if new {
            self.new_tab();
        }
        if close {
            self.close_tab(self.active);
        }
        if quit {
            self.request_quit(ctx);
        }
        if full {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
        }
        let tab = &mut self.tabs[self.active];
        if esc && !self.confirm_quit {
            if tab.selected.is_some() {
                tab.selected = None; // first Esc: leave the suggestions
            } else if self.fullscreen {
                self.fullscreen = false;
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
            } else if self.tray.is_some() {
                self.hide_window(ctx);
            }
        }
    }

    // ── Main window ───────────────────────────────────────────────────

    /// Events and timers: run before every frame, and while the window is
    /// hidden too.
    fn logic(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply(event, Some(ctx));
        }
        self.timers(ctx);
    }

    fn update(&mut self, ui: &mut egui::Ui) {
        let ctx = &ui.ctx().clone();

        // The main window standing in for another: only that one.
        if let Some((kind, _)) = self.stand_in {
            self.stand_in_ui(ui, kind);
            return;
        }

        // Its size and place, while it's on screen as itself.
        if self.visible && self.geometry_after.is_none_or(|t| Instant::now() >= t) {
            let seen = ctx.input(|i| {
                let v = i.viewport();
                (v.minimized != Some(true))
                    .then_some(())
                    .and(v.inner_rect)
                    .map(|inner| Geometry {
                        size: inner.size(),
                        position: v.outer_rect.map(|r| r.min),
                        maximized: v.maximized == Some(true),
                    })
            });
            if let Some(seen) = seen {
                // A maximized window's own size is the one to go back to.
                self.geometry = Some(match (seen.maximized, self.geometry) {
                    (true, Some(before)) => Geometry {
                        maximized: true,
                        ..before
                    },
                    _ => seen,
                });
            }
        }

        // Closing the window: keep running in the tray, or quit.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            if self.tray.is_some() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.hide_window(ctx);
            } else if self.busy() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.confirm_quit = true;
            } else {
                self.quitting = true;
            }
        }

        self.shortcuts(ctx);

        let title = match self.tabs[self.active].searched.as_str() {
            "" => "LOST".to_string(),
            q => format!("{q} – LOST"),
        };
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            self.tab_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::top("search").show(ui, |ui| {
            ui.add_space(6.0);
            self.search_row(ui);
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ui, |ui| self.results(ui));

        let dropped = dropped_paths(ctx);
        if !dropped.is_empty() {
            self.start_import(dropped);
        }
        drop_hint(ctx);
        separate(|| self.settings_window(ctx));
        separate(|| self.about_window(ctx));
        let mut outcome = None;
        if let Some(popup) = &mut self.quick {
            separate(|| outcome = popup.show(ctx));
        }
        if let Some(outcome) = outcome {
            self.quick = None;
            self.quick_outcome(ctx, outcome);
        }
        self.quit_dialog(ctx);
    }

    /// Searches and suggestions wait for a pause in typing.
    fn timers(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut wake: Option<Duration> = None;
        let mut soonest =
            |left: Duration| wake = Some(wake.map_or(left, |w: Duration| w.min(left)));
        for i in 0..self.tabs.len() {
            if let Some(at) = self.tabs[i].suggest_at {
                if now >= at + SUGGEST_DELAY {
                    self.tabs[i].suggest_at = None;
                    self.suggest(i);
                } else {
                    soonest(at + SUGGEST_DELAY - now);
                }
            }
            if let Some(at) = self.tabs[i].edited {
                if now >= at + SEARCH_DELAY {
                    self.tabs[i].edited = None;
                    if self.tabs[i].query.trim() != self.tabs[i].searched {
                        self.search(i, false);
                    }
                } else {
                    soonest(at + SEARCH_DELAY - now);
                }
            }
        }
        if let Some(w) = wake {
            ctx.request_repaint_after(w);
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut select = None;
        let mut close = None;
        let mut new = false;
        let mut menu = None;
        // Top-aligned: centred, the row would claim the panel's whole height,
        // and the panel would grow to match, frame after frame.
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            // ☰ as tall as "+" and the tabs.
            ui.spacing_mut().interact_size.y = TAB_HEIGHT;
            // The ☰ glyph sits high in most fonts; the bars are drawn
            // instead, centred, over an invisible one of the same size.
            let hamburger = ui.menu_button(
                RichText::new("☰").size(18.0).color(Color32::TRANSPARENT),
                |ui| {
                    if ui.button("Settings").clicked() {
                        menu = Some("settings");
                        ui.close();
                    }
                    if ui.button("About").clicked() {
                        menu = Some("about");
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        menu = Some("quit");
                        ui.close();
                    }
                },
            );
            let button = &hamburger.response;
            let color = if button.hovered() {
                ui.visuals().widgets.hovered.fg_stroke.color
            } else {
                ui.visuals().widgets.inactive.fg_stroke.color
            };
            let center = button.rect.center();
            for dy in [-5.0, 0.0, 5.0] {
                ui.painter().line_segment(
                    [
                        egui::pos2(center.x - 8.0, center.y + dy),
                        egui::pos2(center.x + 8.0, center.y + dy),
                    ],
                    egui::Stroke::new(2.0_f32, color),
                );
            }
            // "+" stays put at the right; the tabs share the rest equally.
            let plus = egui::Button::new(RichText::new("+").size(16.0))
                .min_size(egui::vec2(48.0, TAB_HEIGHT));
            if ui.add(plus).on_hover_text("New tab (Ctrl+T)").clicked() {
                new = true;
            }
            ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                // Gaps narrow too once tabs get crowded.
                let gap = if self.tabs.len() > 8 { 2.0 } else { 6.0 };
                ui.spacing_mut().item_spacing = egui::vec2(gap, gap);
                let n = self.tabs.len() as f32;
                let room = ui.available_width() - 1.0;
                let width = ((room - gap * (n - 1.0)) / n).floor().max(MIN_TAB_WIDTH);
                // A lone empty tab has nothing to close.
                let lone = self.tabs.len() == 1;
                for (i, tab) in self.tabs.iter().enumerate() {
                    let closable =
                        !(lone && tab.query.trim().is_empty() && tab.searched.is_empty());
                    let (clicked, closed) = tab_button(ui, tab, i == self.active, width, closable);
                    if closed {
                        close = Some(i);
                    } else if clicked {
                        select = Some(i);
                    }
                }
            });
        });
        if let Some(i) = select {
            self.active = i;
            self.focus_search = true;
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
        if new {
            self.new_tab();
        }
        match menu {
            Some("settings") => self.open_settings(Page::Configuration),
            Some("about") => self.about = true,
            Some("quit") => {
                let ctx = ui.ctx().clone();
                self.request_quit(&ctx);
            }
            _ => {}
        }
    }

    /// The search box, with suggestions beside it (wide windows) or on a
    /// line of their own under it, which stays even when empty, so nothing
    /// moves as they come and go.
    fn search_row(&mut self, ui: &mut egui::Ui) {
        let index = self.active;
        let focus = std::mem::take(&mut self.focus_search);
        let width = ui.available_width();
        let beside = width >= SUGGESTIONS_BESIDE;
        let box_width = if beside {
            (width * 0.55).clamp(380.0, 640.0)
        } else {
            width
        };

        // Suggestions only while the search box is in use, so they don't
        // distract from the results.
        let show = focus || self.search_focused || self.pills_hovered;
        let mut pills_hovered = false;

        let tab = &mut self.tabs[index];
        // ↓/↑ pick a suggestion, Enter searches it.
        let has = show && !tab.suggestions.is_empty();
        let (down, up) = ui.input_mut(|i| {
            if has {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                )
            } else {
                (false, false)
            }
        });

        let mut chosen = None;
        let mut response = None;
        ui.horizontal(|ui| {
            let edit = egui::TextEdit::singleline(&mut tab.query)
                .hint_text("Search offline")
                .font(FontId::proportional(18.0))
                .margin(egui::vec2(10.0, 8.0))
                .desired_width(box_width);
            response = Some(ui.add(edit));
            if beside && show {
                ui.add_space(8.0);
                (chosen, pills_hovered) = pills(ui, &tab.suggestions, tab.selected);
            }
        });
        if !beside {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.set_min_height(PILL_SIZE + 12.0);
                if show {
                    (chosen, pills_hovered) = pills(ui, &tab.suggestions, tab.selected);
                }
            });
        }
        let Some(response) = response else { return };
        self.search_focused = response.has_focus();
        self.pills_hovered = pills_hovered;
        if focus {
            response.request_focus();
        }

        let n = tab.suggestions.len();
        if n > 0 && (down || up) {
            let next = match (tab.selected, down) {
                (None, true) => 0,
                (None, false) => n - 1,
                (Some(s), true) => (s + 1) % n,
                (Some(s), false) => (s + n - 1) % n,
            };
            tab.selected = Some(next);
        }
        if response.changed() {
            let now = Instant::now();
            tab.highlight.clear(); // typed by hand: nothing carried over to mark
            tab.edited = Some(now);
            tab.suggest_at = Some(now);
            tab.selected = None;
            if tab.query.trim().is_empty() {
                tab.suggestions.clear();
            }
        }
        if response.gained_focus() && !tab.query.trim().is_empty() && tab.suggestions.is_empty() {
            tab.suggest_at = Some(Instant::now() - SUGGEST_DELAY);
        }
        let entered = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if entered {
            if let Some(s) = tab.selected {
                chosen = tab.suggestions.get(s).cloned();
            }
            response.request_focus();
        }
        if let Some(text) = chosen {
            // The cursor goes to the end of the new text.
            let end = egui::text::CCursor::new(text.chars().count());
            let id = response.id;
            if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), id) {
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(end, end)));
                state.store(ui.ctx(), id);
            }
            tab.query = text;
            tab.selected = None;
            tab.suggest_at = Some(Instant::now());
            self.focus_search = true;
            self.search(index, false);
        } else if entered {
            self.search(index, false);
        }
    }

    fn results(&mut self, ui: &mut egui::Ui) {
        let index = self.active;
        if crate::query::cache_url(&self.tabs[index].searched).is_some() {
            return self.stored_page(ui);
        }
        let is_empty = self.lost.is_empty();
        let tab = &mut self.tabs[index];
        let visuals = ui.visuals().clone();
        let weak = visuals.weak_text_color();

        let status = if let Some(e) = &tab.error {
            RichText::new(e).color(visuals.error_fg_color)
        } else if tab.searched.is_empty() {
            RichText::new(if is_empty {
                "Nothing to search yet. Import a dataset: ☰ → Settings → Data, or drop files here."
            } else {
                ""
            })
            .color(weak)
        } else if tab.total == 0 && !tab.loading {
            RichText::new(format!("Found no results for “{}”", tab.searched)).color(weak)
        } else if tab.total > 0 {
            RichText::new(format!(
                "Found {} result{} ({} ms)",
                thousands(tab.total as u64),
                if tab.total == 1 { "" } else { "s" },
                tab.took_ms
            ))
            .color(weak)
            .small()
        } else {
            RichText::new("")
        };
        ui.horizontal(|ui| {
            ui.label(status);
            if tab.loading {
                ui.spinner();
            }
        });
        ui.add_space(6.0);

        let mut style = ResultStyle::new(ui);
        style.cached_link = self.options.config.search.cached_links;
        let row = style.row_height(ui);
        let fade = tab.loading && tab.replacing;

        let mut scroll = egui::ScrollArea::vertical()
            .id_salt(("results", tab.id))
            .auto_shrink([false, false]);
        if std::mem::take(&mut tab.scroll_to_top) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let mut action = None;
        let results = &tab.results;
        // Only the rows in view are laid out and drawn.
        let output = scroll.show_rows(ui, row, results.len(), |ui, range| {
            if fade {
                ui.multiply_opacity(0.4);
            }
            let width = ui.available_width();
            for hit in &results[range] {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row), Sense::hover());
                let mut cell = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(Layout::top_down(Align::Min)),
                );
                if let Some(a) = style.draw(&mut cell, hit) {
                    action = Some(a);
                }
            }
        });

        match action {
            Some(ResultAction::Open(url)) => ui.ctx().open_url(egui::OpenUrl::new_tab(url)),
            Some(ResultAction::Cached(url)) => {
                // In a new tab, with this search's words marked.
                let words = self.tabs[index].searched.clone();
                self.new_tab();
                let i = self.active;
                self.tabs[i].query = format!("cache:{url}");
                self.tabs[i].highlight = words;
                self.search(i, false);
                return;
            }
            None => {}
        }

        // Infinite scrolling.
        let tab = &self.tabs[index];
        let near_end = output.state.offset.y + output.inner_rect.height() + LOAD_AHEAD
            >= output.content_size.y;
        if near_end
            && !tab.loading
            && !tab.searched.is_empty()
            && tab.results.len() < tab.total
            && tab.error.is_none()
        {
            self.search(index, true);
        }
    }

    /// A tab's `cache:URL`: the page's text as stored, like a text-mode
    /// browser shows it (the HTML was never kept).
    fn stored_page(&mut self, ui: &mut egui::Ui) {
        let index = self.active;
        let tab = &mut self.tabs[index];
        let visuals = ui.visuals().clone();
        let weak = visuals.weak_text_color();
        if let Some(e) = &tab.error {
            ui.colored_label(visuals.error_fg_color, e);
            return;
        }
        let page = match &tab.page {
            None => {
                if tab.loading {
                    ui.spinner();
                }
                return;
            }
            Some(Stored::Missing) => {
                ui.label(RichText::new("This page isn't in the database.").color(weak));
                return;
            }
            Some(Stored::Found(page)) => page,
        };

        let style = ResultStyle::new(ui);
        let fmt = |size: f32, color: Color32, background: Color32| TextFormat {
            font_id: FontId::proportional(size),
            color,
            background,
            ..Default::default()
        };
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt(("page", tab.id))
            .auto_shrink([false, false]);
        if std::mem::take(&mut tab.scroll_to_top) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let mut open = None;
        scroll.show(ui, |ui| {
            // A comfortable column to read, as on the web.
            ui.set_max_width(ui.available_width().min(760.0));
            let width = ui.available_width();
            // About the page, in a box of its own: not part of its text.
            let fill = if visuals.dark_mode {
                Color32::from_gray(18)
            } else {
                Color32::from_gray(236)
            };
            egui::Frame::new()
                .fill(fill)
                .stroke(visuals.widgets.noninteractive.bg_stroke)
                .corner_radius(6.0)
                .inner_margin(egui::vec2(14.0, 12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let inside = ui.available_width();
                    let title = page.title.as_deref().unwrap_or(&page.url);
                    let mut job = highlighted(
                        title,
                        &page.title_matches,
                        fmt(20.0, style.strong, Color32::TRANSPARENT),
                        fmt(20.0, style.strong, style.mark),
                    );
                    job.wrap.max_width = inside;
                    ui.add(egui::Label::new(job));
                    ui.horizontal_wrapped(|ui| {
                        let link = ui
                            .add(
                                egui::Label::new(
                                    RichText::new(&page.url).color(style.url).size(URL_SIZE),
                                )
                                .sense(Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if link.clicked() {
                            open = Some(page.url.clone());
                        }
                        if let Some(date) = &page.crawled {
                            ui.label(
                                RichText::new(format!("Captured {date}"))
                                    .color(weak)
                                    .size(URL_SIZE),
                            );
                        }
                    });
                });
            ui.add_space(14.0);
            let mut job = highlighted(
                &page.text,
                &page.text_matches,
                fmt(SNIPPET_SIZE + 0.5, style.text, Color32::TRANSPARENT),
                fmt(SNIPPET_SIZE + 0.5, style.strong, style.mark),
            );
            job.wrap.max_width = width;
            ui.add(egui::Label::new(job).selectable(true));
            ui.add_space(24.0);
        });
        if let Some(url) = open {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_quit {
            return;
        }
        if !self.busy() {
            // It finished while we were asking.
            self.confirm_quit = false;
            return;
        }
        let importing = self.importing.load(Ordering::Relaxed);
        let mut answer = None;
        egui::Window::new("Quit LOST?")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if importing {
                    ui.label(format!(
                        "An import is running ({} records read so far):\n{}",
                        thousands(self.import.records),
                        self.import.files.join("\n")
                    ));
                    ui.add_space(4.0);
                    ui.label("Quitting stops it. Pages already saved stay in the database; the import is listed as interrupted.");
                } else {
                    ui.label(format!(
                        "An export is running ({} done).",
                        percent(self.export.done, self.export.total)
                    ));
                    ui.add_space(4.0);
                    ui.label("Quitting stops it, and the file isn't made.");
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let keep = if importing { "Keep importing" } else { "Keep exporting" };
                    if ui.button(keep).clicked() {
                        answer = Some(false);
                    }
                    if ui.button("Quit anyway").clicked() {
                        answer = Some(true);
                    }
                });
            });
        match answer {
            Some(true) => {
                self.confirm_quit = false;
                self.quit(ctx);
            }
            Some(false) => self.confirm_quit = false,
            None => {}
        }
    }

    // ── Settings window ───────────────────────────────────────────────

    fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut page) = self.settings else {
            return;
        };
        self.settings_refresh(ctx, page);
        let raise = std::mem::take(&mut self.raise_settings);
        let mut open = true;
        let mut dropped = Vec::new();
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("lost-settings"),
            egui::ViewportBuilder::default()
                .with_title("LOST Settings")
                // Wide enough for every column of past imports.
                .with_inner_size(SETTINGS_SIZE)
                .with_min_inner_size([SETTINGS_SIZE[0], 420.0])
                .with_drag_and_drop(true),
            |ui, class| {
                if raise && class != egui::ViewportClass::EmbeddedWindow {
                    // Commands here go to this window, not the main one.
                    ui.ctx()
                        .send_viewport_cmd(ViewportCommand::Minimized(false));
                    ui.ctx().send_viewport_cmd(ViewportCommand::Focus);
                }
                if class == egui::ViewportClass::EmbeddedWindow {
                    // Already inside a window egui made for it.
                    self.settings_body(ui, &mut page);
                } else {
                    egui::CentralPanel::default().show(ui, |ui| self.settings_body(ui, &mut page));
                    if ui.input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                }
                dropped = dropped_paths(ui.ctx());
                drop_hint(ui.ctx());
            },
        );
        self.settings_done(page, open, dropped);
    }

    /// Kept up to date while Settings is open: imports from elsewhere, the
    /// database's size, who's trusted.
    fn settings_refresh(&mut self, ctx: &egui::Context, page: Page) {
        if self
            .settings_read
            .is_none_or(|at| at.elapsed() >= SETTINGS_REFRESH)
        {
            match page {
                Page::Data => self.load_history(),
                Page::Configuration => {
                    self.load_trusted();
                    self.settings_read = Some(Instant::now());
                }
            }
        }
        ctx.request_repaint_after(SETTINGS_REFRESH);
    }

    /// What's in the Settings window: its tabs and the page shown.
    fn settings_body(&mut self, ui: &mut egui::Ui, page: &mut Page) {
        // Tabs like the main window's: half the width each.
        ui.horizontal(|ui| {
            let gap = 6.0;
            ui.spacing_mut().item_spacing.x = gap;
            let width = ((ui.available_width() - gap) / 2.0).floor();
            for (p, title) in [(Page::Configuration, "Configuration"), (Page::Data, "Data")] {
                if tab_shape(ui, title, *page == p, width, false).0.clicked() {
                    *page = p;
                }
            }
        });
        ui.add_space(4.0);
        ui.separator();
        match page {
            Page::Configuration => self.config_page(ui),
            Page::Data => self.data_page(ui),
        }
        self.question_modal(ui.ctx());
    }

    /// After drawing Settings: the page now shown, whether it's still open,
    /// files dropped on it.
    fn settings_done(&mut self, page: Page, open: bool, dropped: Vec<PathBuf>) {
        if page == Page::Data && self.settings != Some(Page::Data) {
            self.load_history();
        }
        if page != Page::Configuration || !open {
            self.save_config(); // what was still being typed
        }
        if !open {
            self.config.loaded = false; // read again next time
            self.answer(Answer::No); // a question nobody can see
        }
        self.settings = open.then_some(page);
        if let Some(dialog) = self.want_dialog.take() {
            self.open_dialog(dialog);
        }
        if !dropped.is_empty() {
            self.start_import(dropped);
        }
    }

    /// Data: what's in the database, importing, past imports (the imports
    /// running now among them, each with a progress bar), exporting.
    /// Nothing here changes size while an import, export or removal runs.
    fn data_page(&mut self, ui: &mut egui::Ui) {
        let busy = self.working();
        if self.importing.load(Ordering::Relaxed) {
            // The progress bars follow the bytes read.
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        ui.add_space(8.0);
        ui.label(
            RichText::new({
                let n = self.lost.document_count();
                let size = self
                    .import
                    .size
                    .map(|b| format!(", taking up {}", crate::text::format_bytes(b)))
                    .unwrap_or_default();
                format!(
                    "There are currently {} page{} in the database{size}",
                    thousands(n),
                    if n == 1 { "" } else { "s" }
                )
            })
            .size(18.0),
        );
        ui.add_space(10.0);

        // Import, the window's width.
        ui.heading("Import");
        let height = self.measured.boxes[0];
        let (contents, outer) = centered_box(ui, height, height, |ui| self.import_box(ui, busy));
        if (contents - self.measured.boxes[0]).abs() > 0.5 {
            ui.ctx().request_repaint();
        }
        self.measured.boxes = [contents, outer];

        // Past imports, with Remove at the right, above the table.
        ui.add_space(10.0);
        let mut remove_click = false;
        ui.horizontal(|ui| {
            ui.heading("Past imports");
            if let Some(e) = &self.import.remove_error {
                ui.add_space(8.0);
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let button = egui::Button::new("Remove").min_size(egui::vec2(100.0, 0.0));
                if ui
                    .add_enabled(!self.import.selected.is_empty() && !busy, button)
                    .clicked()
                {
                    remove_click = true;
                }
            });
        });
        let mut remove_answer = None;
        if let Some((ids, pages, name)) = &self.import.confirm_remove {
            let modal = egui::Modal::new(egui::Id::new("confirm-remove")).show(ui.ctx(), |ui| {
                ui.set_width(400.0);
                let title = match ids.len() {
                    1 => format!("Remove “{name}”?"),
                    n => format!("Remove {n} imports?"),
                };
                ui.label(RichText::new(title).size(16.0).strong());
                ui.add_space(6.0);
                ui.label(format!(
                    "{} {} page{} will be removed from the database.",
                    if ids.len() == 1 { "Its" } else { "Their" },
                    thousands(*pages),
                    if *pages == 1 { "" } else { "s" }
                ));
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let size = egui::vec2(100.0, TAB_HEIGHT);
                    if ui.add(egui::Button::new("Remove").min_size(size)).clicked() {
                        remove_answer = Some(true);
                    }
                    if ui.add(egui::Button::new("Cancel").min_size(size)).clicked() {
                        remove_answer = Some(false);
                    }
                });
            });
            // Esc, or a click outside it.
            if remove_answer.is_none() && modal.should_close() {
                remove_answer = Some(false);
            }
        }

        // The table takes what the Export section below leaves.
        let table_height = (ui.available_height() - self.measured.export).max(80.0);
        egui::ScrollArea::vertical()
            .id_salt("imports")
            .max_height(table_height)
            .auto_shrink([false, true])
            .show(ui, |ui| self.imports_table(ui, busy));

        // Export: everything, or the imports ticked above.
        let top = ui.cursor().top();
        ui.add_space(10.0);
        ui.heading("Export");
        let export_click = self.export_box(ui, busy);
        self.measured.export = ui.cursor().top() - top + ui.spacing().item_spacing.y;

        if remove_click {
            let ids: Vec<u64> = self.import.selected.iter().copied().collect();
            // Pages whose current version came from them, updated ones included.
            let pages = ids
                .iter()
                .map(|&id| self.lost.import_pages(id).unwrap_or(0))
                .sum();
            let name = match (&self.import.history, ids.as_slice()) {
                (Some(Ok(rows)), [id]) => rows
                    .iter()
                    .find(|r| r.id == *id)
                    .map(|r| crate::text::files_summary(&r.files))
                    .unwrap_or_else(|| format!("import #{id}")),
                _ => String::new(),
            };
            self.import.confirm_remove = Some((ids, pages, name));
        }
        match remove_answer {
            Some(true) => {
                if let Some((ids, _, _)) = self.import.confirm_remove.take() {
                    self.remove_imports(ids);
                }
            }
            Some(false) => self.import.confirm_remove = None,
            None => {}
        }
        if export_click {
            self.choose_export_file();
        }
    }

    /// The table of past imports, as wide as the window: the imports of the
    /// run going on now first (newest at the top), then the app database's.
    fn imports_table(&mut self, ui: &mut egui::Ui, busy: bool) {
        let history: &[ImportRow] = match &self.import.history {
            Some(Ok(rows)) => rows,
            Some(Err(e)) if self.import.run.is_empty() => {
                ui.colored_label(ui.visuals().warn_fg_color, e);
                return;
            }
            _ => &[],
        };
        if history.is_empty() && self.import.run.is_empty() {
            if self.import.history.is_some() {
                ui.label(RichText::new("None yet.").weak());
            }
            return;
        }

        // Every cell is placed by these widths alone, never by what's in
        // it, so nothing moves from one frame to the next. File takes what
        // the others leave, less a little room at the right for the bars'
        // round ends.
        let total = (ui.available_width() - 8.0).floor();
        let mut widths = [
            ui.spacing().interact_size.y, // the checkbox
            0.0,                          // File: the rest
            110.0,                        // Type
            130.0,                        // Imported on
            340.0,                        // Page count
            STATUS_WIDTH,
        ];
        widths[1] =
            (total - widths.iter().sum::<f32>() - TABLE_SPACING * (widths.len() - 1) as f32)
                .max(120.0);
        let font = egui::TextStyle::Body.resolve(ui.style());
        let text_height = ui.fonts_mut(|f| f.row_height(&font));
        let row_height = text_height.max(ui.spacing().interact_size.y) + 6.0;
        let visuals = ui.visuals().clone();
        let (text, strong, weak, red) = (
            visuals.text_color(),
            visuals.strong_text_color(),
            visuals.weak_text_color(),
            visuals.error_fg_color,
        );
        let read = crate::datasources::bytes_read();
        let selected = &mut self.import.selected;
        let removing = &self.import.removing;
        let run = &self.import.run;
        let records = self.import.records;
        ui.spacing_mut().item_spacing.y = 0.0;
        let row = |ui: &mut egui::Ui, striped: bool| -> [egui::Rect; 6] {
            let width = widths.iter().sum::<f32>() + TABLE_SPACING * 5.0;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_height), Sense::hover());
            if striped {
                ui.painter().rect_filled(rect, 0.0, visuals.faint_bg_color);
            }
            let mut x = rect.left();
            widths.map(|w| {
                let cell =
                    egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, row_height));
                x += w + TABLE_SPACING;
                cell
            })
        };

        // The header: ticks every finished import, or none.
        let cells = row(ui, false);
        let tickable: Vec<u64> = history.iter().map(|r| r.id).collect();
        let ticked = tickable.iter().filter(|id| selected.contains(id)).count();
        let mut all = !tickable.is_empty() && ticked == tickable.len();
        let header = egui::Checkbox::without_text(&mut all)
            .indeterminate(ticked > 0 && ticked < tickable.len());
        let enabled = !busy && !tickable.is_empty();
        if ui
            .add_enabled_ui(enabled, |ui| ui.put(cells[0], header))
            .inner
            .changed()
        {
            if all {
                selected.extend(tickable.iter().copied());
            } else {
                selected.clear();
            }
        }
        for (i, h) in ["File", "Type", "Imported on", "Page count", "Status"]
            .into_iter()
            .enumerate()
        {
            paint_line(ui, cells[i + 1], h, strong);
        }

        // This run's imports: no ticking until they're in the app database.
        let mut striped = false;
        for (n, r) in run.iter().enumerate().rev() {
            striped = !striped;
            let cells = row(ui, striped);
            ui.add_enabled_ui(false, |ui| {
                ui.put(cells[0], egui::Checkbox::without_text(&mut false))
            });
            let started = r.row.as_ref().map(|row| local_datetime(row.started));
            paint_line(ui, cells[3], started.as_deref().unwrap_or(""), text);
            paint_line(
                ui,
                cells[2],
                &crate::text::source_label(r.planned.source),
                text,
            );
            paint_line(ui, cells[1], &file_cell(&r.planned.files), text);
            ui.interact(cells[1], ui.id().with(("run-file", n)), Sense::hover())
                .on_hover_text(r.planned.files.join("\n"));
            let count = match (r.state, &r.row) {
                (RunState::Done, Some(row)) => page_count(row),
                (RunState::Running, _) => thousands(records),
                _ => String::new(),
            };
            paint_line(ui, cells[4], &count, text);
            match r.state {
                RunState::Waiting => paint_bar(ui, cells[5], 0.0, PROGRESS_GREEN),
                RunState::Running => {
                    let done = read as f32 / r.planned.bytes.max(1) as f32;
                    paint_bar(ui, cells[5], done.min(0.99), PROGRESS_GREEN);
                }
                RunState::Done => paint_bar(ui, cells[5], 1.0, PROGRESS_GREEN),
                RunState::Failed => paint_line(ui, cells[5], "failed", red),
            }
        }

        for import in history.iter().rev() {
            striped = !striped;
            let cells = row(ui, striped);
            let mut on = selected.contains(&import.id);
            let tick = egui::Checkbox::without_text(&mut on);
            if ui
                .add_enabled_ui(!busy, |ui| ui.put(cells[0], tick))
                .inner
                .changed()
            {
                if on {
                    selected.insert(import.id);
                } else {
                    selected.remove(&import.id);
                }
            }
            paint_line(ui, cells[3], &local_datetime(import.started), text);
            paint_line(
                ui,
                cells[2],
                &crate::text::source_label(&import.source),
                text,
            );
            // Cut to fit: hover for all of them, and any notes.
            paint_line(ui, cells[1], &file_cell(&import.files), text);
            let hover = if import.notes.is_empty() {
                import.files.join("\n")
            } else {
                format!("{}\n\n{}", import.files.join("\n"), import.notes.join("\n"))
            };
            ui.interact(
                cells[1],
                ui.id().with(("import-file", import.id)),
                Sense::hover(),
            )
            .on_hover_text(hover);
            paint_line(ui, cells[4], &page_count(import), text);
            if removing.contains(&import.id) {
                paint_line(ui, cells[5], "removing…", weak);
            } else if import.status == "complete" {
                paint_bar(ui, cells[5], 1.0, PROGRESS_GREEN);
            } else if import.status == "failed" {
                paint_line(ui, cells[5], "failed", red);
            } else {
                paint_line(ui, cells[5], "interrupted", weak);
            }
        }
    }

    /// An import's progress, from its thread.
    fn import_progress(&mut self, progress: Progress) {
        match progress {
            Progress::Planned(planned) => {
                self.import.run = planned
                    .into_iter()
                    .map(|planned| RunRow {
                        planned,
                        row: None,
                        state: RunState::Waiting,
                    })
                    .collect();
            }
            Progress::Started(n, row) => {
                self.import.records = 0;
                self.log(
                    LogKind::Info,
                    format!("Import #{}: {}…", row.id, row.files.join(", ")),
                );
                if let Some(r) = self.import.run.get_mut(n) {
                    r.row = Some(row);
                    r.state = RunState::Running;
                }
            }
            Progress::Records(records) => self.import.records = records,
            Progress::Finished(n, result) => {
                let state = match result {
                    Ok(row) => {
                        self.log(
                            LogKind::Done,
                            format!(
                                "Import #{} ({}): {} — {} already there.",
                                row.id,
                                row.files.join(", "),
                                page_count(&row),
                                thousands(row.unchanged),
                            ),
                        );
                        for note in &row.notes {
                            self.log(LogKind::Note, note.clone());
                        }
                        if let Some(r) = self.import.run.get_mut(n) {
                            r.row = Some(row);
                        }
                        RunState::Done
                    }
                    Err(e) => {
                        self.log(LogKind::Error, sentence(&e));
                        RunState::Failed
                    }
                };
                if let Some(r) = self.import.run.get_mut(n) {
                    r.state = state;
                }
            }
        }
    }

    /// An import, export or removal is running.
    fn working(&self) -> bool {
        self.busy() || !self.import.removing.is_empty()
    }

    /// Writes a line to stderr: the details of imports, exports and
    /// removals, for anyone looking into a problem from a terminal (the
    /// window shows progress and results without them).
    fn log(&mut self, kind: LogKind, text: impl Into<String>) {
        let time = crate::output::local_time(crate::store::now());
        let text = text.into();
        match kind {
            LogKind::Info | LogKind::Done => eprintln!("[{time}] {text}"),
            LogKind::Error => eprintln!("[{time}] error: {text}"),
            LogKind::Note => eprintln!("           {text}"),
        }
    }

    /// The Import box: where to drop files, or buttons to pick them, and a
    /// line for what went wrong, if anything. The same size whatever runs.
    fn import_box(&mut self, ui: &mut egui::Ui, busy: bool) {
        ui.label(RichText::new("Drop dataset files or folders here").size(16.0));
        ui.label(
            RichText::new(".lost files, Hacker News CSV, Common Crawl WET/WAT (also .gz)")
                .small()
                .weak(),
        );
        ui.add_space(6.0);
        let width = self.measured.rows.entry("import").or_default();
        let free = !self.dialog_open && !busy;
        let mut want = None;
        centered_row(ui, width, |ui| {
            if ui
                .add_enabled(free, egui::Button::new("Choose files…"))
                .clicked()
            {
                want = Some(Dialog::ImportFiles);
            }
            if ui
                .add_enabled(free, egui::Button::new("Choose a folder…"))
                .clicked()
            {
                want = Some(Dialog::ImportFolder);
            }
        });
        if want.is_some() {
            self.want_dialog = want;
        }
        // Always a line here, so nothing moves.
        ui.add_space(2.0);
        match &self.import.message {
            Some(e) => ui.colored_label(ui.visuals().error_fg_color, e),
            None => ui.label(" "),
        };
    }

    /// The Export section, the window's width: signed or not, everything or
    /// the ticked imports. True when the button is clicked.
    fn export_box(&mut self, ui: &mut egui::Ui, busy: bool) -> bool {
        let selected = !self.import.selected.is_empty();
        let mut clicked = false;
        egui::Frame::group(ui.style())
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let right = ui.max_rect().right();
                // Left: what it does, then how the export goes.
                let left = ui
                    .vertical(|ui| {
                        ui.label(
                            RichText::new(if selected {
                                "Save the selected imports as a .lost file"
                            } else {
                                "Save current database as a .lost file"
                            })
                            .size(16.0),
                        );
                        // How the last export went, or how this one goes;
                        // otherwise what it's for. Always one line.
                        if self.export.running {
                            let done = if self.export.total == 0 {
                                0.0
                            } else {
                                self.export.done as f32 / self.export.total as f32
                            };
                            progress_bar(ui, EXPORT_BAR_WIDTH, done, PROGRESS_GREEN);
                        } else {
                            match &self.export.message {
                                Some(Ok(m)) => {
                                    ui.label(RichText::new(m).color(PROGRESS_GREEN));
                                }
                                Some(Err(e)) => {
                                    ui.colored_label(ui.visuals().error_fg_color, e);
                                }
                                None => {
                                    ui.label(
                                        RichText::new("To share it, or to keep a copy")
                                            .small()
                                            .weak(),
                                    );
                                }
                            }
                        }
                    })
                    .response
                    .rect;

                // Right: the button at the edge, the checkbox beside it, both
                // centred on the left side's two lines. Placed, not laid out,
                // so nothing shifts.
                let middle = left.center().y;
                let button_size = egui::vec2(140.0, TAB_HEIGHT);
                let button_rect = egui::Rect::from_min_size(
                    egui::pos2(right - button_size.x, middle - button_size.y / 2.0),
                    button_size,
                );
                let button = egui::Button::new(if selected {
                    "Export selected"
                } else {
                    "Export all"
                });
                let enabled = !self.lost.is_empty() && !self.dialog_open && !busy;
                clicked = ui
                    .add_enabled_ui(enabled, |ui| ui.put(button_rect, button))
                    .inner
                    .clicked();

                let label = "Sign it, so people can tell it's from you";
                let font = egui::TextStyle::Body.resolve(ui.style());
                let text = ui.fonts_mut(|f| {
                    f.layout_no_wrap(label.to_string(), font, Color32::WHITE)
                        .size()
                        .x
                });
                let (indent, check_height) = {
                    let spacing = ui.spacing();
                    (
                        spacing.icon_width + spacing.icon_spacing,
                        spacing.interact_size.y,
                    )
                };
                let check_size = egui::vec2(indent + text + 2.0, check_height);
                let check_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        button_rect.left() - 16.0 - check_size.x,
                        middle - check_size.y / 2.0,
                    ),
                    check_size,
                );
                ui.put(
                    check_rect,
                    egui::Checkbox::new(&mut self.export.sign, label),
                );
            });
        clicked
    }

    /// Asks where to save the export; it starts once that's picked.
    fn choose_export_file(&mut self) {
        let imports: Vec<u64> = self.import.selected.iter().copied().collect();
        let sign = self.export.sign;
        let name = format!(
            "lost-pages-{}{}",
            local_date(crate::store::now()),
            if sign { "-signed" } else { "" }
        );
        self.open_dialog(Dialog::Export {
            name,
            imports,
            sign,
        });
    }

    /// The question about a `.lost` file, in a small window of its own over
    /// the settings (so it isn't missed). Esc or a click outside: don't import.
    fn question_modal(&mut self, ctx: &egui::Context) {
        let Some((q, _)) = &self.import.question else {
            return;
        };
        let pages = format!(
            "{} page{}",
            thousands(q.pages),
            if q.pages == 1 { "" } else { "s" }
        );
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::new("import-question")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(
                RichText::new(format!("Import “{}”?", q.file))
                    .size(16.0)
                    .strong(),
            );
            ui.add_space(6.0);
            match &q.signer {
                Some(id) => {
                    ui.label(format!("It has {pages}, signed by {id}."));
                    ui.label("You haven't imported a file from them before.");
                }
                None => {
                    ui.label(format!("It has {pages}."));
                    ui.label("It isn't signed, so there's no telling who made it.");
                }
            }
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let size = egui::vec2(100.0, TAB_HEIGHT);
                if ui.add(egui::Button::new("Import").min_size(size)).clicked() {
                    answer = Some(Answer::Yes);
                }
                if q.signer.is_some()
                    && ui
                        .add(egui::Button::new("Import and trust them").min_size(size))
                        .clicked()
                {
                    answer = Some(Answer::YesAndTrust);
                }
                if ui
                    .add(egui::Button::new("Don't import").min_size(size))
                    .clicked()
                {
                    answer = Some(Answer::No);
                }
            });
        });
        if answer.is_none() && modal.should_close() {
            answer = Some(Answer::No);
        }
        if let Some(answer) = answer {
            self.answer(answer);
        }
    }

    /// Configuration: the settings in lost.toml, as a form.
    fn config_page(&mut self, ui: &mut egui::Ui) {
        let Some(_path) = self.options.config_path.clone() else {
            ui.label("There's no home folder to keep lost.toml in.");
            return;
        };
        // Where data goes when the field is left empty.
        let default_dir = crate::config::user_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        let mut untrust = None;
        let mut choose_dir = false;
        let dialog_open = self.dialog_open;
        let busy = self.importing.load(Ordering::Relaxed);
        let trusted = &self.trusted;
        let my_id = self.export.id.clone();
        let new_id_error = self.new_id_error.clone();
        let mut new_id_click = false;
        let form = &mut self.config;
        if !form.loaded {
            form.load(&self.options.config);
        }
        // Settings that only take effect on a restart say so, under them,
        // while what's saved differs from what LOST started with.
        let started = &self.started;
        let saved = form.saved_config.clone();
        let in_process_server = self.options.server.is_some();
        let restart_note = |ui: &mut egui::Ui, differs: bool| {
            if differs {
                ui.label(
                    RichText::new("Takes effect after LOST restarts.")
                        .small()
                        .color(ui.visuals().warn_fg_color),
                );
            }
        };
        if let Some(e) = &form.error {
            ui.colored_label(ui.visuals().error_fg_color, format!("Not saved: {e}"));
        }

        egui::ScrollArea::vertical().id_salt("configuration").show(ui, |ui| {
            let note = |ui: &mut egui::Ui, text: &str| {
                ui.label(RichText::new(text).small().weak());
            };
            ui.columns(2, |columns| {
                // Left: search, the web server, the desktop app.
                let ui = &mut columns[0];
                let c = &mut form.config;
                ui.heading("Search");
                egui::Grid::new("config-search").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label("Results per query");
                    ui.add(egui::DragValue::new(&mut c.search.default_limit).range(1..=1000));
                    ui.end_row();
                });
                note(ui, "How many results a search shows at first (the command line's default for -l).");
                ui.add_space(8.0);
                ui.checkbox(&mut c.search.cached_links, "Show “Cached” links");
                note(ui, "After each result's address, to read the page as it's stored here. In this app and the web UI.");
                // The web server in this process (-w) shows them as it started.
                restart_note(
                    ui,
                    in_process_server && saved.search.cached_links != started.search.cached_links,
                );
                ui.add_space(14.0);

                ui.heading("Web server");
                ui.horizontal(|ui| {
                    ui.label("Address");
                    ui.add(egui::TextEdit::singleline(&mut c.server.address).desired_width(130.0));
                    ui.add_space(8.0);
                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut c.server.port).range(1..=65535));
                });
                note(ui, "127.0.0.1: this computer only. 0.0.0.0: every device on the network.");
                restart_note(
                    ui,
                    saved.server.address != started.server.address
                        || saved.server.port != started.server.port,
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Most results per request");
                    ui.add(egui::DragValue::new(&mut c.server.max_results).range(1..=100_000));
                });
                restart_note(ui, saved.server.max_results != started.server.max_results);
                ui.add_space(14.0);

                ui.heading("Desktop app");
                ui.checkbox(&mut c.gui.tray, "Show an icon in the tray");
                note(ui, "With it, closing the window keeps LOST running there.");
                restart_note(ui, saved.gui.tray != started.gui.tray);

                // Right: storage, importing.
                let ui = &mut columns[1];
                ui.heading("Storage");
                ui.horizontal(|ui| {
                    ui.label("Data directory");
                    if ui.add_enabled(!dialog_open, egui::Button::new("Choose…")).clicked() {
                        choose_dir = true;
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut form.data_dir)
                        .hint_text(default_dir.as_str())
                        .desired_width(f32::INFINITY),
                );
                note(ui, "Where the search index and app database live. Leave it empty for the default (shown in grey).");
                restart_note(ui, saved.storage.data_dir != started.storage.data_dir);
                ui.add_space(14.0);

                let c = &mut form.config;
                ui.heading("Import");
                ui.horizontal(|ui| {
                    ui.label("Require signature");
                    egui::ComboBox::from_id_salt("trust")
                        .selected_text(match c.import.trust.as_str() {
                            "signed-only" => "Always",
                            "any" => "Never",
                            _ => "Ask every time",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut c.import.trust, "signed-only".to_string(), "Always");
                            ui.selectable_value(&mut c.import.trust, "any".to_string(), "Never");
                            ui.selectable_value(&mut c.import.trust, "ask".to_string(), "Ask every time");
                        });
                });
                note(ui, match c.import.trust.as_str() {
                    "signed-only" => "Unsigned .lost files aren't imported.",
                    "any" => ".lost files are imported without asking, signed or not.",
                    _ => "Asks before importing a .lost file that isn't signed, or is from someone new.",
                });
                ui.add_space(8.0);
                // Your ID: what others see on files you sign.
                ui.horizontal(|ui| {
                    match &my_id {
                        Some(id) => {
                            ui.label("Your ID");
                            ui.label(RichText::new(id).monospace());
                        }
                        None => {
                            ui.label("Your ID");
                            ui.label(RichText::new("none yet").weak());
                        }
                    }
                    let label = if my_id.is_some() { "New ID…" } else { "Make one…" };
                    if ui.add_enabled(!busy, egui::Button::new(label).small()).clicked() {
                        new_id_click = true;
                    }
                });
                match &new_id_error {
                    Some(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    None if my_id.is_some() => note(ui, "Files you sign show it, so people can tell they're from you."),
                    None => note(ui, "Made the first time you sign an export, or now."),
                }
                ui.add_space(8.0);
                ui.label("People you trust");
                match trusted {
                    Some(Ok(rows)) if rows.is_empty() => note(
                        ui,
                        "Nobody yet. When you import a file from someone new, you can choose to trust them.",
                    ),
                    Some(Ok(rows)) => {
                        egui::Grid::new("trusted").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
                            for row in rows {
                                ui.label(RichText::new(crate::lostfile::short_id_hex(&row.key)).monospace());
                                ui.label(
                                    RichText::new(if row.label.is_empty() {
                                        format!("since {}", local_date(row.added))
                                    } else {
                                        format!("{}, since {}", row.label, local_date(row.added))
                                    })
                                    .weak(),
                                );
                                if ui.add_enabled(!busy, egui::Button::new("Remove").small()).clicked() {
                                    untrust = Some(row.key.clone());
                                }
                                ui.end_row();
                            }
                        });
                        note(ui, "Their signed files import without a question.");
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().warn_fg_color, e);
                    }
                    None => {}
                }
            });
        });

        // Saved as soon as nothing is being typed or dragged: when a field
        // loses the keyboard, a number is let go, a box is ticked.
        let editing = ui.ctx().memory(|m| m.focused().is_some()) || ui.ctx().dragged_id().is_some();
        let save_now = !editing;
        if new_id_click {
            if my_id.is_some() {
                self.confirm_new_id = true;
            } else {
                self.make_new_id();
            }
        }
        if self.confirm_new_id {
            let mut answer = None;
            let modal = egui::Modal::new(egui::Id::new("confirm-new-id")).show(ui.ctx(), |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("Make a new ID?").size(16.0).strong());
                ui.add_space(6.0);
                if let Some(id) = &my_id {
                    ui.label(format!("Files you've signed so far keep showing {id}."));
                }
                ui.label("People who trust you will be asked about files signed with the new one.");
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let size = egui::vec2(120.0, TAB_HEIGHT);
                    if ui
                        .add(egui::Button::new("Make new ID").min_size(size))
                        .clicked()
                    {
                        answer = Some(true);
                    }
                    if ui.add(egui::Button::new("Cancel").min_size(size)).clicked() {
                        answer = Some(false);
                    }
                });
            });
            // Esc, or a click outside it.
            if answer.is_none() && modal.should_close() {
                answer = Some(false);
            }
            if let Some(yes) = answer {
                self.confirm_new_id = false;
                if yes {
                    self.make_new_id();
                }
            }
        }
        if choose_dir {
            self.want_dialog = Some(Dialog::DataDir);
        }
        if save_now {
            self.save_config();
        }
        if untrust.is_some() {
            self.confirm_untrust = untrust;
        }
        // Asking before untrusting: a small window of its own.
        if let Some(key) = &self.confirm_untrust {
            let id = crate::lostfile::short_id_hex(key);
            let mut answer = None;
            let modal = egui::Modal::new(egui::Id::new("confirm-untrust")).show(ui.ctx(), |ui| {
                ui.set_width(420.0);
                ui.label(
                    RichText::new(format!("Stop trusting {id}?"))
                        .size(16.0)
                        .strong(),
                );
                ui.add_space(6.0);
                ui.label("Their signed files will be asked about again before they're imported.");
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let size = egui::vec2(120.0, TAB_HEIGHT);
                    if ui
                        .add(egui::Button::new("Stop trusting").min_size(size))
                        .clicked()
                    {
                        answer = Some(true);
                    }
                    if ui.add(egui::Button::new("Cancel").min_size(size)).clicked() {
                        answer = Some(false);
                    }
                });
            });
            // Esc, or a click outside it.
            if answer.is_none() && modal.should_close() {
                answer = Some(false);
            }
            if let Some(yes) = answer
                && let Some(key) = self.confirm_untrust.take()
                && yes
            {
                if let Err(e) = self.lost.untrust(&key) {
                    self.trusted = Some(Err(e.to_string()));
                } else {
                    self.load_trusted();
                }
            }
        }
    }

    /// Saves Settings → Configuration to the user's lost.toml if anything
    /// changed, and applies at once what doesn't need a restart.
    fn save_config(&mut self) {
        let Some(path) = self.options.config_path.clone() else {
            return;
        };
        let form = &mut self.config;
        if !form.loaded {
            return;
        }
        let now = form.current().to_toml();
        if now == form.saved || now == form.tried {
            return;
        }
        form.tried = now.clone();
        match crate::config::save_user_config(&path, &now) {
            Ok(()) => {
                form.saved = now;
                form.saved_config = form.current();
                form.error = None;
                self.options.config.import = form.saved_config.import.clone();
                self.options.config.search = form.saved_config.search.clone();
            }
            Err(e) => form.error = Some(sentence(&e.to_string())),
        }
    }

    /// A new signing key in place of the old one (see `Lost::new_identity`).
    fn make_new_id(&mut self) {
        match self.lost.new_identity() {
            Ok((_, new)) => {
                self.export.id = Some(new);
                self.new_id_error = None;
                self.load_trusted();
            }
            Err(e) => self.new_id_error = Some(sentence(&e.to_string())),
        }
    }

    /// Removes imports, one after another, on a thread of its own (see
    /// `Lost::remove_import`).
    fn remove_imports(&mut self, ids: Vec<u64>) {
        self.import.removing = ids.clone();
        let (lost, tx, waker) = (self.lost.clone(), self.tx.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let results = ids
                .into_iter()
                .map(|id| {
                    lost.remove_import(id)
                        .map(|pages| (id, pages))
                        .map_err(|e| e.to_string())
                })
                .collect();
            let _ = tx.send(Event::Removed(results));
            waker.wake();
        });
    }

    // ── About ─────────────────────────────────────────────────────────

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about {
            return;
        }
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("lost-about"),
            egui::ViewportBuilder::default()
                .with_title("About LOST")
                .with_inner_size(ABOUT_SIZE)
                .with_resizable(false),
            |ui, class| {
                if class == egui::ViewportClass::EmbeddedWindow {
                    // Already inside a window egui made for it.
                    self.about_body(ui);
                } else {
                    egui::CentralPanel::default().show(ui, |ui| self.about_body(ui));
                    if ui.input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                }
            },
        );
        self.about = open;
    }

    /// What's in the About window.
    fn about_body(&mut self, ui: &mut egui::Ui) {
        let icon = self
            .icon
            .get_or_insert_with(|| {
                ui.ctx().load_texture(
                    "lost-icon",
                    egui::ColorImage::from_rgba_unmultiplied([ICON_SIZE as usize; 2], ICON_RGBA),
                    egui::TextureOptions::LINEAR,
                )
            })
            .clone();
        let server = self.options.server.as_ref().map(|s| {
            let error = s.error.lock().ok().and_then(|e| e.clone());
            (s.url.clone(), error)
        });
        ui.vertical_centered(|ui| {
            ui.add_space(12.0);
            ui.add(egui::Image::new(&icon).fit_to_exact_size(egui::vec2(72.0, 72.0)));
            ui.heading(format!("LOST {}", env!("CARGO_PKG_VERSION")));
            ui.label("Local Offline Search Tool");
            ui.add_space(10.0);
            if let Some((url, error)) = &server {
                ui.add_space(6.0);
                match error {
                    Some(e) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!("Web server stopped: {e}"),
                        );
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.label("Web UI:");
                            ui.hyperlink(url);
                        });
                    }
                }
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Zero-Clause BSD licence").small().weak());
        });
    }
}

/// The eframe app for one window; the state outlives it (see `run`).
struct Window<'a>(&'a mut State);

impl eframe::App for Window<'_> {
    /// Runs before every frame, and also while the window is hidden or
    /// minimized (when nothing is drawn): events from the tray, other
    /// processes and background threads are handled here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.0.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.update(ui);
    }

    /// Remember the window's geometry only, not what was in it.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    /// Where the file dialogs last were, and the window's size and place,
    /// for next time.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(geometry) = self.0.geometry {
            geometry.save(&self.0.options.data_dir);
        }
        for (key, dir) in [
            (IMPORT_DIR_KEY, &self.0.import_dir),
            (EXPORT_DIR_KEY, &self.0.export_dir),
        ] {
            if let Some(dir) = dir {
                storage.set_string(key, dir.display().to_string());
            }
        }
    }
}

/// What the main window stands in for while it's hidden (see
/// `State::begin_stand_in`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum StandIn {
    Quick,
    Settings,
    About,
}

/// The main window's size, place and maximized state, kept between runs in
/// `window.txt` in the data folder: "width height x y maximized".
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    size: egui::Vec2,
    position: Option<egui::Pos2>,
    maximized: bool,
}

impl Geometry {
    const FILE: &'static str = "window.txt";

    fn load(data_dir: &Path) -> Option<Geometry> {
        let text = std::fs::read_to_string(data_dir.join(Self::FILE)).ok()?;
        Geometry::parse(&text)
    }

    fn parse(text: &str) -> Option<Geometry> {
        let mut parts = text.split_whitespace();
        let mut number = || parts.next()?.parse::<f32>().ok();
        let (w, h) = (number()?, number()?);
        let position = match (number(), number()) {
            (Some(x), Some(y)) => Some(egui::pos2(x, y)),
            _ => None,
        };
        let maximized = text.split_whitespace().nth(4) == Some("maximized");
        (w >= 100.0 && h >= 100.0).then_some(Geometry {
            size: egui::vec2(w, h),
            position,
            maximized,
        })
    }

    fn save(&self, data_dir: &Path) {
        let (x, y) = match self.position {
            Some(p) => (p.x.to_string(), p.y.to_string()),
            None => ("-".to_string(), "-".to_string()),
        };
        let text = format!(
            "{} {} {x} {y} {}\n",
            self.size.x,
            self.size.y,
            if self.maximized {
                "maximized"
            } else {
                "normal"
            }
        );
        let _ = std::fs::write(data_dir.join(Self::FILE), text);
    }
}

/// Keys in `gui.ron` for where the file dialogs open.
const IMPORT_DIR_KEY: &str = "import_dir";
const EXPORT_DIR_KEY: &str = "export_dir";

/// A tab: a rounded rectangle (the same corners as buttons) with its title
/// and, unless `closable` is false, its × inside. Returns (clicked, close).
fn tab_button(
    ui: &mut egui::Ui,
    tab: &Tab,
    active: bool,
    width: f32,
    closable: bool,
) -> (bool, bool) {
    let title = tab.title();
    let (response, closed) = tab_shape(ui, &title, active, width, closable);
    let response = response.on_hover_text(title);
    (
        response.clicked() && !closed,
        closed || response.middle_clicked(),
    )
}

/// A tab's shape: a rounded rectangle with `title`, and a × inside if
/// `closable`. Returns the tab's response and whether × was clicked.
fn tab_shape(
    ui: &mut egui::Ui,
    title: &str,
    active: bool,
    width: f32,
    closable: bool,
) -> (egui::Response, bool) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, TAB_HEIGHT), Sense::click());
    let v = ui.visuals();
    let (fill, text) = if active {
        (v.selection.bg_fill, v.selection.stroke.color)
    } else if response.hovered() {
        (
            v.widgets.hovered.weak_bg_fill,
            v.widgets.hovered.fg_stroke.color,
        )
    } else {
        (
            v.widgets.inactive.weak_bg_fill,
            v.widgets.inactive.fg_stroke.color,
        )
    };
    ui.painter()
        .rect_filled(rect, v.widgets.inactive.corner_radius, fill);

    let padding = 10.0;
    let close_width = if closable { 18.0 } else { 0.0 };
    let mut job = LayoutJob::simple_singleline(title.to_string(), FontId::proportional(14.0), text);
    job.wrap = TextWrapping {
        max_width: (width - 2.0 * padding - close_width).max(10.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let at = egui::pos2(
        rect.left() + padding,
        rect.center().y - galley.size().y / 2.0,
    );
    // However narrow the tab gets, nothing is drawn outside it.
    let painter = ui.painter().with_clip_rect(rect);
    painter.galley(at, galley, text);

    let mut closed = false;
    if closable {
        let spot = egui::Rect::from_center_size(
            egui::pos2(
                rect.right() - padding / 2.0 - close_width / 2.0,
                rect.center().y,
            ),
            egui::vec2(close_width, close_width),
        );
        // A click area of its own, without taking space in the row (placing
        // a widget there nudged the next tab down), added after the tab so
        // it's on top and gets its clicks.
        let close = ui
            .interact(
                spot.intersect(rect),
                response.id.with("close"),
                Sense::click(),
            )
            .on_hover_text("Close tab (Ctrl+W)");
        let color = if close.hovered() {
            v.strong_text_color()
        } else {
            text
        };
        painter.text(
            spot.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::proportional(15.0),
            color,
        );
        closed = close.clicked();
    }
    (response, closed)
}

/// Suggestions as pills on one line: as many as fit, never wrapping.
/// Returns the one clicked, and whether the pointer is over any.
fn pills(
    ui: &mut egui::Ui,
    suggestions: &[String],
    selected: Option<usize>,
) -> (Option<String>, bool) {
    let mut chosen = None;
    let mut hovered = false;
    let mut room = ui.available_width();
    let padding = ui.spacing().button_padding.x * 2.0 + ui.spacing().item_spacing.x;
    for (i, s) in suggestions.iter().enumerate() {
        let width = ui.fonts_mut(|f| {
            f.layout_no_wrap(s.clone(), FontId::proportional(PILL_SIZE), Color32::WHITE)
                .size()
                .x
        }) + padding;
        if width > room {
            break;
        }
        room -= width;
        let pill =
            egui::Button::new(RichText::new(s).size(PILL_SIZE)).selected(selected == Some(i));
        let response = ui.add(pill);
        hovered |= response.hovered();
        if response.clicked() {
            chosen = Some(s.clone());
        }
    }
    (chosen, hovered)
}

/// How results are drawn: every row the same height, so only those in view
/// need drawing.
pub(crate) struct ResultStyle {
    pub(crate) link: Color32,
    text: Color32,
    strong: Color32,
    pub(crate) url: Color32,
    pub(crate) mark: Color32,
    /// "Cached", after the address: muted, as in the web UI.
    cached: Color32,
    /// Show "Cached" at all (`search.cached_links`).
    cached_link: bool,
}

impl ResultStyle {
    pub(crate) fn new(ui: &egui::Ui) -> ResultStyle {
        let v = ui.visuals();
        ResultStyle {
            link: v.hyperlink_color,
            text: v.text_color(),
            strong: v.strong_text_color(),
            url: if v.dark_mode {
                Color32::from_rgb(0x7f, 0xc8, 0xa0)
            } else {
                Color32::from_rgb(0x1e, 0x7a, 0x46)
            },
            mark: if v.dark_mode {
                Color32::from_rgba_unmultiplied(255, 210, 90, 40)
            } else {
                Color32::from_rgba_unmultiplied(255, 200, 0, 70)
            },
            cached: if v.dark_mode {
                Color32::from_gray(0x9a)
            } else {
                Color32::from_gray(0x66)
            },
            cached_link: false,
        }
    }

    /// Title line + snippet lines + URL line + the gap below.
    fn row_height(&self, ui: &egui::Ui) -> f32 {
        let line = |size: f32| ui.fonts_mut(|f| f.row_height(&FontId::proportional(size)));
        let spacing = ui.spacing().item_spacing.y;
        line(TITLE_SIZE)
            + SNIPPET_LINES as f32 * line(SNIPPET_SIZE)
            + line(URL_SIZE)
            + 2.0 * spacing
            + 14.0
    }

    /// Draws one result; returns what a click on it asks for.
    fn draw(&self, ui: &mut egui::Ui, hit: &Hit) -> Option<ResultAction> {
        let mut open = None;
        let width = ui.available_width();
        let fmt = |size: f32, color: Color32, background: Color32| TextFormat {
            font_id: FontId::proportional(size),
            color,
            background,
            ..Default::default()
        };

        let (title_text, title_matches): (&str, &[[usize; 2]]) = match &hit.title {
            Some(t) => (t, &hit.title_matches),
            None => (&hit.url, &[]),
        };
        let mut title = highlighted(
            title_text,
            title_matches,
            fmt(TITLE_SIZE, self.link, Color32::TRANSPARENT),
            fmt(TITLE_SIZE, self.link, self.mark),
        );
        title.wrap = TextWrapping {
            max_width: width,
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        };
        let title = ui.fonts_mut(|f| f.layout_job(title));
        let response = ui
            .add(
                egui::Label::new(title)
                    .selectable(false)
                    .show_tooltip_when_elided(false)
                    .sense(Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked() || response.middle_clicked() {
            open = Some(ResultAction::Open(hit.url.clone()));
        }
        response.context_menu(|ui| {
            if ui.button("Open link").clicked() {
                open = Some(ResultAction::Open(hit.url.clone()));
                ui.close();
            }
            if ui.button("Show cached text").clicked() {
                open = Some(ResultAction::Cached(hit.url.clone()));
                ui.close();
            }
            if ui.button("Copy link").clicked() {
                ui.ctx().copy_text(hit.url.clone());
                ui.close();
            }
        });

        let mut snippet = highlighted(
            &hit.snippet,
            &hit.snippet_matches,
            fmt(SNIPPET_SIZE, self.text, Color32::TRANSPARENT),
            fmt(SNIPPET_SIZE, self.strong, self.mark),
        );
        snippet.wrap = TextWrapping {
            max_width: width,
            max_rows: SNIPPET_LINES,
            break_anywhere: false,
            overflow_character: Some('…'),
        };
        let snippet = ui.fonts_mut(|f| f.layout_job(snippet));
        let lines = ui.fonts_mut(|f| f.row_height(&FontId::proportional(SNIPPET_SIZE)))
            * SNIPPET_LINES as f32;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, lines), Sense::hover());
        ui.painter().galley(rect.min, snippet, self.text);

        if !self.cached_link {
            ui.add(
                egui::Label::new(RichText::new(&hit.url).color(self.url).size(URL_SIZE))
                    .truncate()
                    .show_tooltip_when_elided(false),
            );
            return open;
        }
        // The address, cut to fit, and "Cached" right after it, as on the web.
        ui.horizontal(|ui| {
            let cached = RichText::new("Cached").color(self.cached).size(URL_SIZE);
            let font = FontId::proportional(URL_SIZE);
            let cached_width = ui.fonts_mut(|f| {
                f.layout_no_wrap("Cached".to_string(), font, self.cached)
                    .size()
                    .x
            });
            let gap = 10.0;
            ui.spacing_mut().item_spacing.x = gap;
            let room = (width - cached_width - gap).max(40.0);
            ui.allocate_ui(egui::vec2(room, ui.available_height()), |ui| {
                ui.add(
                    egui::Label::new(RichText::new(&hit.url).color(self.url).size(URL_SIZE))
                        .truncate()
                        .show_tooltip_when_elided(false),
                );
            });
            let link = ui
                .add(
                    egui::Label::new(cached)
                        .selectable(false)
                        .sense(Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if link.clicked() || link.middle_clicked() {
                open = Some(ResultAction::Cached(hit.url.clone()));
            }
        });
        open
    }
}

/// A framed box at least `height` tall, its contents centred across and,
/// using their height from the last frame (`last`), down. Returns the
/// contents' height now, and the box's outer height.
fn centered_box(
    ui: &mut egui::Ui,
    height: f32,
    last: f32,
    add: impl FnOnce(&mut egui::Ui),
) -> (f32, f32) {
    let mut contents = 0.0;
    let outer = egui::Frame::group(ui.style())
        .inner_margin(16.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(height);
            let above = ((height - last) / 2.0).floor();
            if above > 0.0 {
                ui.add_space(above);
            }
            contents = ui.vertical_centered(add).response.rect.height();
        })
        .response
        .rect
        .height();
    (contents, outer)
}

/// A row of widgets centred across `ui`, using the row's width from the
/// last frame (`width`, updated).
fn centered_row(ui: &mut egui::Ui, width: &mut f32, add: impl FnOnce(&mut egui::Ui)) {
    let left = ((ui.available_width() - *width) / 2.0).max(0.0).floor();
    ui.horizontal(|ui| {
        ui.add_space(left);
        let now = ui.horizontal(add).response.rect.width();
        if (now - *width).abs() > 0.5 {
            *width = now;
            ui.ctx().request_repaint();
        }
    });
}

/// `15.5%`: one decimal, never 100% before the end.
fn percent(done: u64, total: u64) -> String {
    if total == 0 {
        return "0.0%".to_string();
    }
    let tenths = (done.min(total) as u128 * 1000 / total as u128) as u64;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// Runs a future to completion on this thread (for rfd's dialogs, which
/// need no runtime).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(output) => return output,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

/// Text on one line in a cell exactly `width` wide and one line tall, cut
/// with "…" if it doesn't fit: drawn, not laid out, so it sits on the
/// same line as the cells beside it.
/// "21,528 (6,247 new, replaced or updated 15,036)": an import's pages.
fn page_count(row: &ImportRow) -> String {
    if row.records == 0 {
        return "0".to_string();
    }
    let new = match row.new {
        0 => "no new".to_string(),
        n if n == row.records => "all new".to_string(),
        n => format!("{} new", thousands(n)),
    };
    match row.updated {
        0 => format!("{} ({new})", thousands(row.records)),
        n => format!(
            "{} ({new}, replaced or updated {})",
            thousands(row.records),
            thousands(n)
        ),
    }
}

/// The File column: a page icon (a hint that hovering tells more), then
/// the file names.
fn file_cell(files: &[String]) -> String {
    format!("\u{1F4C4} {}", files.join(", "))
}

/// A thin bar `width` wide, filled to `done` (0 to 1) in `color`, centred
/// in a row of text.
fn progress_bar(ui: &mut egui::Ui, width: f32, done: f32, color: Color32) {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let height = ui.fonts_mut(|f| f.row_height(&font));
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    paint_bar(ui, rect, done, color);
}

/// A thin bar across `rect`, filled to `done` (0 to 1) in `color`.
fn paint_bar(ui: &egui::Ui, rect: egui::Rect, done: f32, color: Color32) {
    let bar = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 4.0));
    let painter = ui.painter();
    painter.rect_filled(bar, 2.0, ui.visuals().widgets.inactive.weak_bg_fill);
    let mut filled = bar;
    filled.set_width(rect.width() * done.clamp(0.0, 1.0));
    if filled.width() > 0.0 {
        painter.rect_filled(filled, 2.0, color);
    }
}

/// One line of text in `rect`, vertically centred, cut with "…" at its
/// right edge if it's too long.
fn paint_line(ui: &mut egui::Ui, rect: egui::Rect, text: &str, color: Color32) {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = TextWrapping {
        max_width: rect.width(),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let top = rect.center().y - galley.size().y / 2.0;
    ui.painter()
        .with_clip_rect(rect)
        .galley(egui::pos2(rect.left(), top), galley, color);
}

/// A message as a sentence: "can't open x" → "Can't open x."
fn sentence(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let mut out: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?', ')', '"']) {
        out.push('.');
    }
    out
}

fn dropped_paths(ctx: &egui::Context) -> Vec<PathBuf> {
    ctx.input(|i| {
        i.raw
            .dropped_files
            .iter()
            .map(|f| f.path().to_path_buf())
            .collect()
    })
}

/// Shows a hint over a window while files are dragged over it.
fn drop_hint(ctx: &egui::Context) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("drop"),
    ));
    let rect = ctx.content_rect();
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "Drop to import into LOST",
        FontId::proportional(24.0),
        Color32::WHITE,
    );
}

// ── Highlighting ────────────────────────────────────────────────────────

/// `text` split at the `[start, end)` character ranges: (piece, is a match).
fn segments<'a>(text: &'a str, ranges: &[[usize; 2]]) -> Vec<(&'a str, bool)> {
    let bounds: Vec<usize> = text
        .char_indices()
        .map(|(b, _)| b)
        .chain([text.len()])
        .collect();
    let chars = bounds.len() - 1;
    let mut out = Vec::new();
    let mut at = 0;
    for &[start, end] in ranges {
        if start < at || end <= start || start >= chars {
            continue;
        }
        let end = end.min(chars);
        if start > at {
            out.push((&text[bounds[at]..bounds[start]], false));
        }
        out.push((&text[bounds[start]..bounds[end]], true));
        at = end;
    }
    if at < chars {
        out.push((&text[bounds[at]..], false));
    }
    out
}

pub(crate) fn highlighted(
    text: &str,
    ranges: &[[usize; 2]],
    normal: TextFormat,
    matched: TextFormat,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    for (piece, is_match) in segments(text, ranges) {
        job.append(
            piece,
            0.0,
            if is_match {
                matched.clone()
            } else {
                normal.clone()
            },
        );
    }
    job
}

// ── Tray ────────────────────────────────────────────────────────────────

mod tray {
    use std::sync::mpsc::Sender;

    use super::{Event, Waker};

    /// Holds the tray icon for as long as the app runs.
    pub struct Tray {
        #[cfg(all(unix, not(target_os = "macos")))]
        handle: ksni::blocking::Handle<LostTray>,
        #[cfg(any(windows, target_os = "macos"))]
        _icon: tray_icon::TrayIcon,
        /// The "Hide"/"Show" item.
        #[cfg(any(windows, target_os = "macos"))]
        toggle: tray_icon::menu::MenuItem,
    }

    /// The menu item that hides or shows the window says which it will do.
    fn toggle_label(shown: bool) -> &'static str {
        if shown { "Hide" } else { "Show" }
    }

    fn send(tx: &Sender<Event>, waker: &Waker, event: Event) {
        let _ = tx.send(event);
        waker.wake();
    }

    // Linux and BSD: StatusNotifierItem over D-Bus (KDE, XFCE, Cinnamon,
    // GNOME with the AppIndicator extension, …), no GTK needed.
    #[cfg(all(unix, not(target_os = "macos")))]
    pub struct LostTray {
        tx: Sender<Event>,
        waker: Waker,
        /// The window is shown.
        shown: bool,
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    impl ksni::Tray for LostTray {
        fn id(&self) -> String {
            "lost".into()
        }

        fn title(&self) -> String {
            "LOST".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            // ARGB, as the spec wants, from our RGBA.
            let data = super::ICON_RGBA
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[r, g, b, a]| [a, r, g, b])
                .collect();
            vec![ksni::Icon {
                width: super::ICON_SIZE as i32,
                height: super::ICON_SIZE as i32,
                data,
            }]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            send(&self.tx, &self.waker, Event::ToggleWindow);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            let item = |label: &str, event: fn() -> Event| -> ksni::MenuItem<Self> {
                StandardItem {
                    label: label.into(),
                    activate: Box::new(move |t: &mut Self| send(&t.tx, &t.waker, event())),
                    ..Default::default()
                }
                .into()
            };
            vec![
                item(toggle_label(self.shown), || Event::ToggleWindow),
                item("Quick search", || Event::QuickSearch(None)),
                item("New search", || Event::NewSearch),
                item("Settings", || Event::Settings),
                item("About", || Event::About),
                ksni::MenuItem::Separator,
                item("Quit", || Event::Quit),
            ]
        }
    }

    impl Tray {
        /// Updates the menu for the window being shown or hidden.
        pub fn set_shown(&self, shown: bool) {
            #[cfg(all(unix, not(target_os = "macos")))]
            self.handle.update(|t| t.shown = shown);
            #[cfg(any(windows, target_os = "macos"))]
            self.toggle.set_text(toggle_label(shown));
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        pub fn create(tx: Sender<Event>, waker: Waker) -> Option<Tray> {
            use ksni::blocking::TrayMethods;
            match (LostTray {
                tx,
                waker,
                shown: true,
            })
            .spawn()
            {
                Ok(handle) => Some(Tray { handle }),
                Err(e) => {
                    eprintln!("lost: no tray icon ({e}); closing the window will quit");
                    None
                }
            }
        }

        // Windows and macOS: the tray-icon crate, on the UI thread.
        #[cfg(any(windows, target_os = "macos"))]
        pub fn create(tx: Sender<Event>, waker: Waker) -> Option<Tray> {
            use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
            use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let menu = Menu::new();
            let toggle = MenuItem::new(toggle_label(true), true, None);
            let quick = MenuItem::new("Quick search", true, None);
            let new_search = MenuItem::new("New search", true, None);
            let settings = MenuItem::new("Settings", true, None);
            let about = MenuItem::new("About", true, None);
            let quit = MenuItem::new("Quit", true, None);
            menu.append_items(&[
                &toggle,
                &quick,
                &new_search,
                &settings,
                &about,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .ok()?;
            let icon = tray_icon::Icon::from_rgba(
                super::ICON_RGBA.to_vec(),
                super::ICON_SIZE,
                super::ICON_SIZE,
            )
            .ok()?;
            let built = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("LOST")
                .with_icon(icon)
                // Left click: the quick search popup; right click: the menu.
                .with_menu_on_left_click(false)
                .build();
            let icon = match built {
                Ok(icon) => icon,
                Err(e) => {
                    eprintln!("lost: no tray icon ({e}); closing the window will quit");
                    return None;
                }
            };

            let ids = [
                (
                    toggle.id().clone(),
                    (|| Event::ToggleWindow) as fn() -> Event,
                ),
                (
                    quick.id().clone(),
                    (|| Event::QuickSearch(None)) as fn() -> Event,
                ),
                (
                    new_search.id().clone(),
                    (|| Event::NewSearch) as fn() -> Event,
                ),
                (settings.id().clone(), (|| Event::Settings) as fn() -> Event),
                (about.id().clone(), (|| Event::About) as fn() -> Event),
                (quit.id().clone(), (|| Event::Quit) as fn() -> Event),
            ];
            {
                let (tx, waker) = (tx.clone(), waker.clone());
                MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                    if let Some((_, event)) = ids.iter().find(|(id, _)| *id == e.id) {
                        send(&tx, &waker, event());
                    }
                }));
            }
            // A left click opens the quick search popup at the icon.
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    rect,
                    ..
                } = e
                {
                    let icon = [
                        rect.position.x as f32,
                        rect.position.y as f32,
                        rect.size.width as f32,
                        rect.size.height as f32,
                    ];
                    send(&tx, &waker, Event::QuickSearch(Some(icon)));
                }
            }));
            Some(Tray {
                _icon: icon,
                toggle,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Geometry, ImportRow, page_count, percent, segments, sentence};

    #[test]
    fn window_geometry() {
        let g = Geometry::parse("920 720 15 40 normal").unwrap();
        assert_eq!((g.size.x, g.size.y, g.maximized), (920.0, 720.0, false));
        assert_eq!(g.position.map(|p| (p.x, p.y)), Some((15.0, 40.0)));
        let g = Geometry::parse("1200 800 - - maximized").unwrap();
        assert!(g.position.is_none() && g.maximized);
        assert!(Geometry::parse("").is_none());
        assert!(Geometry::parse("10 10 0 0 normal").is_none());
    }

    #[test]
    fn page_counts() {
        let row = |records, new, updated| ImportRow {
            records,
            new,
            updated,
            ..Default::default()
        };
        assert_eq!(page_count(&row(517, 517, 0)), "517 (all new)");
        assert_eq!(page_count(&row(517, 0, 0)), "517 (no new)");
        assert_eq!(
            page_count(&row(517, 0, 12)),
            "517 (no new, replaced or updated 12)"
        );
        assert_eq!(
            page_count(&row(21_528, 6_247, 15_036)),
            "21,528 (6,247 new, replaced or updated 15,036)"
        );
        assert_eq!(page_count(&row(0, 0, 0)), "0");
    }

    #[test]
    fn export_file_names() {
        assert_eq!(
            crate::text::files_summary(&["a".into(), "b".into(), "c".into()]),
            "a and 2 more"
        );
        assert_eq!(sentence("can't open x"), "Can't open x.");
        assert_eq!(sentence("Done."), "Done.");
        assert_eq!(percent(15_466, 22_763), "67.9%");
        assert_eq!(percent(22_762, 22_763), "99.9%");
        assert_eq!(percent(5, 5), "100.0%");
        assert_eq!(percent(0, 0), "0.0%");
        assert_eq!(crate::text::source_label("hacker_news"), "Hacker News");
        assert_eq!(crate::text::source_label("lost"), "LOST");
        assert_eq!(
            crate::text::source_label("gemini_capsules"),
            "Gemini Capsules"
        );
    }

    #[test]
    fn highlight_segments() {
        assert_eq!(
            segments("a catfish b", &[[2, 9]]),
            vec![("a ", false), ("catfish", true), (" b", false)]
        );
        // Character offsets, not bytes.
        assert_eq!(
            segments("Ünï cat", &[[4, 7]]),
            vec![("Ünï ", false), ("cat", true)]
        );
        // Bad or overlapping ranges are skipped; ends are clamped.
        assert_eq!(
            segments("abc", &[[1, 9], [0, 1]]),
            vec![("a", false), ("bc", true)]
        );
        assert_eq!(segments("", &[[0, 1]]), Vec::<(&str, bool)>::new());
    }
}
