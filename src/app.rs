//! The window.
//!
//! Three tabs. **Downloads** is the one the app opens on and the one that does
//! the work: the subscription list goes in at the top, the podcasts it contains
//! are listed on the left to be picked from, what the run is doing is written on
//! the right, and the progress bar spans the width underneath both.
//! **Transcripts** turns episodes already on disk into text, laid out the same
//! way and driven by [`transcribe`](crate::transcribe) rather than the download
//! engine. **Settings** holds the things that are set once and then left alone.
//!
//! The OPML is read as soon as it is chosen rather than when the run starts,
//! because the list of podcasts is what the left pane is for — you cannot choose
//! from a list that only exists once you have committed to downloading all of it.
//!
//! Everything the engine reports arrives on a channel and is drained once per
//! frame in [`PodBatchApp::drain`]; nothing in here ever touches the network, so
//! the window keeps painting no matter how slow a feed is.
//!
//! Two things are asked before they are done: starting a run, once the feeds
//! have been read and the size of the job is known, and stopping one. Both go
//! through [`Dialog`], and both are asked the same way whether the button or the
//! keyboard set them off.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{AtomExt as _, RichText, Ui};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::document;
use crate::engine::{self, Cancel, EpisodeStatus, FeedStatus, Plan, Proceed, Settings, Update};
use crate::logs;
use crate::opml;
use crate::sound::{self, Cue};
use crate::theme::{self, CONTROL_HEIGHT, PROGRESS_HEIGHT};
use crate::tools;
use crate::transcribe;
use crate::util::{human_bytes, human_duration, human_rate, transfer_seconds};

/// How many output lines to keep. Long enough to cover a whole run, short enough
/// that a pathological feed can't grow it without bound.
const OUTPUT_LIMIT: usize = 2000;

