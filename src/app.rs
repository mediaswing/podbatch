//! The window.
//!
//! Two tabs. **Downloads** is the one the app opens on and the one that does the
//! work: the subscription list goes in at the top, the podcasts it contains are
//! listed on the left to be picked from, what the run is doing is written on the
//! right, and the progress bar spans the width underneath both. **Settings**
//! holds the things that are set once and then left alone.
//!
//! The OPML is read as soon as it is chosen rather than when the run starts,
//! because the list of podcasts is what the left pane is for — you cannot choose
//! from a list that only exists once you have committed to downloading all of it.
//!
//! Everything the engine reports arrives on a channel and is drained once per
//! frame in [`PodBatchApp::drain`]; nothing in here ever touches the network, so
//! the window keeps painting no matter how slow a feed is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{AtomExt as _, RichText, Ui};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::engine::{self, Cancel, EpisodeStatus, FeedStatus, Settings, Update};
use crate::opml;
use crate::sound::{self, Cue};
use crate::theme::{self, CONTROL_HEIGHT, PROGRESS_HEIGHT};
use crate::util::human_bytes;

/// How many output lines to keep. Long enough to cover a whole run, short enough
/// that a pathological feed can't grow it without bound.
const OUTPUT_LIMIT: usize = 2000;

/// The marks down the left of the output.
///
/// Every one of these is checked against the real font stack by
/// `tests::every_glyph_the_ui_writes_has_a_glyph_to_draw_it_with`. egui falls
/// back through Ubuntu, NotoEmoji and an icon font, and a character none of them
/// carries is drawn as an empty box — so "it looks like a tick" is not a good
/// enough reason to use one.
const MARK_DONE: &str = "✔";
const MARK_SKIPPED: &str = "=";
const MARK_FAILED: &str = "✖";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Downloads,
    Settings,
}

/// Something the user can ask for without reaching for the mouse.
///
/// Every action here is also a control on screen; the shortcuts are a second
/// way to the same place, not a hidden set of features. Kept as data so that
/// the keys, what they do, and the list of them written out in Settings all
/// come from one table and cannot drift apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    ChooseOpml,
    Start,
    Stop,
    Show(Tab),
    SelectAll,
    SelectNone,
}

