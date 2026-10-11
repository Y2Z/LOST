//! The quick search popup (ARCHITECTURE.md §11): a small borderless window,
//! always on top, with a search box and the first few results. Enter (or a
//! click) opens the selected result in the browser, Ctrl+Enter shows the
//! search in a new tab of the main window, Esc or clicking elsewhere closes
//! the popup.
//!
//! A window of the app's own (an immediate viewport), drawn while the main
//! window draws. (Run as a process of its own, its first X11 requests never
//! got an answer under XWayland, freezing it.) While the main window is
//! hidden, a window of its own can't be made (eframe never draws it), so
//! the main window itself becomes the popup for as long as it's open.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, FontId, Key, Layout, Modifiers, RichText, Sense, TextFormat,
    ViewportCommand, text::TextWrapping,
};

use crate::Lost;
use crate::engine::{Hit, SearchResults};
use crate::gui::{ResultStyle, URL_SIZE, highlighted};
use crate::output::thousands;
use crate::query::Limit;

/// The popup's size, in points.
pub const SIZE: [f32; 2] = [560.0, 430.0];
/// Results shown.
const RESULTS: usize = 8;
/// Wait after the last keystroke before searching.
const SEARCH_DELAY: Duration = Duration::from_millis(200);

struct Searched {
    generation: u64,
    result: std::result::Result<SearchResults, String>,
}

/// What the popup asks of the app when it's done.
pub enum Outcome {
    /// Closed (Esc, clicked elsewhere).
    Close,
    /// Open this page in the browser.
    Open(String),
    /// Show this search in a new tab of the main window.
    InLost(String),
}

pub struct Popup {
    lost: Arc<Lost>,
    tx: Sender<Searched>,
    rx: Receiver<Searched>,
    /// Where it opens (top-left, in points), if anywhere in particular.
    position: Option<egui::Pos2>,
    query: String,
    searched: String,
    edited: Option<Instant>,
    generation: u64,
    loading: bool,
    results: Vec<Hit>,
    total: usize,
    selected: usize,
    /// It has had focus; losing it closes the popup.
    had_focus: bool,
    /// Take the focus (the first frame, and when asked for again).
    focus: bool,
}

impl Popup {
    pub fn new(lost: Arc<Lost>, position: Option<egui::Pos2>) -> Popup {
        let (tx, rx) = mpsc::channel();
        Popup {
            lost,
            tx,
            rx,
            position,
            query: String::new(),
            searched: String::new(),
            edited: None,
            generation: 0,
            loading: false,
            results: Vec::new(),
            total: 0,
            selected: 0,
            had_focus: false,
            focus: true,
        }
    }

    /// Asked for again while open: back to the front, with the focus.
    pub fn raise(&mut self) {
        self.focus = true;
        self.had_focus = false;
    }

    /// Draws the popup in a window of its own (call every frame while it's
    /// open). `Some` when it's done.
    pub fn show(&mut self, ctx: &egui::Context) -> Option<Outcome> {
        self.poll(ctx);
        let mut builder = egui::ViewportBuilder::default()
            .with_title("LOST quick search")
            .with_inner_size(SIZE)
            .with_decorations(false)
            .with_always_on_top()
            .with_resizable(false)
            .with_taskbar(false);
        if let Some(position) = self.position {
            builder = builder.with_position(position);
        }
        let mut outcome = None;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("lost-quick"),
            builder,
            |ui, class| {
                if class == egui::ViewportClass::EmbeddedWindow {
                    // Already inside a window egui made for it.
                    outcome = self.body(ui);
                    return;
                }
                outcome = self.window(ui);
            },
        );
        outcome
    }

    /// Draws the popup filling `ui`'s window: its own (see [`Popup::show`]),
    /// or the main window turned into it while that one is hidden.
    pub fn window(&mut self, ui: &mut egui::Ui) -> Option<Outcome> {
        self.poll(ui.ctx());
        if self.focus {
            ui.ctx().send_viewport_cmd(ViewportCommand::Focus);
        }
        let frame = egui::Frame::window(ui.style()).inner_margin(12.0);
        let mut outcome = egui::CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| self.body(ui))
            .inner;
        // Clicking elsewhere closes it, like a menu.
        match ui.input(|i| i.viewport().focused) {
            Some(true) => self.had_focus = true,
            Some(false) if self.had_focus && outcome.is_none() => outcome = Some(Outcome::Close),
            _ => {}
        }
        if ui.input(|i| i.viewport().close_requested()) {
            outcome = Some(Outcome::Close);
        }
        outcome
    }

    /// Takes in finished searches, and starts one after a pause in typing.
    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(Searched { generation, result }) = self.rx.try_recv() {
            if generation == self.generation {
                self.loading = false;
                if let Ok(r) = result {
                    self.total = r.total;
                    self.results = r.results;
                }
            }
        }
        if let Some(at) = self.edited {
            let now = Instant::now();
            if now >= at + SEARCH_DELAY {
                self.search(ctx);
            } else {
                ctx.request_repaint_after(at + SEARCH_DELAY - now);
            }
        }
    }
}