/// The least time between two cues.
///
/// Four downloads finish at once often enough, and four overlapping chimes are
/// noise rather than information; the run says the same thing in writing on the
/// line above, so a cue that has to be dropped costs nothing.
const CUE_GAP: Duration = Duration::from_millis(700);

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
    Transcripts,
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
    ChooseAudioFolder,
    Transcribe,
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
            "Stop the run in progress, after asking",
        ),
        (
            Action::ChooseAudioFolder,
            KeyboardShortcut::new(command_shift, Key::O),
            "Choose a folder of episodes to transcribe",
        ),
        (
            Action::Transcribe,
            KeyboardShortcut::new(command_shift, Key::Enter),
            "Start transcribing",
        ),
        (
            Action::Show(Tab::Downloads),
            KeyboardShortcut::new(command, Key::Num1),
            "Go to the Downloads tab",
        ),
        (
            Action::Show(Tab::Transcripts),
            KeyboardShortcut::new(command, Key::Num2),
            "Go to the Transcripts tab",
        ),
        (
            Action::Show(Tab::Settings),
            KeyboardShortcut::new(command, Key::Num3),
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

/// A question put to the user, over the top of everything else.
///
/// Only one can be up at a time: both are about the run as a whole, and the
/// second cannot arise while the first is unanswered — the keyboard is held
/// while a dialog is open, and the buttons underneath are behind the modal.
enum Dialog {
    /// The feeds have been read; this is what downloading them would cost.
    Confirm(Plan),
    /// A run is going and something asked for it to stop.
    Stop,
    /// Something is about to be put on the user's machine.
    ///
    /// Never skipped and never batched: the user's instruction is that nothing
    /// is installed without asking, so every package, every model and every
    /// download is its own question with the exact command written out in it.
    Install { tool: tools::Tool, install: tools::Install },
    /// Transcribing is minutes per episode, so the size of the job is quoted
    /// before it starts for the same reason a download is.
    Transcribe { files: usize, labelling: bool },
    /// A transcription run is going and something asked for it to stop.
    StopTranscribing,
}

impl Dialog {
    /// The question, as the debug log names it — a run that did nothing is
    /// usually a question somebody answered, and this is what says which.
    fn describe(&self) -> String {
        match self {
            Dialog::Confirm(plan) => format!(
                "\"download {} episode(s), {}?\"",
                plan.episodes,
                human_bytes(plan.bytes)
            ),
            Dialog::Stop => "\"stop downloading?\"".to_string(),
            Dialog::Install { tool, .. } => format!("\"install {}?\"", tool.label()),
            Dialog::Transcribe { files, .. } => format!("\"transcribe {files} file(s)?\""),
            Dialog::StopTranscribing => "\"stop transcribing?\"".to_string(),
        }
    }
}

/// A file in the chosen folder, and how far its transcript has got.
struct AudioRow {
    path: PathBuf,
    name: String,
    selected: bool,
    status: transcribe::Status,
    /// How far through the audio Whisper is, once it is the one running.
    fraction: f32,
    /// Whether a transcript is already sitting beside it, worked out when the
    /// folder is scanned rather than every frame.
    transcribed: bool,
}

/// What the installer thread has to say.
enum InstallUpdate {
    Line(String),
    Progress { done: u64, total: Option<u64> },
    Finished(Result<(), String>),
}

/// A speed to estimate from, and where it came from — which decides how the
/// estimate built on it can honestly be worded.
#[derive(Clone, Copy)]
enum Speed {
    /// One connection, timed on a slice of a real episode before the run. The
    /// run puts several transfers in flight at once, so a time worked out from
    /// this is a ceiling: the answer should come out quicker, not slower.
    Probed(f64),
    /// What a whole run actually managed, start to finish. This already counts
    /// everything that was running at once, so it needs no such allowance.
    Measured(f64),
}

impl Speed {
    fn rate(self) -> f64 {
        match self {
            Self::Probed(rate) | Self::Measured(rate) => rate,
        }
    }
}

/// How much a cue matters, when a batch of updates has earned more than one.
///
/// The end of a run outranks anything inside it: that is the cue someone who
/// walked away is listening for, and it is the one that must not be dropped in
/// favour of the episode that happened to land just before it.
const CUE_EPISODE_DONE: u8 = 0;
const CUE_SOMETHING_FAILED: u8 = 1;
const CUE_RUN_ENDED: u8 = 2;

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

impl OutputKind {
    /// How a line of this colour is filed in `output.log` unless it says
    /// otherwise.
    ///
    /// Green is something that worked and red is something that didn't, both
    /// without exception. Grey is not an outcome at all — it is the colour of a
    /// line that is quieter than the rest, which covers a skipped episode but
    /// also "Stopping…" and where the logs are being kept. Those go down as
    /// notes, and the one caller that really does mean "skipped" says so.
    fn outcome(self) -> logs::Outcome {
        match self {
            OutputKind::Good => logs::Outcome::Done,
            OutputKind::Bad => logs::Outcome::Failed,
            OutputKind::Plain | OutputKind::Muted => logs::Outcome::Note,
        }
    }
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
    /// Light, dark, or whatever the operating system is set to. Held here as
    /// well as in egui's own options so the Settings tab has something to bind
    /// the buttons to; [`PodBatchApp::appearance`] is what keeps the two the
    /// same.
    theme: egui::ThemePreference,

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
    /// The engine holds between reading the feeds and downloading them; this is
    /// what releases it, once the user has agreed to the job.
    proceed: Option<Proceed>,
    rx: Option<UnboundedReceiver<Update>>,
    notify: Arc<dyn Fn() + Send + Sync>,

    /// The question on screen, if there is one.
    dialog: Option<Dialog>,
    /// A plan that arrived while another question was up, waiting its turn.
    pending_plan: Option<Plan>,
    /// Set the moment a dialog opens, so its safe answer can be given the
    /// keyboard focus on that frame and left alone on every frame after — a
    /// dialog that grabs focus back every frame cannot be tabbed away from.
    focus_dialog: bool,

    /// When the downloads themselves began, and the rate the last run actually
    /// managed. A rate measured by moving real episodes beats the one guessed
    /// from reading the feeds, so once there is one it is what the estimate uses.
    downloads_began: Option<Instant>,
    measured_rate: Option<f64>,
    /// When the last cue was played, for [`CUE_GAP`].
    last_cue: Option<Instant>,

    // ---- Transcripts ------------------------------------------------------
    audio_dir: Option<PathBuf>,
    audio: Vec<AudioRow>,
    /// What the machine was found to have, last time we looked. Surveyed on
    /// demand rather than every frame: it shells out to look for binaries, and
    /// doing that sixty times a second would be absurd.
    tools: Vec<tools::Found>,
    /// Whether to spend the extra minutes keeping a speaker's number attached
    /// to the same person. See the note in [`transcribe`].
    label_speakers: bool,
    skip_transcribed: bool,
    /// What a transcript is written as, and how big and bold. Kept here rather
    /// than in the engine's settings so the choice survives between runs.
    transcript_format: document::Format,
    transcript_style: document::Style,

    transcribing: bool,
    transcribe_cancel: Option<Cancel>,
    transcribe_rx: Option<UnboundedReceiver<transcribe::Update>>,

    /// The install running now, if any, and what it has said. Only one at a
    /// time — they are all package-manager operations and running two at once
    /// is how a lock file gets fought over.
    installing: Option<tools::Tool>,
    install_rx: Option<UnboundedReceiver<InstallUpdate>>,
    install_progress: Option<(u64, Option<u64>)>,
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
            theme: egui::ThemePreference::System,
            feeds: Vec::new(),
            running_map: Vec::new(),
            output: Vec::new(),
            output_scroll_to: None,
            running: false,
            cancelling: false,
            outcome: None,
            cancel: None,
            proceed: None,
            rx: None,
            notify,
            dialog: None,
            pending_plan: None,
            focus_dialog: false,
            downloads_began: None,
            measured_rate: None,
            last_cue: None,
            audio_dir: None,
            audio: Vec::new(),
            // Empty rather than surveyed: `new` runs before the window is up,
            // and looking for six programs is not something to make the first
            // frame wait on. The tab surveys the first time it is opened.
            tools: Vec::new(),
            label_speakers: true,
            skip_transcribed: true,
            transcript_format: document::Format::default(),
            transcript_style: document::Style::default(),
            transcribing: false,
            transcribe_cancel: None,
            transcribe_rx: None,
            installing: None,
            install_rx: None,
            install_progress: None,
        };
        cc.egui_ctx.set_theme(app.theme);

        // Said once, at the top of the box: a log nobody can find is no better
        // than one that was never written.
        match logs::status() {
            Ok(dir) => app.say(
                OutputKind::Muted,
                format!("Logging to {}", dir.display()),
            ),
            Err(e) => app.say(
                OutputKind::Bad,
                format!("No log files this time — {e}"),
            ),
        }

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

    /// Put a line in the output box — and, since this is every line the user is
    /// ever shown, in `output.log` as well. The box scrolls back only so far
    /// and empties when the window closes; the file is the copy that keeps.
    fn say(&mut self, kind: OutputKind, text: String) {
        self.say_as(kind, kind.outcome(), text);
    }

    /// The same, for the line whose outcome its colour doesn't give away.
    fn say_as(&mut self, kind: OutputKind, outcome: logs::Outcome, text: String) {
        logs::record(outcome, &text);
        self.show(kind, text);
    }

    /// Put a line in the box without writing it to the logs.
    ///
    /// For the transcript as it comes in. `output.log` is a record of what the
    /// program did, and `debug.log` is the same story in more detail — neither
    /// is a place to keep the thing the program produced. Logging it there
    /// copies an entire episode's text into both files, drowns the handful of
    /// lines that say what actually happened, and leaves three copies of a
    /// transcript on disk where the user asked for one.
    fn show(&mut self, kind: OutputKind, text: String) {
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
        self.downloads_began = None;

        // The box is cleared for the new run; the debug log is where the
        // settings a run was given can still be read afterwards.
        logs::debug(format!(
            "run starting: {} podcast(s), into {}, {} at a time, limit {}, skip existing {}",
            subscriptions.len(),
            self.out_dir.display(),
            self.concurrency,
            match self.limit_enabled {
                true => self.limit.max(1).to_string(),
                false => "none".to_string(),
            },
            self.skip_existing
        ));

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Cancel::new();
        let proceed = Proceed::new();

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
            proceed.clone(),
            Arc::clone(&self.notify),
        );

        self.rx = Some(rx);
        self.cancel = Some(cancel);
        self.proceed = Some(proceed);
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
                if self.running && !self.cancelling && self.dialog.is_none() {
                    self.ask(Dialog::Stop);
                }
            }
            Action::Show(tab) => {
                self.tab = tab;
                // The tab is no use until it knows what the machine has, and
                // this is the first moment it is worth the cost of finding out.
                if tab == Tab::Transcripts && self.tools.is_empty() {
                    self.survey_tools();
                }
            }
            // Ticking is per-tab: the same chord means the podcasts on one and
            // the episodes on the other, so it always acts on the list in view.
            Action::SelectAll => match self.tab {
                Tab::Transcripts => {
                    if !self.transcribing {
                        self.audio.iter_mut().for_each(|a| a.selected = true);
                    }
                }
                _ => {
                    if !self.running {
                        self.feeds.iter_mut().for_each(|f| f.selected = true);
                    }
                }
            },
            Action::SelectNone => match self.tab {
                Tab::Transcripts => {
                    if !self.transcribing {
                        self.audio.iter_mut().for_each(|a| a.selected = false);
                    }
                }
                _ => {
                    if !self.running {
                        self.feeds.iter_mut().for_each(|f| f.selected = false);
                    }
                }
            },
            Action::ChooseAudioFolder => {
                if !self.transcribing
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_directory(existing_ancestor(
                            self.audio_dir.as_deref().unwrap_or(&self.out_dir),
                        ))
                        .pick_folder()
                {
                    self.tab = Tab::Transcripts;
                    if self.tools.is_empty() {
                        self.survey_tools();
                    }
                    self.load_audio_folder(dir);
                }
            }
            Action::Transcribe => {
                if !self.transcribing && self.dialog.is_none() {
                    self.tab = Tab::Transcripts;
                    self.ask_transcribe();
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
        // An open dialog has the floor. Escape belongs to it — the key that
        // asked the question must not also answer it — and the rest are actions
        // on a window the user is currently being asked about.
        if self.dialog.is_some() {
            return;
        }

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

    // ---- questions --------------------------------------------------------

    fn ask(&mut self, dialog: Dialog) {
        self.dialog = Some(dialog);
        self.focus_dialog = true;
    }

    /// Put the plan up as a question, or hold it if something else is being
    /// asked first.
    ///
    /// Stopping is reachable while the feeds are still being read, so the plan
    /// can arrive with "Stop downloading?" already on screen. Replacing it
    /// there would swap the buttons under a press already on its way to one of
    /// them — and the swap is from "Stop" to "Download", so an attempt to
    /// abort would start the whole run instead.
    fn put_plan(&mut self, plan: Plan) {
        match self.dialog {
            Some(_) => self.pending_plan = Some(plan),
            None => self.ask(Dialog::Confirm(plan)),
        }
    }

    /// The user said yes.
    fn agreed(&mut self, dialog: Dialog) {
        logs::debug(format!("answered yes to {}", dialog.describe()));
        match dialog {
            Dialog::Confirm(_) => {
                if let Some(proceed) = &self.proceed {
                    proceed.go();
                }
                self.downloads_began = Some(Instant::now());
            }
            Dialog::Stop => {
                // Nothing left to confirm: the run they were asked about is the
                // one they just stopped.
                self.pending_plan = None;
                self.stop();
            }
            // "Yes" means different things to the three kinds of install: run
            // it, open the page that explains it, or open a terminal to run it
            // in. Only the first actually changes the machine from in here.
            Dialog::Install { tool, install } => match install {
                tools::Install::Guided { .. } => {
                    if let Err(e) = tools::open_terminal() {
                        self.say(OutputKind::Bad, e);
                    }
                }
                tools::Install::Manual { ref url, .. } => {
                    if let Err(e) = tools::open_url(url) {
                        self.say(OutputKind::Bad, e);
                    }
                }
                _ => self.start_install(tool, install),
            },
            Dialog::Transcribe { .. } => self.start_transcribing(),
            Dialog::StopTranscribing => self.stop_transcribing(),
        }
    }

    /// The user said no. Declining to start is declining the run, so the engine
    /// waiting behind the question is let go rather than left holding it.
    fn declined(&mut self, dialog: Dialog) {
        logs::debug(format!("answered no to {}", dialog.describe()));
        match dialog {
            Dialog::Confirm(_) => self.stop(),
            // Carrying on: if the plan arrived while this was up, it is now
            // the question that needs asking.
            Dialog::Stop => {
                if let Some(plan) = self.pending_plan.take() {
                    self.ask(Dialog::Confirm(plan));
                }
            }
            // Saying no to any of these is simply not doing it. Nothing is
            // waiting on the answer, so there is nothing to release.
            Dialog::Install { .. } | Dialog::Transcribe { .. } | Dialog::StopTranscribing => {}
        }
    }

    /// Draw whichever question is open, and act on the answer.
    ///
    /// The dialog is taken out of `self` for the duration so the closure can
    /// have the whole app, and put back only if it went unanswered.
    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };

        // A modal per kind of question. Sharing one id would hand a press
        // registered on the box that was up to whichever box replaced it, since
        // egui tracks a widget by where it sits and what order it was added in
        // — and both of these have a button in the same place.
        let (headline, lines, yes, no, id) = match &dialog {
            Dialog::Confirm(plan) => (
                plan_headline(plan),
                plan_lines(
                    plan,
                    self.measured_rate
                        .map(Speed::Measured)
                        .or(plan.probed_rate.map(Speed::Probed)),
                    self.concurrency,
                ),
                "▶ Download",
                "Cancel",
                "confirm-download",
            ),
            Dialog::Stop => (
                "Stop downloading?".to_string(),
                vec![
                    "The episodes part way through are thrown away, so nothing half-finished \
                     is left on the disk."
                        .to_string(),
                    "Every episode that has already landed stays where it is, and starting \
                     again picks up from there."
                        .to_string(),
                ],
                "■ Stop",
                "Keep downloading",
                "confirm-stop",
            ),
            Dialog::Install { tool, install } => install_question(*tool, install),
            Dialog::Transcribe { files, labelling } => (
                format!("Transcribe {}?", count(*files, "episode")),
                vec![
                    "Transcribing listens to the whole episode, so it takes a few minutes \
                     of the machine's full attention per file."
                        .to_string(),
                    match labelling {
                        true => "Speakers will be followed through each episode, so a number \
                                 should mean the same person throughout. It is worked out from \
                                 the words rather than the voices, so check anything that \
                                 matters."
                            .to_string(),
                        false => "Speakers will alternate between Speaker 1 and Speaker 2. \
                                  That is right for two people talking, and wrong for three \
                                  or more — telling those apart is what the Ollama step is \
                                  for."
                            .to_string(),
                    },
                    "Each transcript is written as a .txt file beside its audio.".to_string(),
                ],
                "▶ Transcribe",
                "Cancel",
                "confirm-transcribe",
            ),
            Dialog::StopTranscribing => (
                "Stop transcribing?".to_string(),
                vec![
                    "The episode part way through will be left without a transcript.".to_string(),
                    "The ones already written stay where they are.".to_string(),
                ],
                "■ Stop",
                "Keep transcribing",
                "confirm-stop-transcribing",
            ),
        };

        // A command the user has to run themselves is shown in full, and can be
        // copied — retyping a `curl … | bash` line by hand is how people end up
        // running something subtly different from what they were shown.
        let command = match &dialog {
            Dialog::Install { install, .. } => match install {
                tools::Install::Guided { command, .. } => Some(command.clone()),
                tools::Install::Run { display, .. } => Some(display.clone()),
                _ => None,
            },
            _ => None,
        };

        // True only on the frame the box appears, which is also the frame a
        // keypress from before it existed would still be sitting in the queue.
        let first_frame = self.focus_dialog;

        let mut answer = None;
        egui::Modal::new(egui::Id::new(id)).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(RichText::new(headline).strong().size(18.0));
            ui.add_space(8.0);
            for line in &lines {
                ui.label(line);
                ui.add_space(2.0);
            }

            if let Some(command) = &command {
                ui.add_space(8.0);
                egui::Frame::default()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(8.0)
                    .corner_radius(4.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(command).monospace()).wrap(),
                        );
                    });
                if ui.button("Copy command").clicked() {
                    ui.ctx().copy_text(command.clone());
                }
            }

            ui.add_space(14.0);

            ui.horizontal(|ui| {
                let confirm = ui.add_sized(
                    egui::vec2(150.0, CONTROL_HEIGHT),
                    egui::Button::new(yes),
                );
                let cancel = ui.add_sized(
                    egui::vec2(170.0, CONTROL_HEIGHT),
                    egui::Button::new(no),
                );

                // The harmless answer is the one holding focus when the box
                // appears, so a stray Return does the thing that can be undone.
                if self.focus_dialog {
                    cancel.request_focus();
                    self.focus_dialog = false;
                }
                if confirm.clicked() {
                    answer = Some(true);
                }
                if cancel.clicked() {
                    answer = Some(false);
                }
            });
        });

        // Escape dismisses, which is the same as saying no — but not on the
        // frame the box went up. A plan can arrive from the engine in the same
        // frame as a keypress, and a question answered by a key pressed before
        // it was asked is not a question at all: it would flash by unread and
        // cancel the run on a press that was meant for whatever came before.
        if !first_frame && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            answer = Some(false);
        }

        match answer {
            Some(true) => self.agreed(dialog),
            Some(false) => self.declined(dialog),
            None => self.dialog = Some(dialog),
        }
    }

    /// Drain everything the engine has said since the last frame.
    fn drain(&mut self) {
        let Some(rx) = self.rx.as_mut() else { return };

        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }

        // The loudest thing this batch of updates deserves. Collected rather
        // than played as it goes, because a batch can hold a dozen finished
        // episodes and they are worth one sound between them.
        let mut cue: Option<(u8, Cue)> = None;
        let mut want = |rank: u8, sound: Cue| {
            if cue.is_none_or(|(existing, _)| rank >= existing) {
                cue = Some((rank, sound));
            }
        };

        for update in updates {
            match update {
                Update::FeedFolder { feed, name } => {
                    if let Some(row) = self.row_mut(feed) {
                        row.folder = name;
                    }
                }
                Update::FeedStatus { feed, status } => {
                    // A podcast that could not be read is a failure worth
                    // hearing about: nothing under it will download at all.
                    if matches!(status, FeedStatus::Failed(_)) {
                        want(CUE_SOMETHING_FAILED, Cue::Failure);
                    }
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
                    // Named by the episode throughout, with the file it landed
                    // in after it: the file name is a timestamp now, and on its
                    // own it says nothing about which episode just arrived.
                    let file = ep.file_name.clone();
                    let title = ep.title.clone();
                    let size = human_bytes(ep.done);
                    match status {
                        EpisodeStatus::Done => {
                            want(CUE_EPISODE_DONE, Cue::Success);
                            self.say(
                                OutputKind::Good,
                                format!("{MARK_DONE} {show} — {title} → {file} ({size})"),
                            );
                        }
                        EpisodeStatus::Skipped => self.say_as(
                            OutputKind::Muted,
                            logs::Outcome::Skipped,
                            format!("{MARK_SKIPPED} {show} — {title} → {file} (already downloaded)"),
                        ),
                        EpisodeStatus::Failed(e) => {
                            want(CUE_SOMETHING_FAILED, Cue::Failure);
                            self.say(
                                OutputKind::Bad,
                                format!("{MARK_FAILED} {show} — {title}: {e}"),
                            );
                        }
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
                Update::Problem(line) => self.say(OutputKind::Bad, line),
                Update::Planned(plan) => {
                    if plan.episodes == 0 {
                        // Nothing to fetch is nothing to ask about, and the
                        // engine doesn't wait for an answer in that case either.
                        if plan.skipped > 0 {
                            self.say(
                                OutputKind::Muted,
                                "Every episode is already downloaded — nothing to fetch."
                                    .into(),
                            );
                        }
                    } else {
                        self.say(OutputKind::Plain, plan_headline(&plan));
                        self.put_plan(plan);
                    }
                }
                Update::Finished { cancelled } => {
                    self.outcome = Some(if cancelled {
                        Outcome::Stopped
                    } else {
                        Outcome::Completed
                    });
                    // Stopping was asked for, so it isn't news; the cue is for
                    // the run that ended on its own while nobody was watching.
                    if !cancelled {
                        want(CUE_RUN_ENDED, cue_for(&self.totals()));
                    }
                    self.note_rate(cancelled);
                    self.running = false;
                    self.cancelling = false;
                    self.cancel = None;
                    self.proceed = None;
                    self.rx = None;
                    // A question about a run that has ended has nothing left to
                    // ask; a window closed under a modal that outlived it would
                    // be stuck behind it.
                    self.dialog = None;
                    self.pending_plan = None;
                }
            }
        }

        if let Some((rank, sound)) = cue {
            self.play(rank, sound);
        }
    }

    /// Play a cue, unless one was played too recently for a second to be heard
    /// as anything but noise.
    fn play(&mut self, rank: u8, cue: Cue) {
        if !self.play_sounds {
            return;
        }

        let now = Instant::now();
        if cue_is_due(rank, self.last_cue, now) {
            sound::play(cue);
            self.last_cue = Some(now);
        }
    }

    /// Remember how fast the run that just ended actually went, so the next
    /// run's estimate rests on a measurement rather than a guess.
    ///
    /// Only worth keeping from a run long enough and large enough to have
    /// measured anything: a couple of small files in a couple of seconds says
    /// more about the server's latency than about the line.
    ///
    /// And only from a run that finished. The bytes counted here are the ones
    /// that landed complete, while the clock covers everything that was in
    /// flight — so a run stopped with four large episodes most of the way down
    /// looks like a slow line when it was nothing of the sort, and the next
    /// estimate would inherit that.
    fn note_rate(&mut self, cancelled: bool) {
        let Some(began) = self.downloads_began.take() else {
            return;
        };
        if cancelled {
            return;
        }

        let seconds = began.elapsed().as_secs_f64();
        let bytes = self.totals().bytes;
        if seconds >= 2.0 && bytes >= 4 * 1024 * 1024 {
            self.measured_rate = Some(bytes as f64 / seconds);
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

    /// Three tabs, splitting the full width of the window between them.
    ///
    /// Full width rather than three small buttons in the corner: they are the
    /// only navigation the app has, so they are worth being unmissable and easy
    /// to hit, and a target that spans a third of the window is both.
    fn tab_bar(&mut self, ui: &mut Ui) {
        let tabs = [
            (Tab::Downloads, "Downloads"),
            (Tab::Transcripts, "Transcripts"),
            (Tab::Settings, "Settings"),
        ];
        let gaps = ui.spacing().item_spacing.x * (tabs.len() - 1) as f32;
        let width = ((ui.available_width() - gaps) / tabs.len() as f32).max(1.0);

        ui.horizontal(|ui| {
            for (tab, label) in tabs {
                let button = egui::Button::selectable(self.tab == tab, label);
                if ui
                    .add_sized(egui::vec2(width, CONTROL_HEIGHT), button)
                    .clicked()
                {
                    self.perform(Action::Show(tab));
                }
            }
        });
    }

    /// The top of the Downloads tab: where the subscription list comes from, and
    /// the button that starts the run.
    ///
    /// Choosing the file and starting the run sit together on the first line,
    /// in the order they are done in. The file's own path goes underneath with
    /// the whole width to itself, because it is the one thing here whose length
    /// is not ours to choose.
    fn opml_bar(&mut self, ui: &mut Ui) {
        let busy = self.running;

        ui.horizontal(|ui| {
            ui.label(RichText::new("Subscription list").strong());

            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("📂 Choose OPML file…")
                        .min_size(egui::vec2(190.0, CONTROL_HEIGHT)),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Subscription list", &["opml", "xml"])
                    .pick_file()
            {
                self.load_opml(path);
            }

            let ready = self.selected_count() > 0;
            if self.running {
                let label = if self.cancelling { "Stopping…" } else { "■ Stop" };
                if ui
                    .add_enabled(
                        !self.cancelling && self.dialog.is_none(),
                        egui::Button::new(label).min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    self.ask(Dialog::Stop);
                }
            } else {
                let response = ui.add_enabled(
                    ready,
                    egui::Button::new("▶ Download episodes")
                        .min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
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

        ui.horizontal(|ui| {
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
            } else if matches!(self.dialog, Some(Dialog::Confirm(_))) {
                "Feeds read — waiting for you".to_string()
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

    // ---- transcripts ------------------------------------------------------

    /// Look the machine over and remember what it found.
    ///
    /// Called when the tab is first opened, after any install, and whenever the
    /// user presses Check again — never per frame. It shells out looking for
    /// half a dozen binaries and asks Ollama what it has, which is far too much
    /// to do at sixty frames a second.
    fn survey_tools(&mut self) {
        self.tools = tools::survey();
        let missing: Vec<&str> = self
            .tools
            .iter()
            .filter(|f| !f.presence.is_ready() && !f.tool.optional())
            .map(|f| f.tool.label())
            .collect();
        logs::debug(format!(
            "transcription tools: {} missing{}",
            missing.len(),
            match missing.is_empty() {
                true => String::new(),
                false => format!(" ({})", missing.join(", ")),
            }
        ));
    }

    fn tool_ready(&self, tool: tools::Tool) -> bool {
        self.tools
            .iter()
            .any(|f| f.tool == tool && f.presence.is_ready())
    }

    fn tool_path(&self, tool: tools::Tool) -> Option<PathBuf> {
        self.tools
            .iter()
            .find(|f| f.tool == tool)
            .and_then(|f| f.path.clone())
    }

    /// Everything a transcript needs, present. The two Ollama entries are not
    /// counted — a run without them is a run with blunter speaker numbers, not
    /// a run that cannot happen.
    fn can_transcribe(&self) -> bool {
        [
            tools::Tool::Ffmpeg,
            tools::Tool::Whisper,
            tools::Tool::WhisperModel,
        ]
        .iter()
        .all(|t| self.tool_ready(*t))
    }

    /// Read a folder and list the audio in it.
    fn load_audio_folder(&mut self, dir: PathBuf) {
        let files = transcribe::audio_in(&dir);
        logs::debug(format!(
            "scanned {} — {} audio file(s)",
            dir.display(),
            files.len()
        ));

        self.audio = files
            .into_iter()
            .map(|path| {
                let transcribed =
                    transcribe::transcript_path(&path, self.transcript_format).is_file();
                AudioRow {
                    name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    // Anything already transcribed starts unticked: the common
                    // case for re-opening a folder is "do the new ones", and
                    // re-transcribing an episode costs minutes for a file that
                    // is already sitting there.
                    selected: !transcribed,
                    status: match transcribed {
                        true => transcribe::Status::Done,
                        false => transcribe::Status::Pending,
                    },
                    fraction: 0.0,
                    transcribed,
                    path,
                }
            })
            .collect();

        if self.audio.is_empty() {
            self.say(
                OutputKind::Muted,
                format!("No audio files in {}", elide_path(&dir)),
            );
        } else {
            self.say(
                OutputKind::Plain,
                format!(
                    "{} in {}",
                    count(self.audio.len(), "audio file"),
                    elide_path(&dir)
                ),
            );
        }
        self.audio_dir = Some(dir);
    }

    fn ticked_audio(&self) -> Vec<PathBuf> {
        self.audio
            .iter()
            .filter(|a| a.selected)
            .map(|a| a.path.clone())
            .collect()
    }

    /// Ask before transcribing, for the same reason a download asks: this is
    /// minutes per episode of the machine's full attention.
    fn ask_transcribe(&mut self) {
        let files = self.ticked_audio().len();
        if files == 0 || self.transcribing || !self.can_transcribe() {
            return;
        }
        let labelling = self.label_speakers && self.tool_ready(tools::Tool::OllamaModel);
        self.ask(Dialog::Transcribe { files, labelling });
    }

    fn start_transcribing(&mut self) {
        let files = self.ticked_audio();
        let (Some(ffmpeg), Some(whisper), Some(model)) = (
            self.tool_path(tools::Tool::Ffmpeg),
            self.tool_path(tools::Tool::Whisper),
            self.tool_path(tools::Tool::WhisperModel),
        ) else {
            self.say(
                OutputKind::Bad,
                "Something needed for transcribing has gone missing — press Check again.".into(),
            );
            return;
        };

        // Clear the marks from any previous run so the list reports this one
        // rather than a mixture of the two.
        for row in self.audio.iter_mut().filter(|a| a.selected) {
            row.status = transcribe::Status::Pending;
            row.fraction = 0.0;
        }

        let labelling = self.label_speakers && self.tool_ready(tools::Tool::OllamaModel);
        self.say(
            OutputKind::Plain,
            format!(
                "Transcribing {}{}",
                count(files.len(), "file"),
                match labelling {
                    true => ", following speakers through each episode",
                    false => ", alternating between two speakers",
                }
            ),
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Cancel::new();
        transcribe::spawn(
            transcribe::Settings {
                files,
                ffmpeg,
                whisper,
                model,
                label_speakers: labelling,
                skip_existing: self.skip_transcribed,
                format: self.transcript_format,
                style: self.transcript_style,
            },
            tx,
            cancel.clone(),
            Arc::clone(&self.notify),
        );

        self.transcribe_rx = Some(rx);
        self.transcribe_cancel = Some(cancel);
        self.transcribing = true;
    }

    fn stop_transcribing(&mut self) {
        if let Some(cancel) = &self.transcribe_cancel {
            cancel.cancel();
            self.say(
                OutputKind::Muted,
                "Stopping — finishing the episode in hand…".into(),
            );
        }
    }

    /// Begin an install the user has agreed to, on a thread of its own.
    fn start_install(&mut self, tool: tools::Tool, install: tools::Install) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let notify = Arc::clone(&self.notify);

        // Guided and Manual never reach here — they are not things we run, and
        // the dialog for them offers a terminal rather than a Yes.
        let job = match install {
            tools::Install::Run { program, args, display } => {
                self.say(OutputKind::Plain, format!("Running: {display}"));
                Some(Ok((program, args)))
            }
            tools::Install::Download { url, to, bytes } => {
                self.say(
                    OutputKind::Plain,
                    format!("Downloading {} ({})", tool.label(), human_bytes(bytes)),
                );
                Some(Err((url, to)))
            }
            tools::Install::Guided { .. } | tools::Install::Manual { .. } => None,
        };
        let Some(job) = job else { return };

        let cancel = Cancel::new();
        self.transcribe_cancel = Some(cancel.clone());

        std::thread::Builder::new()
            .name("podbatch-install".into())
            .spawn(move || {
                let send = |u: InstallUpdate| {
                    let _ = tx.send(u);
                    notify();
                };
                let result = match job {
                    Ok((program, args)) => {
                        let send_line = |line: String| send(InstallUpdate::Line(line));
                        tools::install(&program, &args, send_line)
                    }
                    Err((url, to)) => tools::download(
                        &url,
                        &to,
                        |done, total| send(InstallUpdate::Progress { done, total }),
                        || cancel.is_cancelled(),
                    ),
                };
                send(InstallUpdate::Finished(result));
            })
            .expect("spawn install thread");

        self.installing = Some(tool);
        self.install_rx = Some(rx);
        self.install_progress = None;
    }

    /// Drain the installer thread.
    fn drain_install(&mut self) {
        let Some(rx) = self.install_rx.as_mut() else {
            return;
        };
        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }

        for update in updates {
            match update {
                InstallUpdate::Line(line) => {
                    // Package managers are chatty and most of it is noise; the
                    // debug log takes the lot and the window takes the lines
                    // that say something happened.
                    logs::debug(format!("install: {line}"));
                    let interesting = ["Error", "error:", "Warning", "==>", "Pouring", "🍺"];
                    if interesting.iter().any(|m| line.contains(m)) {
                        let kind = match line.contains("rror") {
                            true => OutputKind::Bad,
                            false => OutputKind::Muted,
                        };
                        self.say(kind, line);
                    }
                }
                InstallUpdate::Progress { done, total } => {
                    self.install_progress = Some((done, total));
                }
                InstallUpdate::Finished(result) => {
                    let tool = self.installing.take();
                    self.install_rx = None;
                    self.install_progress = None;
                    self.transcribe_cancel = None;

                    let label = tool.map(|t| t.label()).unwrap_or("It");
                    match result {
                        Ok(()) => {
                            self.say(OutputKind::Good, format!("{label} is installed."));
                            // Installing Ollama does not start it, and a server
                            // that isn't running looks exactly like one that
                            // isn't installed. Start it before re-surveying so
                            // the tab doesn't report a fresh install as missing.
                            if tool == Some(tools::Tool::Ollama) {
                                tools::start_ollama().ok();
                                std::thread::sleep(Duration::from_millis(600));
                            }
                        }
                        Err(e) => self.say(OutputKind::Bad, format!("{label} — {e}")),
                    }
                    self.survey_tools();
                    self.play(CUE_RUN_ENDED, Cue::Success);
                }
            }
        }
    }

    /// Drain the transcription engine.
    fn drain_transcribe(&mut self) {
        let Some(rx) = self.transcribe_rx.as_mut() else {
            return;
        };
        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }

        let mut cue: Option<(u8, Cue)> = None;
        let mut best = |rank: u8, c: Cue| {
            if cue.is_none_or(|(existing, _)| rank >= existing) {
                cue = Some((rank, c));
            }
        };

        for update in updates {
            match update {
                transcribe::Update::Status { file, status } => {
                    if let Some(row) = self.audio.get_mut(file) {
                        if matches!(status, transcribe::Status::Done) {
                            row.transcribed = true;
                            row.fraction = 1.0;
                        }
                        row.status = status.clone();
                    }
                    match status {
                        transcribe::Status::Done => best(CUE_EPISODE_DONE, Cue::Success),
                        transcribe::Status::Failed(_) => best(CUE_SOMETHING_FAILED, Cue::Failure),
                        _ => {}
                    }
                }
                transcribe::Update::Progress { file, fraction } => {
                    if let Some(row) = self.audio.get_mut(file) {
                        row.fraction = fraction;
                    }
                }
                // The transcript as it appears. Shown, not logged: it is the
                // thing being produced rather than a report on how it went.
                // Muted, because colouring it like an outcome would drown the
                // outcomes it sits between.
                transcribe::Update::Line { text } => self.show(OutputKind::Muted, text),
                transcribe::Update::Log(text) => self.say(OutputKind::Plain, text),
                transcribe::Update::Problem(text) => self.say(OutputKind::Bad, text),
                transcribe::Update::Finished { cancelled } => {
                    self.transcribing = false;
                    self.transcribe_rx = None;
                    self.transcribe_cancel = None;
                    let done = self
                        .audio
                        .iter()
                        .filter(|a| matches!(a.status, transcribe::Status::Done))
                        .count();
                    self.say(
                        match cancelled {
                            true => OutputKind::Muted,
                            false => OutputKind::Good,
                        },
                        match cancelled {
                            true => format!("Stopped — {} transcribed", count(done, "file")),
                            false => format!("Finished — {} transcribed", count(done, "file")),
                        },
                    );
                    best(CUE_RUN_ENDED, Cue::Success);
                }
            }
        }

        if let Some((rank, cue)) = cue {
            self.play(rank, cue);
        }
    }

    /// The top of the Transcripts tab: the folder, and the button that starts.
    fn transcripts_bar(&mut self, ui: &mut Ui) {
        let busy = self.transcribing || self.installing.is_some();

        ui.horizontal(|ui| {
            ui.label(RichText::new("Episodes").strong());

            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new("📂 Choose folder…")
                        .min_size(egui::vec2(190.0, CONTROL_HEIGHT)),
                )
                .clicked()
            {
                self.perform(Action::ChooseAudioFolder);
            }

            let ready = !self.ticked_audio().is_empty() && self.can_transcribe();
            if self.transcribing {
                if ui
                    .add_enabled(
                        self.dialog.is_none(),
                        egui::Button::new("■ Stop").min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    self.ask(Dialog::StopTranscribing);
                }
            } else {
                let response = ui.add_enabled(
                    ready && !busy,
                    egui::Button::new("▶ Transcribe")
                        .min_size(egui::vec2(200.0, CONTROL_HEIGHT)),
                );
                if !self.can_transcribe() {
                    response.on_hover_text(
                        "Install what's needed first — the list is under the episodes.",
                    );
                } else if !ready {
                    response.on_hover_text("Choose a folder and tick at least one episode.");
                } else if response.clicked() {
                    self.ask_transcribe();
                }
            }

            let muted = theme::palette(ui.visuals()).muted;
            if !self.audio.is_empty() {
                let ticked = self.audio.iter().filter(|a| a.selected).count();
                ui.label(
                    RichText::new(format!("{ticked} of {} selected", self.audio.len()))
                        .color(muted),
                );
            }
        });

        ui.horizontal(|ui| {
            let muted = theme::palette(ui.visuals()).muted;
            match &self.audio_dir {
                Some(dir) => {
                    ui.add(egui::Label::new(RichText::new(elide_path(dir)).color(muted)).truncate())
                        .on_hover_text(dir.display().to_string());
                }
                None => {
                    ui.label(
                        RichText::new("No folder chosen — transcripts are written beside the audio")
                            .color(muted),
                    );
                }
            }
        });
    }

    /// The left half of the Transcripts tab: the episodes, and what is needed
    /// to transcribe them.
    fn audio_pane(&mut self, ui: &mut Ui) {
        let busy = self.transcribing;
        let palette = theme::palette(ui.visuals());

        ui.horizontal(|ui| {
            ui.label(RichText::new("Audio files").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!busy && !self.audio.is_empty(), egui::Button::new("None"))
                    .clicked()
                {
                    self.audio.iter_mut().for_each(|a| a.selected = false);
                }
                if ui
                    .add_enabled(!busy && !self.audio.is_empty(), egui::Button::new("All"))
                    .clicked()
                {
                    self.audio.iter_mut().for_each(|a| a.selected = true);
                }
            });
        });
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() * 0.55)
            .show(ui, |ui| {
                if self.audio.is_empty() {
                    ui.label(
                        RichText::new("Choose a folder of episodes above.").color(palette.muted),
                    );
                }
                for row in &mut self.audio {
                    ui.horizontal(|ui| {
                        ui.add_enabled(!busy, egui::Checkbox::new(&mut row.selected, ""));
                        ui.add(
                            egui::Label::new(&row.name)
                                .truncate(),
                        )
                        .on_hover_text(row.path.display().to_string());

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let (text, colour) = audio_status(row, &palette);
                                if !text.is_empty() {
                                    ui.label(RichText::new(text).color(colour).small());
                                }
                            },
                        );
                    });
                }
            });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        self.tools_pane(ui);
    }

    /// What transcribing needs, what is present, and the one button that puts
    /// each missing piece right — after asking.
    fn tools_pane(&mut self, ui: &mut Ui) {
        let palette = theme::palette(ui.visuals());
        let busy = self.transcribing || self.installing.is_some();

        ui.horizontal(|ui| {
            ui.label(RichText::new("What this needs").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("Check again"))
                    .on_hover_text("Look again for the programs below.")
                    .clicked()
                {
                    self.survey_tools();
                }
            });
        });
        ui.add_space(4.0);

        // Collected first: the loop below wants `&mut self` to open a dialog,
        // which it cannot do while iterating `self.tools`.
        let found: Vec<tools::Found> = self.tools.clone();
        let manager = self
            .tools
            .iter()
            .find(|f| f.tool == tools::Tool::PackageManager)
            .map(|f| f.presence.is_ready())
            .unwrap_or(false);
        let mut wanted: Option<tools::Tool> = None;

        egui::ScrollArea::vertical()
            .id_salt("tools")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for entry in &found {
                    // A package manager that is present is scaffolding, not a
                    // requirement — saying "Homebrew ✔" every time adds a line
                    // that never needs acting on.
                    if entry.tool == tools::Tool::PackageManager && entry.presence.is_ready() {
                        continue;
                    }

                    ui.horizontal(|ui| {
                        let (mark, colour) = match &entry.presence {
                            tools::Presence::Ready(_) => (MARK_DONE, palette.ok),
                            tools::Presence::Missing if entry.tool.optional() => {
                                (MARK_SKIPPED, palette.muted)
                            }
                            tools::Presence::Missing => (MARK_FAILED, palette.bad),
                        };
                        ui.label(RichText::new(mark).color(colour).strong());
                        ui.label(entry.tool.label());

                        if entry.tool.optional() {
                            ui.label(RichText::new("optional").color(palette.muted).small());
                        }

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| match &entry.presence {
                                tools::Presence::Ready(detail) => {
                                    ui.label(
                                        RichText::new(detail).color(palette.muted).small(),
                                    );
                                }
                                tools::Presence::Missing => {
                                    if ui
                                        .add_enabled(!busy, egui::Button::new("Install…"))
                                        .on_hover_text(
                                            "Shows exactly what will be run, and asks first.",
                                        )
                                        .clicked()
                                    {
                                        wanted = Some(entry.tool);
                                    }
                                }
                            },
                        );
                    });
                    ui.label(
                        RichText::new(entry.tool.why())
                            .color(palette.muted)
                            .small(),
                    );
                    ui.add_space(4.0);
                }

                if !manager && !found.is_empty() {
                    ui.label(
                        RichText::new(
                            "No package manager was found, so the pieces above have to be \
                             installed by hand. Press Install on one and it will show you how.",
                        )
                        .color(palette.warn)
                        .small(),
                    );
                }
            });

        if let Some(tool) = wanted {
            let install = tools::plan(tool, tools::manager());
            self.ask(Dialog::Install { tool, install });
        }
    }

    /// One bar across the bottom of the Transcripts tab, for whichever long job
    /// is running — an install, or the transcription itself.
    ///
    /// The two never overlap: installing is blocked while transcribing and the
    /// other way round, so one bar can serve both without ambiguity about which
    /// it is reporting.
    fn transcribe_progress(&mut self, ui: &mut Ui) {
        let muted = theme::palette(ui.visuals()).muted;

        if let Some(tool) = self.installing {
            let (fraction, detail) = match self.install_progress {
                Some((done, Some(total))) if total > 0 => (
                    done as f32 / total as f32,
                    format!("{} of {}", human_bytes(done), human_bytes(total)),
                ),
                Some((done, _)) => (0.0, human_bytes(done)),
                // A package manager gives no percentage, so the bar animates
                // rather than lying about how far along it is.
                None => (0.0, "working…".to_string()),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Installing {}", tool.label())).strong());
                ui.label(RichText::new(detail).color(muted));
            });
            if fraction > 0.0 {
                ui.add_sized(
                    egui::vec2(ui.available_width(), PROGRESS_HEIGHT),
                    egui::ProgressBar::new(fraction).show_percentage(),
                );
            } else {
                ui.add_sized(
                    egui::vec2(ui.available_width(), PROGRESS_HEIGHT),
                    egui::ProgressBar::new(0.0).animate(true),
                );
            }
            return;
        }

        if !self.transcribing {
            return;
        }

        let total = self.audio.iter().filter(|a| a.selected).count().max(1);
        let settled = self
            .audio
            .iter()
            .filter(|a| {
                matches!(
                    a.status,
                    transcribe::Status::Done
                        | transcribe::Status::Skipped
                        | transcribe::Status::Failed(_)
                )
            })
            .count();
        // The file in hand counts for the fraction of itself that is done, so
        // the bar keeps moving through a long episode rather than sitting still
        // for ten minutes and then jumping.
        let part: f32 = self
            .audio
            .iter()
            .filter(|a| matches!(a.status, transcribe::Status::Working(_)))
            .map(|a| a.fraction)
            .sum();

        let working = self
            .audio
            .iter()
            .find(|a| matches!(a.status, transcribe::Status::Working(_)));

        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{settled} of {total}")).strong());
            if let Some(row) = working {
                let stage = match &row.status {
                    transcribe::Status::Working(s) => s.label(),
                    _ => "",
                };
                ui.add(
                    egui::Label::new(RichText::new(format!("{} — {stage}", row.name)).color(muted))
                        .truncate(),
                );
            }
        });
        ui.add_sized(
            egui::vec2(ui.available_width(), PROGRESS_HEIGHT),
            egui::ProgressBar::new(((settled as f32 + part) / total as f32).clamp(0.0, 1.0))
                .show_percentage(),
        );
    }

    /// The right half: the same output box the Downloads tab uses, plus the
    /// two choices that change what a transcript comes out like.
    fn transcripts_options(&mut self, ui: &mut Ui) {
        let busy = self.transcribing;
        let muted = theme::palette(ui.visuals()).muted;

        ui.horizontal_wrapped(|ui| {
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(&mut self.skip_transcribed, "Skip ones already transcribed"),
            );
            ui.add_space(12.0);

            let has_ollama = self.tool_ready(tools::Tool::OllamaModel);
            ui.add_enabled(
                !busy && has_ollama,
                egui::Checkbox::new(&mut self.label_speakers, "Follow speakers through the episode"),
            )
            .on_hover_text(
                "Whisper hears when the voice changes but not whose it is, so on its own it \
                 numbers every turn afresh. With this on, Ollama reads the turns afterwards \
                 and works out which are the same person, so Speaker 1 stays Speaker 1. It \
                 adds a minute or two per episode and it can get it wrong.",
            );
            if !has_ollama {
                ui.label(
                    RichText::new("(needs Ollama)")
                        .color(muted)
                        .small(),
                );
            }
        });

        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Write it as");
            let before = self.transcript_format;
            egui::ComboBox::from_id_salt("transcript-format")
                .selected_text(self.transcript_format.label())
                .show_ui(ui, |ui| {
                    for format in document::Format::all() {
                        ui.selectable_value(
                            &mut self.transcript_format,
                            format,
                            format.label(),
                        );
                    }
                });

            ui.add_space(12.0);
            // Greyed out for plain text rather than hidden: a control that
            // vanishes leaves you wondering where the setting went, and the
            // answer — "that format cannot carry it" — is worth showing.
            let styled = self.transcript_format != document::Format::Text;
            ui.add_enabled_ui(!busy && styled, |ui| {
                ui.label("Size");
                ui.add(egui::DragValue::new(&mut self.transcript_style.size).range(8..=48))
                    .on_hover_text("Point size of the text in the file, not of this window.");
                ui.checkbox(&mut self.transcript_style.bold, "Bold");
            });

            // Changing the format changes the file name we look for, so what
            // counts as "already transcribed" changes with it.
            if before != self.transcript_format
                && let Some(dir) = self.audio_dir.clone()
            {
                self.load_audio_folder(dir);
            }
        });
        ui.label(
            RichText::new(self.transcript_format.note())
                .color(muted)
                .small(),
        );
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
        ui.checkbox(&mut self.play_sounds, "Sound as episodes land")
            .on_hover_text(
                "One cue as each episode finishes downloading and a different one when \
                 something fails, with a last cue when the whole run ends. Never more than \
                 one every second or so, however many finish at once.",
            );

        ui.add_space(12.0);
        self.appearance(ui);

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

    /// Light or dark, or neither — which is the default, and means the app is
    /// dark exactly when the machine is.
    ///
    /// Both palettes are built in [`theme`] and the whole window is drawn from
    /// them, so this is a single call to egui rather than anything the panes
    /// have to know about. The choice is offered anyway because the system
    /// setting is not always the one that suits the room: a dark app in a bright
    /// office is as hard to read as a white one at night, and a user who needs
    /// the contrast a particular way round should not have to change their whole
    /// machine to get it here.
    ///
    /// Three buttons rather than a dark-mode tick box, because "follow the
    /// system" is a real answer and a two-state toggle cannot express it once it
    /// has been left. They are ordinary buttons, so Tab reaches them and Space
    /// or Enter presses them like everything else in the window.
    fn appearance(&mut self, ui: &mut Ui) {
        use egui::ThemePreference as Pref;

        let muted = theme::palette(ui.visuals()).muted;

        ui.label(RichText::new("Appearance").strong());
        ui.horizontal(|ui| {
            for (preference, label, hint) in [
                (
                    Pref::System,
                    "System",
                    "Match whatever this computer is set to, and follow it when it changes.",
                ),
                (Pref::Light, "Light", "Dark text on a light background."),
                (Pref::Dark, "Dark", "Light text on a dark background."),
            ] {
                ui.selectable_value(&mut self.theme, preference, label)
                    .on_hover_text(hint);
            }

            // Handed on only when the two disagree, which is the frame a button
            // was pressed. Compared against what egui is actually holding rather
            // than against the value from the top of this function: that way the
            // preference gets through however it came to change, and the check
            // stays true whatever else has been at it.
            if ui.ctx().options(|options| options.theme_preference) != self.theme {
                ui.ctx().set_theme(self.theme);
                logs::debug(format!("theme set to {:?}", self.theme));
            }
        });
        ui.label(
            RichText::new(
                "Both looks are built to the same contrast standard, so nothing becomes \
                 harder to read either way round.",
            )
            .color(muted)
            .small(),
        );
    }
}