/// The shortcut table: what it does, the keys, and how it is described.
///
/// `Modifiers::COMMAND` is Cmd on a Mac and Ctrl everywhere else, so one table
/// is right on both platforms and `Context::format_shortcut` writes it out the
/// way that platform's users expect to read it.
fn shortcuts() -> Vec<(Action, egui::KeyboardShortcut, &'static str)> {
    use egui::{Key, KeyboardShortcut, Modifiers};

    let command = Modifiers::COMMAND;
    let command_shift = Modifiers::COMMAND.plus(Modifiers::SHIFT);

    vec![
        (
            Action::ChooseOpml,
            KeyboardShortcut::new(command, Key::O),
            "Choose an OPML subscription list",
        ),
        (
            Action::Start,
            KeyboardShortcut::new(command, Key::Enter),
            "Start downloading",
        ),
        (
            Action::Stop,
            KeyboardShortcut::new(Modifiers::NONE, Key::Escape),
            "Stop the run in progress",
        ),
        (
            Action::Show(Tab::Downloads),
            KeyboardShortcut::new(command, Key::Num1),
            "Go to the Downloads tab",
        ),
        (
            Action::Show(Tab::Settings),
            KeyboardShortcut::new(command, Key::Num2),
            "Go to the Settings tab",
        ),
        (
            Action::SelectAll,
            KeyboardShortcut::new(command, Key::A),
            "Tick every podcast",
        ),
        (
            Action::SelectNone,
            KeyboardShortcut::new(command_shift, Key::A),
            "Untick every podcast",
        ),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Completed,
    Stopped,
}

struct EpisodeRow {
    title: String,
    /// What it is called on disk, which is what the output reports for anything
    /// that actually landed there.
    file_name: String,
    status: EpisodeStatus,
    done: u64,
    total: Option<u64>,
}

impl EpisodeRow {
    fn finished(&self) -> bool {
        matches!(
            self.status,
            EpisodeStatus::Done | EpisodeStatus::Skipped | EpisodeStatus::Failed(_)
        )
    }
}

/// One podcast from the OPML: both a thing to tick and, once a run starts, a
/// thing with progress of its own.
struct FeedRow {
    title: String,
    url: String,
    selected: bool,
    folder: String,
    status: FeedStatus,
    episodes: Vec<EpisodeRow>,
    /// Set once the engine has said how many episodes there are; until then the
    /// row can only report that it is still fetching.
    listed: bool,
}

impl FeedRow {
    fn finished_count(&self) -> usize {
        self.episodes.iter().filter(|e| e.finished()).count()
    }

    fn failed_count(&self) -> usize {
        self.episodes
            .iter()
            .filter(|e| matches!(e.status, EpisodeStatus::Failed(_)))
            .count()
    }
}

/// A line in the output box, with the colour its severity earns.
struct OutputLine {
    text: String,
    kind: OutputKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutputKind {
    Plain,
    Good,
    Muted,
    Bad,
}

#[derive(Default)]
struct Totals {
    episodes: usize,
    finished: usize,
    downloaded: usize,
    skipped: usize,
    failed: usize,
    bytes: u64,
}

pub struct PodBatchApp {
    tab: Tab,

    opml_path: Option<PathBuf>,
    out_dir: PathBuf,
    concurrency: usize,
    limit_enabled: bool,
    limit: usize,
    skip_existing: bool,
    play_sounds: bool,

    feeds: Vec<FeedRow>,
    /// Engine feed index -> index into `feeds`. The engine is only told about
    /// the ticked podcasts, so its numbering is not ours.
    running_map: Vec<usize>,

    output: Vec<OutputLine>,
    /// A scroll position to force on the next frame only, set by the keyboard
    /// scrolling in [`PodBatchApp::output_pane`]. `None` the rest of the time,
    /// leaving the box to follow the newest line or stay where it was put.
    output_scroll_to: Option<f32>,

    running: bool,
    cancelling: bool,
    /// How the last run ended, shown in the headline once it has. `None` before
    /// the first run.
    outcome: Option<Outcome>,
    cancel: Option<Cancel>,
    rx: Option<UnboundedReceiver<Update>>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl PodBatchApp {
    pub fn new(cc: &eframe::CreationContext<'_>, opml_path: Option<PathBuf>) -> Self {
        theme::apply(&cc.egui_ctx);

        // The engine calls this after every message so the window wakes up and
        // repaints. Without it the UI would only update when the mouse moved.
        let ctx = cc.egui_ctx.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());

        let mut app = Self {
            tab: Tab::Downloads,
            opml_path: None,
            out_dir: default_out_dir(),
            concurrency: 4,
            limit_enabled: false,
            limit: 10,
            skip_existing: true,
            play_sounds: true,
            feeds: Vec::new(),
            running_map: Vec::new(),
            output: Vec::new(),
            output_scroll_to: None,
            running: false,
            cancelling: false,
            outcome: None,
            cancel: None,
            rx: None,
            notify,
        };

        if let Some(path) = opml_path {
            app.load_opml(path);
        }
        app
    }

    // ---- subscription list ------------------------------------------------

    /// Read an OPML file and fill the left pane from it.
    ///
    /// Everything starts ticked: the common case is "download the lot", and
    /// un-ticking the few you don't want is less work than ticking the many you
    /// do.
    fn load_opml(&mut self, path: PathBuf) {
        // The map points into the list about to be replaced, so it stops
        // meaning anything the moment the list changes.
        self.running_map.clear();

        match opml::parse_file(&path) {
            Ok(subs) => {
                self.feeds = subs
                    .into_iter()
                    .map(|s| FeedRow {
                        title: s.title,
                        url: s.url,
                        selected: true,
                        folder: String::new(),
                        status: FeedStatus::Pending,
                        episodes: Vec::new(),
                        listed: false,
                    })
                    .collect();
                self.outcome = None;
                self.say(
                    OutputKind::Plain,
                    format!(
                        "Loaded {} podcast{} from {}",
                        self.feeds.len(),
                        if self.feeds.len() == 1 { "" } else { "s" },
                        path.display()
                    ),
                );
                self.opml_path = Some(path);
            }
            Err(e) => {
                self.feeds.clear();
                self.say(OutputKind::Bad, format!("{}: {e}", path.display()));
                // Kept, so the field still shows what was tried and the error
                // sits next to the file it is about.
                self.opml_path = Some(path);
            }
        }
    }

    fn selected_count(&self) -> usize {
        self.feeds.iter().filter(|f| f.selected).count()
    }

    // ---- output box -------------------------------------------------------

    fn say(&mut self, kind: OutputKind, text: String) {
        self.output.push(OutputLine { text, kind });
        if self.output.len() > OUTPUT_LIMIT {
            let excess = self.output.len() - OUTPUT_LIMIT;
            self.output.drain(0..excess);
        }
    }

    // ---- running ----------------------------------------------------------

    fn totals(&self) -> Totals {
        let mut t = Totals::default();
        for feed in self.feeds.iter().filter(|f| f.selected) {
            t.episodes += feed.episodes.len();
            for ep in &feed.episodes {
                match ep.status {
                    EpisodeStatus::Done => {
                        t.downloaded += 1;
                        t.bytes += ep.done;
                    }
                    EpisodeStatus::Skipped => t.skipped += 1,
                    EpisodeStatus::Failed(_) => t.failed += 1,
                    _ => {}
                }
                if ep.finished() {
                    t.finished += 1;
                }
            }
        }
        t
    }

    /// True once every running feed has reported its episode list, which is when
    /// the overall total stops moving and a percentage starts meaning something.
    fn all_listed(&self) -> bool {
        self.running_map.iter().all(|&i| {
            self.feeds
                .get(i)
                .is_none_or(|f| f.listed || f.status.is_settled())
        })
    }

    fn start(&mut self) {
        let subscriptions: Vec<opml::Subscription> = self
            .feeds
            .iter()
            .filter(|f| f.selected)
            .map(|f| opml::Subscription {
                title: f.title.clone(),
                url: f.url.clone(),
            })
            .collect();

        if subscriptions.is_empty() {
            self.say(OutputKind::Bad, "Tick at least one podcast first.".into());
            return;
        }

        self.running_map = self
            .feeds
            .iter()
            .enumerate()
            .filter(|(_, f)| f.selected)
            .map(|(i, _)| i)
            .collect();

        for feed in &mut self.feeds {
            feed.status = FeedStatus::Pending;
            feed.episodes.clear();
            feed.listed = false;
            feed.folder.clear();
        }

        self.output.clear();
        self.output_scroll_to = None;
        self.cancelling = false;
        self.outcome = None;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Cancel::new();

        engine::spawn(
            Settings {
                subscriptions,
                out_dir: self.out_dir.clone(),
                concurrency: self.concurrency,
                limit: self.limit_enabled.then_some(self.limit.max(1)),
                skip_existing: self.skip_existing,
            },
            tx,
            cancel.clone(),
            Arc::clone(&self.notify),
        );

        self.rx = Some(rx);
        self.cancel = Some(cancel);
        self.running = true;
    }

    /// Do what a shortcut asked for, if it is a thing that can be done now.
    ///
    /// The guards are the same ones the buttons apply, so a shortcut can never
    /// reach a state a click could not — pressing Start twice does not start
    /// two runs, and Escape during no run does nothing rather than something.
    fn perform(&mut self, action: Action) {
        match action {
            Action::ChooseOpml => {
                if !self.running
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Subscription list", &["opml", "xml"])
                        .pick_file()
                {
                    self.load_opml(path);
                }
            }
            Action::Start => {
                if !self.running {
                    self.tab = Tab::Downloads;
                    self.start();
                }
            }
            Action::Stop => {
                if self.running && !self.cancelling {
                    self.stop();
                }
            }
            Action::Show(tab) => self.tab = tab,
            Action::SelectAll => {
                if !self.running {
                    self.feeds.iter_mut().for_each(|f| f.selected = true);
                }
            }
            Action::SelectNone => {
                if !self.running {
                    self.feeds.iter_mut().for_each(|f| f.selected = false);
                }
            }
        }
    }

    /// Consume any shortcut pressed this frame.
    ///
    /// `consume_shortcut` takes the press out of the input queue, so a widget
    /// that also wants the key never sees it and the same press cannot be acted
    /// on twice.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // A field being typed into wants these keys for itself: in a number
        // field mid-edit, Cmd+A means "select all of this text" and Escape
        // means "cancel the edit", and neither should reach the podcast list.
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        for (action, shortcut, _) in shortcuts() {
            if ctx.input_mut(|i| i.consume_shortcut(&shortcut)) {
                self.perform(action);
            }
        }
    }

    fn stop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
            self.cancelling = true;
            self.say(
                OutputKind::Muted,
                "Stopping — letting the transfers in flight wind down…".into(),
            );
        }
    }

    /// Drain everything the engine has said since the last frame.
    fn drain(&mut self) {
        let Some(rx) = self.rx.as_mut() else { return };

        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }

        for update in updates {
            match update {
                Update::FeedFolder { feed, name } => {
                    if let Some(row) = self.row_mut(feed) {
                        row.folder = name;
                    }
                }
                Update::FeedStatus { feed, status } => {
                    if let Some(row) = self.row_mut(feed) {
                        row.status = status;
                    }
                }
                Update::Episodes { feed, episodes } => {
                    if let Some(row) = self.row_mut(feed) {
                        row.episodes = episodes
                            .into_iter()
                            .map(|e| EpisodeRow {
                                title: e.title,
                                file_name: e.file_name,
                                status: EpisodeStatus::Pending,
                                done: 0,
                                total: e.size_hint,
                            })
                            .collect();
                        row.listed = true;
                    }
                }
                Update::EpisodeStatus { feed, episode, status } => {
                    let Some(&index) = self.running_map.get(feed) else {
                        continue;
                    };
                    let Some(row) = self.feeds.get_mut(index) else {
                        continue;
                    };
                    let Some(ep) = row.episodes.get_mut(episode) else {
                        continue;
                    };
                    ep.status = status.clone();

                    // One line per episode as it settles, which is what makes
                    // the output box a record of the run rather than a log of
                    // things that went wrong.
                    let show = row.title.clone();
                    // What landed on disk is named by its file; what didn't is
                    // named by the episode it was going to be.
                    let file = ep.file_name.clone();
                    let title = ep.title.clone();
                    let size = human_bytes(ep.done);
                    match status {
                        EpisodeStatus::Done => self.say(
                            OutputKind::Good,
                            format!("{MARK_DONE} {show} — {file} ({size})"),
                        ),
                        EpisodeStatus::Skipped => self.say(
                            OutputKind::Muted,
                            format!("{MARK_SKIPPED} {show} — {file} (already downloaded)"),
                        ),
                        EpisodeStatus::Failed(e) => self.say(
                            OutputKind::Bad,
                            format!("{MARK_FAILED} {show} — {title}: {e}"),
                        ),
                        _ => {}
                    }
                }
                Update::Progress { feed, episode, done, total } => {
                    let Some(&index) = self.running_map.get(feed) else {
                        continue;
                    };
                    if let Some(ep) = self
                        .feeds
                        .get_mut(index)
                        .and_then(|f| f.episodes.get_mut(episode))
                    {
                        ep.done = done;
                        if total.is_some() {
                            ep.total = total;
                        }
                    }
                }
                Update::Log(line) => self.say(OutputKind::Plain, line),
                Update::Finished { cancelled } => {
                    self.outcome = Some(if cancelled {
                        Outcome::Stopped
                    } else {
                        Outcome::Completed
                    });
                    // Stopping was asked for, so it isn't news; the cue is for
                    // the run that ended on its own while nobody was watching.
                    if self.play_sounds && !cancelled {
                        sound::play(cue_for(&self.totals()));
                    }
                    self.running = false;
                    self.cancelling = false;
                    self.cancel = None;
                    self.rx = None;
                }
            }
        }
    }

    fn row_mut(&mut self, engine_index: usize) -> Option<&mut FeedRow> {
        let index = *self.running_map.get(engine_index)?;
        self.feeds.get_mut(index)
    }

    fn accept_dropped_files(&mut self, ctx: &egui::Context) {
        if self.running {
            return;
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(path) = dropped
            .iter()
            .map(|f| f.path().to_path_buf())
            .find(|p| !p.as_os_str().is_empty())
        {
            if path.is_dir() {
                self.out_dir = path;
            } else {
                self.load_opml(path);
            }
        }
    }

    // ---- panes ------------------------------------------------------------

    /// Two tabs, splitting the full width of the window between them.
    ///
    /// Full width rather than two small buttons in the corner: they are the only
    /// navigation the app has, so they are worth being unmissable and easy to
    /// hit, and a target that spans half the window is both.
    fn tab_bar(&mut self, ui: &mut Ui) {
        let tabs = [(Tab::Downloads, "Downloads"), (Tab::Settings, "Settings")];
        let gaps = ui.spacing().item_spacing.x * (tabs.len() - 1) as f32;
        let width = ((ui.available_width() - gaps) / tabs.len() as f32).max(1.0);

        ui.horizontal(|ui| {
            for (tab, label) in tabs {
                let button = egui::Button::selectable(self.tab == tab, label);
                if ui
                    .add_sized(egui::vec2(width, CONTROL_HEIGHT), button)
                    .clicked()
                {
                    self.tab = tab;
                }
            }
        });
    }

    /// The top of the Downloads tab: where the subscription list comes from, and
    /// the button that starts the run.
    fn opml_bar(&mut self, ui: &mut Ui) {
        let busy = self.running;

        ui.horizontal(|ui| {
            ui.label(RichText::new("Subscription list").strong());

            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("📂 Choose OPML file…")
                        .min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Subscription list", &["opml", "xml"])
                    .pick_file()
            {
                self.load_opml(path);
            }

            let muted = theme::palette(ui.visuals()).muted;
            match &self.opml_path {
                Some(path) => {
                    let shown = elide_path(path);
                    ui.add(egui::Label::new(RichText::new(shown).color(muted)).truncate())
                        .on_hover_text(path.display().to_string());
                }
                None => {
                    ui.label(
                        RichText::new("None chosen — or drop one onto this window").color(muted),
                    );
                }
            }
        });

        ui.horizontal(|ui| {
            let ready = self.selected_count() > 0;
            if self.running {
                let label = if self.cancelling { "Stopping…" } else { "■ Stop" };
                if ui
                    .add_enabled(
                        !self.cancelling,
                        egui::Button::new(label).min_size(egui::vec2(220.0, CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    self.stop();
                }
            } else {
                let response = ui.add_enabled(
                    ready,
                    egui::Button::new("▶ Download episodes")
                        .min_size(egui::vec2(220.0, CONTROL_HEIGHT)),
                );
                if !ready {
                    response.on_hover_text("Choose an OPML file and tick at least one podcast.");
                } else if response.clicked() {
                    self.start();
                }
            }

            let muted = theme::palette(ui.visuals()).muted;
            if !self.feeds.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "{} of {} selected",
                        self.selected_count(),
                        self.feeds.len()
                    ))
                    .color(muted),
                );
            }
        });
    }

    /// The left half: the podcasts in the OPML, and which of them to fetch.
    fn feed_pane(&mut self, ui: &mut Ui) {
        let busy = self.running;
        let palette = theme::palette(ui.visuals());

        ui.horizontal(|ui| {
            ui.label(RichText::new("Podcasts").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!busy && !self.feeds.is_empty(), egui::Button::new("None"))
                    .clicked()
                {
                    self.feeds.iter_mut().for_each(|f| f.selected = false);
                }
                if ui
                    .add_enabled(!busy && !self.feeds.is_empty(), egui::Button::new("All"))
                    .clicked()
                {
                    self.feeds.iter_mut().for_each(|f| f.selected = true);
                }
            });
        });

        if self.feeds.is_empty() {
            ui.add_space(8.0);
            ui.label(
                RichText::new("Choose an OPML file and the podcasts in it appear here.")
                    .color(palette.muted),
            );
            return;
        }

        egui::ScrollArea::vertical()
            .id_salt("feeds")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for feed in &mut self.feeds {
                    ui.horizontal(|ui| {
                        // The status is laid out first, from the right, so that
                        // it has claimed its width before the title is asked to
                        // fit. A truncating title placed first takes the whole
                        // row and the status then draws on top of it.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (text, colour) = feed_status(feed, &palette);
                            if !text.is_empty() {
                                ui.label(RichText::new(text).color(colour).small());
                            }

                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    // The title is the tick box's own label
                                    // rather than a separate one next to it.
                                    // That label is what egui hands to the
                                    // screen reader as the control's name, and
                                    // an unnamed tick box in a list of forty
                                    // podcasts is announced as "checkbox",
                                    // forty times over.
                                    ui.style_mut().wrap_mode =
                                        Some(egui::TextWrapMode::Truncate);
                                    let title =
                                        egui::Atom::from(feed.title.as_str()).atom_shrink(true);
                                    ui.add_enabled(
                                        !busy,
                                        egui::Checkbox::new(&mut feed.selected, title),
                                    )
                                    .on_hover_text(&feed.url);
                                },
                            );
                        });
                    });
                }
            });
    }

    /// The right half: what the run is doing, line by line.
    ///
    /// The box takes keyboard focus of its own, because a scrolling region full
    /// of plain text contains nothing focusable and egui's `ScrollArea` has no
    /// keyboard handling — so without this the run's history is readable with a
    /// mouse and by no other means. Focused, it scrolls with the arrow, page and
    /// Home/End keys.
    fn output_pane(&mut self, ui: &mut Ui) {
        let palette = theme::palette(ui.visuals());
        ui.label(RichText::new("Output").strong());

        let focus_id = ui.make_persistent_id("output-box");
        let focused = ui.memory(|m| m.has_focus(focus_id));

        let frame = egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .stroke(if focused {
                // The focus ring. `active` is the style egui uses for the
                // keyboard-focused widget, and this box has to look focused the
                // same way every other control does.
                ui.visuals().widgets.active.bg_stroke
            } else {
                ui.visuals().widgets.noninteractive.bg_stroke
            })
            .inner_margin(8.0)
            .show(ui, |ui| {
                let mut area = egui::ScrollArea::vertical()
                    .id_salt("output")
                    .auto_shrink([false, false])
                    // Follows the newest line, but only while the reader is
                    // already at the bottom — egui stops sticking as soon as
                    // they scroll away, so reading back through a run isn't
                    // interrupted every time an episode finishes.
                    .stick_to_bottom(true);

                // A one-shot: taken, so the offset is forced on the frame the
                // key was pressed and not after. Re-applying it every frame
                // would pin the box there and the wheel would stop working.
                if let Some(offset) = self.output_scroll_to.take() {
                    area = area.vertical_scroll_offset(offset);
                }

                let out = area.show(ui, |ui| {
                    if self.output.is_empty() {
                        ui.label(RichText::new("Nothing to report yet.").color(palette.muted));
                    }
                    for line in &self.output {
                        let colour = match line.kind {
                            OutputKind::Plain => ui.visuals().text_color(),
                            OutputKind::Good => palette.ok,
                            OutputKind::Muted => palette.muted,
                            OutputKind::Bad => palette.bad,
                        };
                        ui.label(RichText::new(&line.text).color(colour));
                    }
                });

                (out.state.offset.y, out.inner_rect.height(), out.content_size.y)
            });

        let (offset, viewport, content) = frame.inner;

        // Focusable but not interactive: this rect covers the scroll bar, and a
        // clickable region laid over it would take the presses meant for
        // dragging the bar. Tab still reaches it, which is the point.
        let response = ui.interact(
            frame.response.rect,
            focus_id,
            egui::Sense::focusable_noninteractive(),
        );

        if response.has_focus() {
            let line = ui.text_style_height(&egui::TextStyle::Body);
            let page = (viewport - line).max(line);
            let furthest = (content - viewport).max(0.0);

            let step = ui.input(|i| {
                let mut step = 0.0;
                for (key, by) in [
                    (egui::Key::ArrowDown, line),
                    (egui::Key::ArrowUp, -line),
                    (egui::Key::PageDown, page),
                    (egui::Key::PageUp, -page),
                ] {
                    if i.key_pressed(key) {
                        step += by;
                    }
                }
                step
            });

            let (home, end) = ui.input(|i| (i.key_pressed(egui::Key::Home), i.key_pressed(egui::Key::End)));

            if home {
                self.output_scroll_to = Some(0.0);
            } else if end {
                self.output_scroll_to = Some(furthest);
            } else if step != 0.0 {
                self.output_scroll_to = Some((offset + step).clamp(0.0, furthest));
            }
        }
    }

    /// The bar across the bottom of the tab, spanning both panes.
    fn progress_bar(&mut self, ui: &mut Ui) {
        let totals = self.totals();
        let palette = theme::palette(ui.visuals());

        ui.horizontal(|ui| {
            if self.running {
                ui.spinner();
            }

            // "0/0 episodes" before anything has run is a true statement that
            // reads like a failure, so a run that hasn't happened says so.
            let headline = if self.feeds.is_empty() || (!self.running && self.outcome.is_none()) {
                "Ready.".to_string()
            } else if self.running && !self.all_listed() {
                let read = self
                    .running_map
                    .iter()
                    .filter(|&&i| {
                        self.feeds
                            .get(i)
                            .is_some_and(|f| f.listed || f.status.is_settled())
                    })
                    .count();
                format!("Reading feeds — {read}/{} done", self.running_map.len())
            } else {
                let counts = format!("{}/{} episodes", totals.finished, totals.episodes);
                match self.outcome {
                    Some(Outcome::Stopped) if !self.running => format!("Stopped at {counts}"),
                    Some(Outcome::Completed) if !self.running => format!("Finished — {counts}"),
                    _ => counts,
                }
            };
            ui.label(RichText::new(headline).strong());

            if totals.downloaded > 0 {
                ui.label(
                    RichText::new(format!(
                        "· {} downloaded ({})",
                        totals.downloaded,
                        human_bytes(totals.bytes)
                    ))
                    .color(palette.ok),
                );
            }
            if totals.skipped > 0 {
                ui.label(
                    RichText::new(format!("· {} already had", totals.skipped))
                        .color(palette.muted),
                );
            }
            if totals.failed > 0 {
                ui.label(RichText::new(format!("· {} failed", totals.failed)).color(palette.bad));
            }
        });

        let fraction = if totals.episodes == 0 {
            0.0
        } else {
            totals.finished as f32 / totals.episodes as f32
        };
        let bar = ui.add(
            egui::ProgressBar::new(fraction)
                .desired_width(ui.available_width())
                .desired_height(PROGRESS_HEIGHT),
        );
        theme::percentage_across(ui, bar.rect, fraction);
    }

    fn settings_pane(&mut self, ui: &mut Ui) {
        let busy = self.running;
        let muted = theme::palette(ui.visuals()).muted;

        ui.add_space(4.0);
        ui.label(RichText::new("Save episodes in").strong());
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("📁 Change folder…")
                        .min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
                )
                .clicked()
                && let Some(dir) = rfd::FileDialog::new()
                    .set_directory(existing_ancestor(&self.out_dir))
                    .pick_folder()
            {
                self.out_dir = dir;
            }
            ui.add(egui::Label::new(RichText::new(elide_path(&self.out_dir)).color(muted)).truncate())
                .on_hover_text(self.out_dir.display().to_string());
        });
        ui.label(
            RichText::new("Each podcast gets a subfolder of its own in here.")
                .color(muted)
                .small(),
        );

        ui.add_space(12.0);
        ui.label(RichText::new("What to download").strong());
        ui.add_enabled(
            !busy,
            egui::Checkbox::new(&mut self.skip_existing, "Skip episodes already downloaded"),
        )
        .on_hover_text(
            "Leaves a file alone when it is already on disk at the size the feed says it \
             should be. Turn this off to download everything again.",
        );
        ui.horizontal(|ui| {
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut self.limit_enabled, "Only the newest"),
            );
            ui.add_enabled(
                !busy && self.limit_enabled,
                egui::DragValue::new(&mut self.limit).range(1..=1000),
            );
            ui.label("episodes per podcast");
        });

        ui.add_space(12.0);
        ui.label(RichText::new("While downloading").strong());
        ui.horizontal(|ui| {
            ui.label("Downloads at once");
            ui.add_enabled(
                !busy,
                egui::DragValue::new(&mut self.concurrency).range(1..=16),
            )
            .on_hover_text("How many files to fetch in parallel, across all podcasts.");
        });
        ui.checkbox(&mut self.play_sounds, "Sound when finished")
            .on_hover_text(
                "Plays a short cue when the run ends — one sound if everything downloaded, \
                 another if anything failed.",
            );

        ui.add_space(12.0);
        ui.label(RichText::new("Keyboard").strong());
        ui.label(
            RichText::new(
                "Every part of this app can be operated without a mouse. Tab moves forward \
                 through the controls, Shift+Tab back, Space toggles a tick box and Enter \
                 presses a button. The output box takes focus of its own and scrolls with \
                 the arrow, Page and Home/End keys.",
            )
            .color(muted),
        );
        ui.add_space(6.0);

        // Written out from the same table that handles the keys, so the list
        // cannot describe a shortcut the app does not have.
        egui::Grid::new("shortcuts")
            .num_columns(2)
            .spacing(egui::vec2(18.0, 6.0))
            .show(ui, |ui| {
                for (_, shortcut, description) in shortcuts() {
                    ui.label(RichText::new(ui.ctx().format_shortcut(&shortcut)).strong());
                    ui.label(description);
                    ui.end_row();
                }
            });
    }
}