impl Popup {
    fn search(&mut self, ctx: &egui::Context) {
        self.searched = self.query.trim().to_string();
        self.generation += 1;
        self.edited = None;
        self.selected = 0;
        if self.searched.is_empty() {
            self.results.clear();
            self.total = 0;
            self.loading = false;
            return;
        }
        self.loading = true;
        let (lost, tx, ctx) = (self.lost.clone(), self.tx.clone(), ctx.clone());
        let (generation, query) = (self.generation, self.searched.clone());
        std::thread::spawn(move || {
            let result = lost
                .search(&query, Limit::new(0, RESULTS))
                .map_err(|e| e.to_string());
            let _ = tx.send(Searched { generation, result });
            ctx.request_repaint();
        });
    }

    fn body(&mut self, ui: &mut egui::Ui) -> Option<Outcome> {
        let mut action = None;
        let (in_lost, enter, down, up, esc) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        let focus = std::mem::take(&mut self.focus);
        let edit = egui::TextEdit::singleline(&mut self.query)
            .hint_text("Quick search")
            .font(FontId::proportional(18.0))
            .margin(egui::vec2(10.0, 8.0))
            .desired_width(f32::INFINITY);
        let response = ui.add(edit);
        // Typing goes here — once the window has the focus: asking for
        // text input (the input method) before that, on a window that isn't
        // on screen yet, can hang under XWayland.
        let focused = ui.input(|i| i.viewport().focused) != Some(false);
        if focused && (focus || !response.has_focus()) {
            response.request_focus();
        } else if focus {
            self.focus = true; // again next frame
        }
        if response.changed() {
            self.edited = Some(Instant::now());
        }

        let n = self.results.len();
        if n > 0 && down {
            self.selected = (self.selected + 1) % n;
        }
        if n > 0 && up {
            self.selected = (self.selected + n - 1) % n;
        }
        if esc {
            action = Some(Outcome::Close);
        }
        if in_lost && !self.query.trim().is_empty() {
            action = Some(Outcome::InLost(self.query.trim().to_string()));
        } else if enter && let Some(hit) = self.results.get(self.selected) {
            action = Some(Outcome::Open(hit.url.clone()));
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let weak = ui.visuals().weak_text_color();
            if !self.searched.is_empty() && !self.loading {
                let text = match self.total {
                    0 => format!("No results for “{}”", self.searched),
                    1 => "1 result".to_string(),
                    n => format!("{} results", thousands(n as u64)),
                };
                ui.label(RichText::new(text).small().color(weak));
            }
            if self.loading {
                ui.spinner();
            }
        });

        let style = ResultStyle::new(ui);
        let v = ui.visuals().clone();
        for (i, hit) in self.results.iter().enumerate() {
            let fill = if i == self.selected {
                v.widgets.hovered.weak_bg_fill
            } else {
                Color32::TRANSPARENT
            };
            let row = egui::Frame::default()
                .fill(fill)
                .corner_radius(4.0)
                .inner_margin(egui::vec2(8.0, 4.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let (text, matches): (&str, &[[usize; 2]]) = match &hit.title {
                        Some(t) => (t, &hit.title_matches),
                        None => (&hit.url, &[]),
                    };
                    let fmt = |background| TextFormat {
                        font_id: FontId::proportional(15.0),
                        color: style.link,
                        background,
                        ..Default::default()
                    };
                    let mut job =
                        highlighted(text, matches, fmt(Color32::TRANSPARENT), fmt(style.mark));
                    job.wrap = TextWrapping {
                        max_width: ui.available_width(),
                        max_rows: 1,
                        break_anywhere: true,
                        overflow_character: Some('…'),
                    };
                    let title = ui.fonts_mut(|f| f.layout_job(job));
                    ui.add(
                        egui::Label::new(title)
                            .selectable(false)
                            .show_tooltip_when_elided(false),
                    );
                    ui.add(
                        egui::Label::new(RichText::new(&hit.url).color(style.url).size(URL_SIZE))
                            .selectable(false)
                            .truncate()
                            .show_tooltip_when_elided(false),
                    );
                });
            // The labels don't take clicks, so the row does.
            let row = row.response.interact(Sense::click());
            if row.hovered() {
                self.selected = i;
            }
            if row.clicked() {
                action = Some(Outcome::Open(hit.url.clone()));
            }
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.label(
                RichText::new("Enter opens · Ctrl+Enter shows all in LOST · Esc closes")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        });
        action
    }
}