/// The question asked before anything is put on the machine.
///
/// Every branch names the thing, says what it is for, and — where there is one
/// — shows the exact command. The three kinds differ in what "yes" does, so
/// they differ in what the button says: a run we do, a page we open, or a
/// terminal we open for the user to do it themselves.
fn install_question(
    tool: tools::Tool,
    install: &tools::Install,
) -> (String, Vec<String>, &'static str, &'static str, &'static str) {
    let headline = format!("Install {}?", tool.label());
    match install {
        tools::Install::Run { .. } => (
            headline,
            vec![
                tool.why().to_string(),
                "This runs the command below on your machine.".to_string(),
                // `ollama pull` fetches a model rather than a program, and the
                // size of it is the part worth knowing before agreeing.
                match tool {
                    tools::Tool::OllamaModel => format!(
                        "It downloads about {}, once.",
                        human_bytes(tools::OLLAMA_MODEL_BYTES)
                    ),
                    _ => "The package manager will say what it is fetching.".to_string(),
                },
            ],
            "Install",
            "Cancel",
            "confirm-install",
        ),
        tools::Install::Download { bytes, to, .. } => (
            headline,
            vec![
                tool.why().to_string(),
                format!(
                    "This downloads {} to {}.",
                    human_bytes(*bytes),
                    elide_path(to)
                ),
                "It is fetched once and kept, so this only happens the first time."
                    .to_string(),
            ],
            "Download",
            "Cancel",
            "confirm-download-model",
        ),
        tools::Install::Guided { why, .. } => (
            headline,
            vec![
                why.clone(),
                "Nothing is installed by pressing this — it opens a terminal, and the \
                 command is yours to run or not."
                    .to_string(),
            ],
            "Open Terminal",
            "Close",
            "confirm-guided",
        ),
        tools::Install::Manual { why, url } => (
            headline,
            vec![why.clone(), format!("Instructions are at {url}")],
            "Open the page",
            "Close",
            "confirm-manual",
        ),
    }
}