/// The status a podcast row reports on the right of its name.
fn feed_status(feed: &FeedRow, palette: &theme::Palette) -> (String, egui::Color32) {
    match &feed.status {
        FeedStatus::Pending => (String::new(), palette.muted),
        FeedStatus::Fetching => ("reading…".to_string(), palette.accent),
        FeedStatus::Downloading => (
            format!("{}/{}", feed.finished_count(), feed.episodes.len()),
            palette.accent,
        ),
        FeedStatus::Done => {
            let failed = feed.failed_count();
            if failed > 0 {
                (format!("{failed} failed"), palette.bad)
            } else {
                (format!("{} episodes", feed.episodes.len()), palette.ok)
            }
        }
        FeedStatus::Failed(e) => (e.clone(), palette.bad),
    }
}

/// Which cue a finished run has earned. Anything that failed makes it a failure,
/// however much else succeeded — a run that quietly dropped one episode should
/// not sound like a clean one.
fn cue_for(totals: &Totals) -> Cue {
    if totals.failed > 0 {
        Cue::Failure
    } else {
        Cue::Success
    }
}

impl eframe::App for PodBatchApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain();
        self.accept_dropped_files(&ctx);
        self.handle_shortcuts(&ctx);

        // The engine wakes us on every message, but a download reports at most
        // ten times a second and the spinner needs a steadier clock than that.
        if self.running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::Panel::top(egui::Id::new("tabs")).show(ui, |ui| {
            ui.add_space(6.0);
            self.tab_bar(ui);
            ui.add_space(2.0);
        });

        match self.tab {
            Tab::Downloads => {
                egui::Panel::top(egui::Id::new("opml")).show(ui, |ui| {
                    ui.add_space(6.0);
                    self.opml_bar(ui);
                    ui.add_space(6.0);
                });

                // Added before the side panel, so the bar spans the whole width
                // underneath both halves of the split rather than just one.
                egui::Panel::bottom(egui::Id::new("progress")).show(ui, |ui| {
                    ui.add_space(6.0);
                    self.progress_bar(ui);
                    ui.add_space(6.0);
                });

                egui::Panel::left(egui::Id::new("feeds"))
                    .resizable(true)
                    .default_size(340.0)
                    .min_size(220.0)
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        self.feed_pane(ui);
                    });

                egui::CentralPanel::default().show(ui, |ui| {
                    ui.add_space(6.0);
                    self.output_pane(ui);
                });
            }
            Tab::Settings => {
                egui::CentralPanel::default().show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.settings_pane(ui));
                });
            }
        }
    }
}