/// The status an audio row reports on the right of its name.
fn audio_status(row: &AudioRow, palette: &theme::Palette) -> (String, egui::Color32) {
    match &row.status {
        transcribe::Status::Pending => match row.transcribed {
            true => ("transcribed".to_string(), palette.muted),
            false => (String::new(), palette.muted),
        },
        // The stage matters more than the percentage while converting, and the
        // percentage only means anything once Whisper is the one running.
        transcribe::Status::Working(stage) => match stage {
            transcribe::Stage::Transcribing => (
                format!("{:.0}%", row.fraction * 100.0),
                palette.accent,
            ),
            other => (other.label().to_string(), palette.accent),
        },
        transcribe::Status::Done => ("done".to_string(), palette.ok),
        transcribe::Status::Skipped => ("skipped".to_string(), palette.muted),
        transcribe::Status::Failed(e) => (e.clone(), palette.bad),
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

/// Whether a cue can be heard as a cue, given when the last one played.
///
/// The end of a run is never held back. That is the sound someone who walked
/// away is listening for, and dropping it because an episode happened to land
/// a moment earlier would lose the only one that had to be heard.
fn cue_is_due(rank: u8, last: Option<Instant>, now: Instant) -> bool {
    rank == CUE_RUN_ENDED || last.is_none_or(|last| now.duration_since(last) >= CUE_GAP)
}

/// The question at the top of the confirmation box.
fn plan_headline(plan: &Plan) -> String {
    format!("Download {}?", count(plan.episodes, "episode"))
}

/// What the confirmation box says underneath the question.
///
/// The point of the box is that "download the lot" can mean four minutes or
/// four hours, and until the feeds have been read nobody — the user or the app
/// — can tell which. So it says how much, how fast the line looks, and how long
/// that comes to, and it says which of those it is unsure about: an estimate
/// that hides its own footing is worse than none.
fn plan_lines(plan: &Plan, speed: Option<Speed>, concurrency: usize) -> Vec<String> {
    let mut lines = Vec::new();

    let sized = plan.episodes.saturating_sub(plan.unsized_episodes);
    match (sized, plan.unsized_episodes) {
        (0, _) => lines.push(
            "None of them says how big it is, so there is no telling how much there is to \
             fetch."
                .to_string(),
        ),
        (_, 0) => lines.push(format!("{} to fetch.", human_bytes(plan.bytes))),
        (_, unsized_episodes) => lines.push(format!(
            "{} to fetch, plus {} that don't say how big they are.",
            human_bytes(plan.bytes),
            count(unsized_episodes, "episode")
        )),
    }

    match speed.and_then(|speed| transfer_seconds(plan.bytes, speed.rate()).map(|s| (speed, s))) {
        Some((speed, seconds)) if plan.bytes > 0 => {
            // "less than a minute" is already a bound and takes no "about" in
            // front of it; a figure in minutes takes one, and takes "at least"
            // instead when episodes are missing from the sum it was made from.
            let estimate = human_duration(seconds);
            let sentence = match (seconds < 60.0, plan.unsized_episodes > 0) {
                (true, false) => format!("that is {estimate}"),
                (true, true) => format!("that is {estimate} for the ones that say"),
                (false, false) => format!("that is about {estimate}"),
                (false, true) => format!("that is at least {estimate}"),
            };

            // A probe is one connection's worth. Several run at once, so the
            // figure it gives is the slow end of what to expect, and saying so
            // is the difference between an estimate and a promise.
            let per = match speed {
                Speed::Probed(_) if concurrency > 1 => " on one connection",
                _ => "",
            };
            let allowance = match speed {
                Speed::Probed(_) if concurrency > 1 => {
                    format!(" — likely quicker, with {concurrency} downloading at once")
                }
                _ => String::new(),
            };

            lines.push(format!(
                "At around {}{per}, {sentence}{allowance}.",
                human_rate(speed.rate())
            ));
        }
        // Either nothing measurable came back from reading the feeds, or there
        // is no declared size to divide by. Saying so is the honest answer.
        _ => lines.push(
            "How long that takes is anyone's guess — there was nothing to measure your \
             connection against."
                .to_string(),
        ),
    }

    if plan.skipped > 0 {
        lines.push(format!(
            "{} already downloaded, and will be left alone.",
            count(plan.skipped, "episode")
        ));
    }

    lines
}

/// `1 episode`, `4 episodes`.
fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
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
        self.paint(ui);
    }
}

impl PodBatchApp {
    /// The whole window, minus the one argument that cannot be built in a test.
    ///
    /// Split out from [`eframe::App::ui`] so the tests can paint a real frame:
    /// `eframe::Frame` has no public constructor, and this function never used
    /// it. Everything the window draws goes through here.
    fn paint(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.drain();
        self.drain_transcribe();
        self.drain_install();
        self.accept_dropped_files(&ctx);
        self.handle_shortcuts(&ctx);

        // Transcribing reports once per segment, which on a quiet passage can
        // be several seconds apart; an install reports only when it feels like
        // it. Both need a steadier clock than that to animate against.
        if self.transcribing || self.installing.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // The engine wakes us on every message, but a download reports at most
        // ten times a second and the spinner needs a steadier clock than that.
        // Not while the run is held at the confirmation box: nothing is moving
        // there, and there is nothing to animate.
        if self.running && !matches!(self.dialog, Some(Dialog::Confirm(_))) {
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
            // Laid out like the Downloads tab on purpose: the list of things to
            // be done on the left, what is happening to them on the right. The
            // two tabs do different work, but they are the same shape of job
            // and there is nothing to gain by making them look unrelated.
            Tab::Transcripts => {
                egui::Panel::top(egui::Id::new("transcripts-bar")).show(ui, |ui| {
                    ui.add_space(6.0);
                    self.transcripts_bar(ui);
                    ui.add_space(6.0);
                });

                egui::Panel::bottom(egui::Id::new("transcripts-options")).show(ui, |ui| {
                    ui.add_space(6.0);
                    self.transcripts_options(ui);
                    ui.add_space(4.0);
                    self.transcribe_progress(ui);
                    ui.add_space(6.0);
                });

                egui::Panel::left(egui::Id::new("audio"))
                    .resizable(true)
                    .default_size(380.0)
                    .min_size(260.0)
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        self.audio_pane(ui);
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

        // Last, so the question is drawn over the window it is about.
        self.dialogs(&ctx);
    }
}

impl FeedStatus {
    /// Whether this feed will produce no further episode list.
    fn is_settled(&self) -> bool {
        matches!(self, FeedStatus::Done | FeedStatus::Failed(_))
    }
}

/// `~/Podbatch/Downloads`, or the same under the current directory on the rare
/// system with no home.
///
/// Only the starting point: the folder is a setting, and one the user is free to
/// point anywhere they like — an external drive being the obvious case, since a
/// full subscription list is measured in gigabytes. The logs go to
/// `~/Podbatch/Logging` next door, so everything the app writes is under one
/// folder rather than scattered between the home directory and wherever the
/// platform keeps application data.
fn default_out_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Podbatch")
        .join("Downloads")
}

/// The nearest ancestor that exists, so the folder picker opens somewhere real
/// even before `~/Podbatch/Downloads` has been created.
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
        // `rest` still uses the platform's own separator, so building the
        // rest of the string with the same one is what keeps `~\Downloads`
        // from coming out as `~/Downloads\Sub` on Windows.
        return format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
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
            // The two dialogs, which write the same marks on their buttons.
            "▶ Download",
            "Keep downloading",
            "How long that takes is anyone's guess — there was nothing to measure your \
             connection against.",
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
            Action::Show(Tab::Transcripts),
            Action::Show(Tab::Settings),
            Action::SelectAll,
            Action::SelectNone,
            Action::ChooseAudioFolder,
            Action::Transcribe,
        ] {
            let count = actions.iter().filter(|&&a| a == expected).count();
            assert_eq!(count, 1, "{expected:?} should have exactly one shortcut");
        }
        assert_eq!(actions.len(), 10, "an action was added without a test");

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

    /// Escape is a key that gets pressed by accident, and it used to throw away
    /// however much of a run had not finished. Now it asks.
    #[test]
    fn stopping_asks_before_it_stops() {
        let mut app = test_app();
        app.feeds = vec![feed(true, vec![])];
        app.running = true;
        app.cancel = Some(Cancel::new());

        app.perform(Action::Stop);
        assert!(matches!(app.dialog, Some(Dialog::Stop)));
        assert!(!app.cancelling, "asking is not stopping");

        // Saying no leaves the run exactly as it was.
        let dialog = app.dialog.take().expect("dialog");
        app.declined(dialog);
        assert!(!app.cancelling);
        assert!(app.running);

        // Saying yes is what actually stops it.
        app.perform(Action::Stop);
        let dialog = app.dialog.take().expect("dialog");
        app.agreed(dialog);
        assert!(app.cancelling);
    }

    /// Turning down the confirmation has to release the engine, which is
    /// sitting on the question waiting to be told either way. Leaving it there
    /// would hang the run — and the window — for good.
    #[test]
    fn declining_the_confirmation_stops_the_run() {
        let mut app = test_app();
        app.running = true;
        app.cancel = Some(Cancel::new());
        app.proceed = Some(Proceed::new());

        app.declined(Dialog::Confirm(Plan::default()));
        assert!(app.cancelling, "the engine was left waiting on an answer");
    }

    #[test]
    fn confirming_lets_the_engine_go() {
        let mut app = test_app();
        app.running = true;
        app.proceed = Some(Proceed::new());

        app.agreed(Dialog::Confirm(Plan::default()));
        assert!(
            app.downloads_began.is_some(),
            "the clock the next estimate is built on starts here"
        );
    }

    /// What the box actually says. The numbers are the whole reason it exists,
    /// so each one has to survive the sentence it is put in.
    #[test]
    fn the_confirmation_says_how_much_and_how_long() {
        let plan = Plan {
            episodes: 40,
            bytes: 40 * 1024 * 1024,
            unsized_episodes: 0,
            skipped: 12,
            probed_rate: Some(1024.0 * 1024.0),
        };

        assert_eq!(plan_headline(&plan), "Download 40 episodes?");
        let lines = plan_lines(&plan, Some(Speed::Measured(1024.0 * 1024.0)), 4).join("\n");
        assert!(lines.contains("40.0 MB to fetch"), "{lines}");
        assert!(lines.contains("1.0 MB/s"), "{lines}");
        // 40 MB at 1 MB/s is 40 seconds, which is worded as its own bound —
        // "about less than a minute" is what a lazier join produces here.
        assert!(lines.contains("that is less than a minute."), "{lines}");
        assert!(lines.contains("12 episodes already downloaded"), "{lines}");

        // The same plan over a slower line, where the figure is a real one and
        // does take a hedge in front of it.
        let slow = plan_lines(&plan, Some(Speed::Measured(64.0 * 1024.0)), 4).join("\n");
        assert!(slow.contains("that is about 11 minutes."), "{slow}");
    }

    /// An estimate with nothing behind it is worse than no estimate: it is the
    /// number the user will plan their evening around.
    #[test]
    fn an_unmeasurable_run_says_so_rather_than_guessing() {
        let plan = Plan {
            episodes: 3,
            bytes: 0,
            unsized_episodes: 3,
            skipped: 0,
            probed_rate: None,
        };

        let lines = plan_lines(&plan, None, 4).join("\n");
        assert!(lines.contains("None of them says how big it is"), "{lines}");
        assert!(lines.contains("anyone's guess"), "{lines}");
        assert!(!lines.contains("already downloaded"), "{lines}");

        // A known size with no rate still declines to put a time on it.
        let sized = Plan { bytes: 5_000_000, unsized_episodes: 0, ..plan };
        let lines = plan_lines(&sized, None, 4).join("\n");
        assert!(lines.contains("4.8 MB to fetch"), "{lines}");
        assert!(lines.contains("anyone's guess"), "{lines}");
    }

    /// Episodes that don't declare a size are not in the byte count, so the
    /// estimate built from it is a floor and has to be worded as one.
    #[test]
    fn an_estimate_missing_some_sizes_is_given_as_a_minimum() {
        let plan = Plan {
            episodes: 10,
            bytes: 10 * 1024 * 1024,
            unsized_episodes: 4,
            skipped: 0,
            probed_rate: Some(1024.0),
        };

        let lines = plan_lines(&plan, plan.probed_rate.map(Speed::Probed), 1).join("\n");
        assert!(lines.contains("4 episodes that don't say how big they are"), "{lines}");
        assert!(lines.contains("at least"), "{lines}");
    }

    /// The confirmation drawn for real, in a context with the app's own fonts
    /// and theme, and answered the way an accidental Escape answers it.
    ///
    /// The two frames are the whole interaction: one that puts the box up, one
    /// that takes the key. Everything either side of it is unit-testable, but
    /// "does Escape reach the dialog rather than the run underneath it" is not
    /// a question the state alone can answer — the shortcut table has a claim on
    /// that key too, and only a real frame decides which of them gets it.
    #[test]
    fn the_confirmation_paints_and_escape_answers_it_rather_than_the_run() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let mut app = test_app();
        app.feeds = vec![feed(true, vec![])];
        app.running = true;
        app.cancel = Some(Cancel::new());
        app.proceed = Some(Proceed::new());
        app.ask(Dialog::Confirm(Plan {
            episodes: 40,
            bytes: 40 * 1024 * 1024,
            unsized_episodes: 0,
            skipped: 0,
            probed_rate: Some(1024.0 * 1024.0),
        }));

        // Two frames, in the order the real window runs them: fonts and the
        // modal's own size are both settled lazily, so the first frame of any
        // egui context is a warm-up and paints nothing worth reading.
        frame(&ctx, &mut app);
        let painted = frame(&ctx, &mut app);
        assert!(
            painted.contains("Download 40 episodes?"),
            "the question never reached the screen: {painted}"
        );
        assert!(painted.contains("40.0 MB to fetch"), "{painted}");
        assert!(painted.contains("1.0 MB/s"), "{painted}");

        // Then Escape, which the dialog must take before the shortcut table can
        // read it as "stop the run".
        escaping(&ctx, &mut app);

        assert!(app.dialog.is_none(), "Escape should have dismissed the box");
        assert!(
            app.cancelling,
            "declining to start the downloads has to let the waiting engine go"
        );
    }

    /// A key pressed before a question existed must not answer it.
    ///
    /// The plan arrives from the engine, in `drain`, part way through a frame —
    /// so a box can go up in the same frame as a keypress aimed at whatever was
    /// on screen a moment earlier. Answered by that press, the confirmation
    /// would appear and be gone inside one frame, cancelling a run the user
    /// never saw themselves being asked about.
    #[test]
    fn a_question_cannot_be_answered_by_a_key_pressed_before_it_was_asked() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let mut app = test_app();
        app.running = true;
        app.cancel = Some(Cancel::new());
        app.proceed = Some(Proceed::new());
        frame(&ctx, &mut app);

        // The engine's plan lands in the same frame the key is pressed.
        app.put_plan(Plan { episodes: 5, bytes: 500, ..Plan::default() });
        escaping(&ctx, &mut app);
        assert!(
            app.dialog.is_some(),
            "the box was answered by a press that predates it"
        );
        assert!(!app.cancelling, "and the run was cancelled by it");

        // Escape from here on does answer it, because now it has been seen.
        escaping(&ctx, &mut app);
        assert!(app.dialog.is_none());
        assert!(app.cancelling);
    }

    /// A question already on screen must not be swapped for another one.
    ///
    /// Stopping is reachable while the feeds are still being read, so "Stop
    /// downloading?" can be up when the plan arrives. Both boxes put a button
    /// in the same place, so replacing one with the other hands a press meant
    /// for "■ Stop" to "▶ Download" — the click that was trying to abort the
    /// run would start the whole thing instead.
    #[test]
    fn a_plan_arriving_mid_question_waits_its_turn() {
        let mut app = test_app();
        app.running = true;
        app.cancel = Some(Cancel::new());
        app.proceed = Some(Proceed::new());
        app.ask(Dialog::Stop);

        app.put_plan(Plan { episodes: 5, ..Plan::default() });
        assert!(
            matches!(app.dialog, Some(Dialog::Stop)),
            "the question on screen was swapped underneath the user"
        );
        assert!(app.pending_plan.is_some());

        // Carrying on brings the plan up as the next question.
        let dialog = app.dialog.take().expect("dialog");
        app.declined(dialog);
        assert!(matches!(app.dialog, Some(Dialog::Confirm(_))));
        assert!(app.pending_plan.is_none());
    }

    /// And stopping at that question drops the plan rather than asking about a
    /// run that no longer exists.
    #[test]
    fn stopping_discards_a_plan_that_was_waiting_behind_the_question() {
        let mut app = test_app();
        app.running = true;
        app.cancel = Some(Cancel::new());
        app.proceed = Some(Proceed::new());
        app.ask(Dialog::Stop);
        app.put_plan(Plan { episodes: 5, ..Plan::default() });

        let dialog = app.dialog.take().expect("dialog");
        app.agreed(dialog);
        assert!(app.cancelling);
        assert!(app.pending_plan.is_none(), "asked about a run that was stopped");
        assert!(app.dialog.is_none());
    }

    /// Run one frame the way the window does, and hand back every scrap of text
    /// that was actually painted.
    fn frame(ctx: &egui::Context, app: &mut PodBatchApp) -> String {
        frame_with(ctx, app, Default::default())
    }

    /// One frame with Escape pressed in it.
    fn escaping(ctx: &egui::Context, app: &mut PodBatchApp) -> String {
        frame_with(
            ctx,
            app,
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
        )
    }

    /// The whole window, painted. Two of these are needed before anything can
    /// be read off it — see the note in [`frame`].
    fn paint_frame(ctx: &egui::Context, app: &mut PodBatchApp) -> String {
        let mut output = ctx.run_ui(Default::default(), |ui| app.paint(ui));
        let mut text = String::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut text);
        }
        output.textures_delta.clear();
        text
    }

    fn frame_with(ctx: &egui::Context, app: &mut PodBatchApp, input: egui::RawInput) -> String {
        let mut output = ctx.run_ui(input, |_| {
            app.handle_shortcuts(ctx);
            app.dialogs(ctx);
        });
        let mut text = String::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut text);
        }
        // Nothing here can hand the texture atlas to a renderer, and dropping
        // it undelivered panics.
        output.textures_delta.clear();
        text
    }

    fn collect_text(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                out.push_str(text.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_text(s, out)),
            _ => {}
        }
    }

    /// A dozen episodes finishing at once is one sound, not a dozen — but the
    /// cue that says the whole run is over is never the one that gets dropped.
    #[test]
    fn cues_are_spaced_out_except_the_one_that_ends_the_run() {
        let now = Instant::now();
        let just_now = now - Duration::from_millis(50);
        let a_while_ago = now - Duration::from_secs(5);

        assert!(cue_is_due(CUE_EPISODE_DONE, None, now), "the first cue always plays");
        assert!(!cue_is_due(CUE_EPISODE_DONE, Some(just_now), now));
        assert!(!cue_is_due(CUE_SOMETHING_FAILED, Some(just_now), now));
        assert!(cue_is_due(CUE_EPISODE_DONE, Some(a_while_ago), now));
        assert!(cue_is_due(CUE_RUN_ENDED, Some(just_now), now));
    }

    /// Where episodes go before anyone has said otherwise. It is only a
    /// default — the folder is a setting — but it is the one almost every run
    /// uses, and it sits next to `~/Podbatch/Logging` on purpose.
    #[test]
    fn episodes_default_to_a_folder_beside_the_logs() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(default_out_dir(), home.join("Podbatch").join("Downloads"));
        }
        // And whatever it is, the picker can open somewhere real from it.
        assert!(existing_ancestor(&default_out_dir()).is_dir());
    }

    /// Choosing a theme has to reach egui, or the buttons move a field nothing
    /// reads and the window carries on looking exactly as it did.
    #[test]
    fn choosing_a_theme_repaints_the_window_in_it() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let mut app = test_app();
        app.tab = Tab::Settings;

        // Dark by choice, whatever the machine this runs on is set to.
        app.theme = egui::ThemePreference::System;
        let painted = settings_frame(&ctx, &mut app, egui::ThemePreference::Dark);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        assert!(ctx.style_of(egui::Theme::Dark).visuals.dark_mode);

        // And where the choice lives: in the Settings tab, between the last of
        // the download settings and the keyboard reference — the pane is drawn
        // for real here, so this is the section actually being on screen rather
        // than a method that would draw it if anything called it.
        for label in ["Appearance", "System", "Light", "Dark"] {
            assert!(painted.contains(label), "no {label:?} in the pane: {painted}");
        }
        let at = |needle: &str| painted.find(needle).unwrap_or_else(|| panic!("{needle}"));
        assert!(at("Sound as episodes land") < at("Appearance"));
        assert!(at("Appearance") < at("Keyboard"));

        // And light, which is the same journey the other way.
        settings_frame(&ctx, &mut app, egui::ThemePreference::Light);
        assert_eq!(ctx.theme(), egui::Theme::Light);
    }

    /// One frame of the whole Settings tab, with `choice` picked from the
    /// appearance buttons as if it had been clicked.
    fn settings_frame(
        ctx: &egui::Context,
        app: &mut PodBatchApp,
        choice: egui::ThemePreference,
    ) -> String {
        let mut output = ctx.run_ui(Default::default(), |ui| {
            app.theme = choice;
            app.settings_pane(ui);
        });
        let mut text = String::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut text);
        }
        output.textures_delta.clear();
        text
    }

    #[test]
    fn elides_the_home_directory() {
        if let Some(home) = dirs::home_dir() {
            let want = format!("~{}podcasts", std::path::MAIN_SEPARATOR);
            assert_eq!(elide_path(&home.join("podcasts")), want);
        }
        // A path outside the home directory is shown exactly as it is, on
        // whichever platform's own separator it was built with.
        let outside = Path::new(".").join("opt").join("media");
        assert_eq!(elide_path(&outside), outside.display().to_string());
    }

    // ---- transcripts ------------------------------------------------------

    /// A machine with everything on it, so a test can choose what to take away.
    fn all_tools_present() -> Vec<tools::Found> {
        tools::Tool::all()
            .iter()
            .map(|tool| tools::Found {
                tool: *tool,
                presence: tools::Presence::Ready("here".into()),
                path: Some(PathBuf::from("/usr/bin/thing")),
            })
            .collect()
    }

    fn without(tool: tools::Tool) -> Vec<tools::Found> {
        all_tools_present()
            .into_iter()
            .map(|mut f| {
                if f.tool == tool {
                    f.presence = tools::Presence::Missing;
                    f.path = None;
                }
                f
            })
            .collect()
    }

    /// A folder with audio in it, and a handle to clean it up.
    fn folder_of_audio(names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podbatch-audio-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in names {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        dir
    }

    #[test]
    fn the_transcripts_tab_paints_its_pieces() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut app = test_app();
        app.tab = Tab::Transcripts;
        app.tools = without(tools::Tool::Whisper);

        paint_frame(&ctx, &mut app);
        let painted = paint_frame(&ctx, &mut app);

        for expected in ["Transcripts", "Audio files", "What this needs", "Whisper"] {
            assert!(painted.contains(expected), "{expected:?} missing from {painted}");
        }
        // The missing piece offers a way to put it right; the ones that are
        // there do not.
        assert!(painted.contains("Install…"), "no install button: {painted}");
    }

    #[test]
    fn the_tab_lists_the_audio_in_the_folder_and_ignores_the_rest() {
        let dir = folder_of_audio(&["one.mp3", "two.m4a", "notes.txt", "art.jpg"]);
        let mut app = test_app();
        app.load_audio_folder(dir.clone());

        let listed: Vec<&str> = app.audio.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(listed, vec!["one.mp3", "two.m4a"]);
        assert!(app.audio.iter().all(|a| a.selected), "new files start ticked");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An episode with a transcript beside it starts unticked, so "transcribe"
    /// on a folder half done means the half that isn't.
    ///
    /// Which file counts as "the transcript" follows the chosen format: a `.txt`
    /// sitting next to an episode says nothing about whether the Word document
    /// you asked for exists.
    #[test]
    fn an_already_transcribed_episode_starts_unticked() {
        let dir = folder_of_audio(&["done.mp3", "todo.mp3"]);
        std::fs::write(dir.join("done.docx"), "a transcript").expect("write");

        let mut app = test_app();
        app.load_audio_folder(dir.clone());

        let done = app.audio.iter().find(|a| a.name == "done.mp3").expect("done");
        let todo = app.audio.iter().find(|a| a.name == "todo.mp3").expect("todo");
        assert!(!done.selected, "an existing transcript should start unticked");
        assert!(todo.selected);

        // Switch format and the .docx no longer answers the question.
        app.transcript_format = document::Format::Pdf;
        app.load_audio_folder(dir.clone());
        assert!(
            app.audio.iter().all(|a| a.selected),
            "a .docx should not count as a PDF transcript"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of the Ollama step being optional.
    #[test]
    fn transcribing_is_possible_without_ollama_but_not_without_whisper() {
        let mut app = test_app();

        app.tools = without(tools::Tool::Ollama);
        assert!(app.can_transcribe(), "Ollama is optional");
        app.tools = without(tools::Tool::OllamaModel);
        assert!(app.can_transcribe(), "the Ollama model is optional");

        for required in [
            tools::Tool::Ffmpeg,
            tools::Tool::Whisper,
            tools::Tool::WhisperModel,
        ] {
            app.tools = without(required);
            assert!(!app.can_transcribe(), "{required:?} should be required");
        }
    }

    /// The user's rule: nothing lands on the machine without being asked about.
    #[test]
    fn nothing_is_installed_without_being_asked_first() {
        let mut app = test_app();
        app.tools = without(tools::Tool::Whisper);

        let install = tools::Install::Run {
            program: "brew".into(),
            args: vec!["install".into(), "whisper-cpp".into()],
            display: "brew install whisper-cpp".into(),
        };
        app.ask(Dialog::Install { tool: tools::Tool::Whisper, install });

        assert!(app.dialog.is_some(), "asking should put a question up");
        assert!(app.installing.is_none(), "nothing may run before the answer");

        // Said no: still nothing.
        let dialog = app.dialog.take().expect("a dialog");
        app.declined(dialog);
        assert!(app.installing.is_none(), "a refusal must not install anything");
        assert!(app.install_rx.is_none());
    }

    /// What is agreed to has to be what is shown, so the command goes in the box
    /// verbatim rather than being described.
    #[test]
    fn the_install_question_shows_the_command_that_will_run() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut app = test_app();
        app.tab = Tab::Transcripts;
        app.ask(Dialog::Install {
            tool: tools::Tool::Whisper,
            install: tools::plan(tools::Tool::Whisper, Some(tools::Manager::Homebrew)),
        });

        paint_frame(&ctx, &mut app);
        let painted = paint_frame(&ctx, &mut app);
        assert!(painted.contains("Install Whisper?"), "{painted}");
        assert!(painted.contains("brew install whisper-cpp"), "{painted}");
        assert!(painted.contains("Copy command"), "{painted}");
    }

    /// A bootstrap that needs a password is never offered as something we run.
    #[test]
    fn a_missing_package_manager_offers_a_terminal_rather_than_an_install() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut app = test_app();
        app.tab = Tab::Transcripts;
        app.ask(Dialog::Install {
            tool: tools::Tool::PackageManager,
            install: tools::plan(tools::Tool::PackageManager, None),
        });

        paint_frame(&ctx, &mut app);
        let painted = paint_frame(&ctx, &mut app);
        assert!(painted.contains("Open Terminal"), "{painted}");
        assert!(
            !painted.contains("Install\n") && painted.contains("Nothing is installed by pressing"),
            "{painted}"
        );
    }

    /// The two kinds of speaker numbering are not interchangeable, so the box
    /// that starts a run says which one is about to happen.
    #[test]
    fn the_transcribe_question_says_which_kind_of_numbering_it_will_do() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        let mut app = test_app();
        app.tab = Tab::Transcripts;
        app.ask(Dialog::Transcribe { files: 3, labelling: true });
        paint_frame(&ctx, &mut app);
        let painted = paint_frame(&ctx, &mut app);
        assert!(painted.contains("Transcribe 3 episodes?"), "{painted}");
        assert!(painted.contains("same person throughout"), "{painted}");

        // A context of its own: egui keeps per-id state between frames, and a
        // second modal with the same id on the same context is not a fresh box.
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut app = test_app();
        app.tab = Tab::Transcripts;
        app.ask(Dialog::Transcribe { files: 1, labelling: false });
        paint_frame(&ctx, &mut app);
        let painted = paint_frame(&ctx, &mut app);
        assert!(painted.contains("Transcribe 1 episode?"), "{painted}");
        assert!(painted.contains("alternate between Speaker 1"), "{painted}");
    }

    /// Starting is guarded the same way the button is: no files, no tools, or a
    /// run already going all mean the question is never asked.
    #[test]
    fn transcribing_cannot_start_from_a_state_the_button_would_refuse() {
        let dir = folder_of_audio(&["one.mp3"]);

        let mut app = test_app();
        app.tools = all_tools_present();
        app.load_audio_folder(dir.clone());

        // Nothing ticked.
        app.audio.iter_mut().for_each(|a| a.selected = false);
        app.ask_transcribe();
        assert!(app.dialog.is_none(), "nothing ticked should ask nothing");

        // Ticked, but the tools are gone.
        app.audio.iter_mut().for_each(|a| a.selected = true);
        app.tools = without(tools::Tool::Whisper);
        app.ask_transcribe();
        assert!(app.dialog.is_none(), "no Whisper should ask nothing");

        // Ticked, tools present, but already running.
        app.tools = all_tools_present();
        app.transcribing = true;
        app.ask_transcribe();
        assert!(app.dialog.is_none(), "a second run must not be startable");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The transcript belongs in the file the user asked for, and nowhere else.
    ///
    /// It used to go through `say`, which logs — so a whole episode of text was
    /// copied into `output.log` and again into `debug.log`, burying the few
    /// lines that say what actually happened under an hour of conversation.
    #[test]
    fn the_transcript_reaches_the_screen_without_reaching_the_logs() {
        let mut app = test_app();

        app.show(OutputKind::Muted, "a line of transcript".into());
        assert_eq!(app.output.len(), 1, "it should be on screen");

        // `show` is the only path that skips the log, so the guard against a
        // future edit quietly routing this back through `say` is that `say`
        // and `show` are different functions with different jobs.
        app.say(OutputKind::Plain, "something that happened".into());
        assert_eq!(app.output.len(), 2);
    }

    /// The box holds a run's worth of lines and no more, however long the
    /// episode. A transcript is thousands of lines and must not grow it without
    /// bound.
    #[test]
    fn the_output_box_stays_bounded_while_a_transcript_pours_in() {
        let mut app = test_app();
        for i in 0..OUTPUT_LIMIT + 500 {
            app.show(OutputKind::Muted, format!("line {i}"));
        }
        assert_eq!(app.output.len(), OUTPUT_LIMIT);
        // The newest survive, not the oldest.
        assert_eq!(
            app.output.last().map(|l| l.text.as_str()),
            Some(format!("line {}", OUTPUT_LIMIT + 499).as_str())
        );
    }

    /// Ticking acts on whichever list is in front of the user.
    #[test]
    fn select_all_follows_the_tab_it_is_pressed_on() {
        let dir = folder_of_audio(&["one.mp3"]);
        let mut app = test_app();
        app.feeds = vec![feed(false, vec![])];
        app.load_audio_folder(dir.clone());
        app.audio.iter_mut().for_each(|a| a.selected = false);

        app.tab = Tab::Transcripts;
        app.perform(Action::SelectAll);
        assert!(app.audio[0].selected, "the episodes should have been ticked");
        assert!(!app.feeds[0].selected, "the podcasts should have been left alone");

        app.tab = Tab::Downloads;
        app.perform(Action::SelectAll);
        assert!(app.feeds[0].selected);

        std::fs::remove_dir_all(&dir).ok();
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
            theme: egui::ThemePreference::System,
            feeds: Vec::new(),
            running_map: Vec::new(),
            output: Vec::new(),
            output_scroll_to: None,
            running: false,
            cancelling: false,
            outcome: None,
            cancel: None,
            proceed: None,
            rx: None,
            notify: Arc::new(|| {}),
            dialog: None,
            pending_plan: None,
            focus_dialog: false,
            downloads_began: None,
            measured_rate: None,
            last_cue: None,
            audio_dir: None,
            audio: Vec::new(),
            tools: Vec::new(),
            label_speakers: true,
            skip_transcribed: true,
            transcript_format: document::Format::default(),
            transcript_style: document::Style::default(),
            transcribing: false,
            transcribe_cancel: None,
            transcribe_rx: None,
            installing: None,
            install_rx: None,
            install_progress: None,
        }
    }
}