impl FeedStatus {
    /// Whether this feed will produce no further episode list.
    fn is_settled(&self) -> bool {
        matches!(self, FeedStatus::Done | FeedStatus::Failed(_))
    }
}

/// `~/podcasts`, or the current directory on the rare system with no home.
fn default_out_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("podcasts")
}

/// The nearest ancestor that exists, so the folder picker opens somewhere real
/// even before `~/podcasts` has been created.
fn existing_ancestor(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|p| p.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Show a path with `~` for the home directory: shorter, and it doesn't put the
/// user's account name on screen during a screen-share.
fn elide_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(status: EpisodeStatus, done: u64) -> EpisodeRow {
        EpisodeRow {
            title: "t".into(),
            file_name: "t.mp3".into(),
            status,
            done,
            total: None,
        }
    }

    fn feed(selected: bool, episodes: Vec<EpisodeRow>) -> FeedRow {
        FeedRow {
            title: "show".into(),
            url: "https://x.test/feed".into(),
            selected,
            folder: String::new(),
            status: FeedStatus::Done,
            episodes,
            listed: true,
        }
    }

    #[test]
    fn finished_covers_every_terminal_state() {
        assert!(episode(EpisodeStatus::Done, 0).finished());
        assert!(episode(EpisodeStatus::Skipped, 0).finished());
        assert!(episode(EpisodeStatus::Failed("x".into()), 0).finished());
        assert!(!episode(EpisodeStatus::Pending, 0).finished());
        assert!(!episode(EpisodeStatus::Downloading, 0).finished());
        // Cancelled episodes stay unfinished: the run stopped, they didn't.
        assert!(!episode(EpisodeStatus::Cancelled, 0).finished());
    }

    /// An un-ticked podcast must not move the numbers on the bar, or un-ticking
    /// one that has already been downloaded would show a run as incomplete.
    #[test]
    fn totals_only_count_the_selected_podcasts() {
        let mut app = test_app();
        app.feeds = vec![
            feed(
                true,
                vec![
                    episode(EpisodeStatus::Done, 100),
                    episode(EpisodeStatus::Failed("no".into()), 0),
                ],
            ),
            feed(false, vec![episode(EpisodeStatus::Done, 5_000)]),
        ];

        let totals = app.totals();
        assert_eq!(totals.episodes, 2);
        assert_eq!(totals.finished, 2);
        assert_eq!(totals.downloaded, 1);
        assert_eq!(totals.failed, 1);
        // Only the ticked podcast's bytes.
        assert_eq!(totals.bytes, 100);
    }

    #[test]
    fn one_failure_is_enough_to_make_the_run_a_failure() {
        let totals = |downloaded, failed| Totals {
            episodes: downloaded + failed,
            finished: downloaded + failed,
            downloaded,
            failed,
            ..Totals::default()
        };
        assert_eq!(cue_for(&totals(3, 0)), Cue::Success);
        assert_eq!(cue_for(&totals(0, 0)), Cue::Success);
        assert_eq!(cue_for(&totals(99, 1)), Cue::Failure);
    }

    /// The engine only ever hears about the ticked podcasts, so its feed numbers
    /// are not the window's. Getting this wrong would post every episode's
    /// progress against the wrong show.
    #[test]
    fn engine_indices_map_back_through_the_selection() {
        let mut app = test_app();
        app.feeds = vec![feed(false, vec![]), feed(true, vec![]), feed(true, vec![])];
        app.feeds[1].title = "second".into();
        app.feeds[2].title = "third".into();
        app.running_map = vec![1, 2];

        assert_eq!(app.row_mut(0).unwrap().title, "second");
        assert_eq!(app.row_mut(1).unwrap().title, "third");
        assert!(app.row_mut(2).is_none());
    }

    #[test]
    fn a_bad_opml_file_reports_instead_of_leaving_a_stale_list() {
        let dir = std::env::temp_dir().join(format!(
            "podbatch-opml-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let good = dir.join("good.opml");
        std::fs::write(
            &good,
            r#"<opml><body><outline type="rss" text="A" xmlUrl="https://a.test/f"/></body></opml>"#,
        )
        .expect("write");

        let bad = dir.join("bad.opml");
        std::fs::write(&bad, "not xml at all").expect("write");

        let mut app = test_app();
        app.load_opml(good);
        assert_eq!(app.feeds.len(), 1);
        assert!(app.feeds[0].selected, "podcasts start ticked");

        app.load_opml(bad);
        assert!(app.feeds.is_empty(), "a failed load must not leave the old list");
        assert_eq!(app.output.last().map(|l| l.kind), Some(OutputKind::Bad));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every symbol the UI writes has to exist somewhere in the font stack.
    ///
    /// egui draws a character no font in its chain covers as an empty box, and
    /// it does it silently — the code compiles, the tests pass, and the app
    /// ships with a row of tofu down the side of the output. This asks the real
    /// font stack the app configures, which is the only thing that actually
    /// knows. It has already earned its keep: `▼` and `↓` both look perfectly
    /// reasonable and are in none of the bundled fonts.
    ///
    /// Width, not `Fonts::has_glyphs`: that reports a character missing whenever
    /// it resolves to the same face that supplies the replacement glyph, which
    /// on this stack is NotoEmoji — so it calls `▶`, `✔`, `✖` and `📂` missing
    /// when all four render perfectly. An unresolved character has no advance at
    /// all, and none of the symbols below is a zero-width one.
    #[test]
    fn every_glyph_the_ui_writes_has_a_glyph_to_draw_it_with() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        // Fonts are built lazily on the first frame. The frame's texture delta
        // has to be consumed or dropping it panics — there is no renderer here
        // to hand it to.
        let mut output = ctx.run_ui(Default::default(), |_| {});
        output.textures_delta.clear();

        let symbols = [
            MARK_DONE,
            MARK_SKIPPED,
            MARK_FAILED,
            "📂 Choose OPML file…",
            "📁 Change folder…",
            "▶ Download episodes",
            "■ Stop",
            "Stopping — letting the transfers in flight wind down…",
        ];

        // Resolved before taking the font lock: reading the style inside the
        // closure would be a second lock on the same context, which deadlocks.
        let font = egui::TextStyle::Button.resolve(&ctx.style_of(egui::Theme::Light));

        let missing: Vec<char> = ctx.fonts_mut(|fonts| {
            symbols
                .iter()
                .flat_map(|s| s.chars())
                .filter(|&c| !c.is_whitespace() && fonts.glyph_width(&font, c) <= 0.0)
                .collect()
        });

        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }

    /// The other half of the check above: a character genuinely absent from the
    /// stack must be seen as absent, or the test is only ever proving that
    /// `glyph_width` returns something.
    #[test]
    fn a_missing_glyph_is_actually_detected() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut output = ctx.run_ui(Default::default(), |_| {});
        output.textures_delta.clear();

        let font = egui::TextStyle::Button.resolve(&ctx.style_of(egui::Theme::Light));
        ctx.fonts_mut(|fonts| {
            // A private-use character, which no font here has any reason to
            // carry, and the down-pointing triangle this test was written for.
            for c in ['\u{E000}', '▼', '↓'] {
                assert_eq!(
                    fonts.glyph_width(&font, c),
                    0.0,
                    "expected {c:?} to be missing"
                );
            }
        });
    }

    /// Two actions on one chord means one of them silently never happens.
    #[test]
    fn no_two_shortcuts_share_a_chord() {
        let all = shortcuts();
        for (i, (_, a, _)) in all.iter().enumerate() {
            for (_, b, _) in all.iter().skip(i + 1) {
                assert!(a != b, "two actions share {a:?}");
            }
        }
    }

    /// Every action must be reachable from the keyboard, and every listed
    /// shortcut must describe itself — the Settings pane prints this table
    /// verbatim.
    #[test]
    fn every_action_has_exactly_one_shortcut_and_a_description() {
        let all = shortcuts();
        let actions: Vec<Action> = all.iter().map(|(a, _, _)| *a).collect();

        for expected in [
            Action::ChooseOpml,
            Action::Start,
            Action::Stop,
            Action::Show(Tab::Downloads),
            Action::Show(Tab::Settings),
            Action::SelectAll,
            Action::SelectNone,
        ] {
            let count = actions.iter().filter(|&&a| a == expected).count();
            assert_eq!(count, 1, "{expected:?} should have exactly one shortcut");
        }
        assert_eq!(actions.len(), 7, "an action was added without a test");

        for (action, _, description) in &all {
            assert!(!description.is_empty(), "{action:?} has no description");
        }
    }

    /// A shortcut must not be able to reach a state a click could not. Starting
    /// a second run over a live one would leak the first engine's channel and
    /// double the traffic.
    #[test]
    fn shortcuts_respect_the_same_guards_as_the_buttons() {
        let mut app = test_app();
        app.feeds = vec![feed(true, vec![])];
        app.running = true;

        app.perform(Action::SelectNone);
        assert!(app.feeds[0].selected, "selection is frozen while running");

        app.perform(Action::Start);
        assert!(app.rx.is_none(), "a second run must not start over a live one");

        // Escape with nothing running is a no-op, not a crash.
        app.running = false;
        app.perform(Action::Stop);
        assert!(!app.cancelling);
    }

    #[test]
    fn tab_shortcuts_move_between_tabs() {
        let mut app = test_app();
        app.perform(Action::Show(Tab::Settings));
        assert_eq!(app.tab, Tab::Settings);
        app.perform(Action::Show(Tab::Downloads));
        assert_eq!(app.tab, Tab::Downloads);

        // Starting a run brings the tab that shows it back into view; watching
        // progress from the Settings tab is not a thing anyone asked for.
        app.feeds = vec![feed(true, vec![])];
        app.tab = Tab::Settings;
        app.perform(Action::Start);
        assert_eq!(app.tab, Tab::Downloads);
    }

    #[test]
    fn select_all_and_none_cover_every_podcast() {
        let mut app = test_app();
        app.feeds = vec![feed(true, vec![]), feed(false, vec![])];

        app.perform(Action::SelectNone);
        assert_eq!(app.selected_count(), 0);
        app.perform(Action::SelectAll);
        assert_eq!(app.selected_count(), 2);
    }

    #[test]
    fn elides_the_home_directory() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(elide_path(&home.join("podcasts")), "~/podcasts");
        }
        assert_eq!(elide_path(Path::new("/opt/media")), "/opt/media");
    }

    /// An app with no egui context behind it, for testing the parts that are
    /// state rather than painting.
    fn test_app() -> PodBatchApp {
        PodBatchApp {
            tab: Tab::Downloads,
            opml_path: None,
            out_dir: PathBuf::from("/tmp/out"),
            concurrency: 4,
            limit_enabled: false,
            limit: 10,
            skip_existing: true,
            play_sounds: false,
            feeds: Vec::new(),
            running_map: Vec::new(),
            output: Vec::new(),
            output_scroll_to: None,
            running: false,
            cancelling: false,
            outcome: None,
            cancel: None,
            rx: None,
            notify: Arc::new(|| {}),
        }
    }
}
