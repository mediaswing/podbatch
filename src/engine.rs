//! The download engine.
//!
//! Runs on its own thread with a Tokio runtime and reports everything it does
//! back to the UI over a channel, so the GUI never blocks on I/O. The engine
//! knows nothing about egui; it just calls `notify` after each message so the
//! front end can wake up and repaint.
//!
//! A run happens in two passes. The first reads every feed, names every file
//! and sets aside the episodes already on disk, ending in an [`Update::Planned`]
//! that says how much there is to fetch; the second does the fetching, and only
//! once the window has said to go ahead via [`Proceed`]. The pause exists so the
//! user can be told the size of what they just asked for before it starts —
//! which is not something either side can know until the feeds have been read.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc::UnboundedSender, Semaphore};

use crate::feed;
use crate::logs;
use crate::opml;
use crate::tags;
use crate::util;

const USER_AGENT: &str = concat!("PodBatch/", env!("CARGO_PKG_VERSION"));
/// Network hiccups are common on podcast CDNs; a couple of retries turns most
/// of them into a blip rather than a failed episode.
const ATTEMPTS: u32 = 3;
/// How often a running download reports progress upward.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

/// How much of one episode to pull down to see how fast the line is, and how
/// long to spend doing it. Whichever comes first: on a fast line this is a
/// quarter of a megabyte and over in a moment, and on a slow one it is a
/// second and a half of waiting and no more — the confirmation box is meant to
/// save the user time, so it cannot cost much of it to appear.
const PROBE_BYTES: u64 = 256 * 1024;
const PROBE_TIME: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub struct Settings {
    /// The podcasts the user ticked. Parsing the OPML is the window's job, so
    /// that the list can be shown and chosen from before anything is fetched.
    pub subscriptions: Vec<opml::Subscription>,
    pub out_dir: PathBuf,
    /// Maximum simultaneous network transfers across all podcasts.
    pub concurrency: usize,
    /// Newest N episodes per podcast; `None` means every episode.
    pub limit: Option<usize>,
    pub skip_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedStatus {
    Pending,
    Fetching,
    Downloading,
    Done,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpisodeStatus {
    Pending,
    Downloading,
    Done,
    /// Already on disk, so there was nothing to fetch.
    Skipped,
    /// Taken out of the job by the user, who un-ticked it while the run was
    /// held at the confirmation. Kept apart from `Skipped` because the two are
    /// different answers to "why is this episode not here?" — one is the app
    /// noticing it already had it, the other is a decision somebody made.
    Unticked,
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct EpisodeInfo {
    pub title: String,
    pub file_name: String,
    pub size_hint: Option<u64>,
}

/// What a run would fetch, worked out from the feeds before anything is
/// downloaded. Everything the confirmation box needs to describe the job.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Episodes that would actually be downloaded.
    pub episodes: usize,
    /// What those episodes come to, counting only the ones that declare a size.
    pub bytes: u64,
    /// How many of them declare no size at all, so `bytes` is a floor and not a
    /// total. Plenty of feeds omit the enclosure length.
    pub unsized_episodes: usize,
    /// Episodes already on disk, which are not part of the job.
    pub skipped: usize,
    /// Bytes per second **one** connection managed on a slice of a real
    /// episode, if the probe got anything worth measuring. Several downloads
    /// run at once, so the whole run should manage more than this rather than
    /// less — see [`probe_rate`].
    pub probed_rate: Option<f64>,
}

/// Messages from the engine to the UI. Feeds and episodes are addressed by
/// index into the lists the engine announced earlier.
#[derive(Debug)]
pub enum Update {
    FeedStatus { feed: usize, status: FeedStatus },
    FeedFolder { feed: usize, name: String },
    Episodes { feed: usize, episodes: Vec<EpisodeInfo> },
    EpisodeStatus { feed: usize, episode: usize, status: EpisodeStatus },
    Progress { feed: usize, episode: usize, done: u64, total: Option<u64> },
    Log(String),
    /// A line like [`Update::Log`], for something that went wrong.
    ///
    /// Kept apart from `Log` so the window can colour it and the output log can
    /// file it as a failure. Without this, a feed that could not be fetched —
    /// which loses a whole podcast — goes down in the log looking exactly like
    /// a line about how many episodes there are.
    Problem(String),
    /// Every feed has been read; this is the job, waiting to be agreed to.
    Planned(Plan),
    Finished { cancelled: bool },
}

/// Handle the UI keeps so it can stop a run in progress.
#[derive(Clone)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    /// Public because the transcription engine shares this handle: stopping a
    /// run means the same thing to both, and a second flag would be a second
    /// thing to remember to set.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle the UI keeps so it can let the downloads start once the user has seen
/// what they come to.
///
/// A run holds between the two passes until this is set or [`Cancel`] is: the
/// engine never assumes the answer, because "yes" is the one that spends the
/// next half hour of someone's bandwidth.
#[derive(Clone)]
pub struct Proceed(Arc<AtomicBool>);

impl Proceed {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub fn go(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    fn is_go(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Proceed {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle the UI keeps so it can take individual episodes back out of the job.
///
/// The window cannot know what is in a podcast until the feeds have been read,
/// so per-episode choosing can only happen in the pause [`Proceed`] creates.
/// This is what carries that choice across: the window fills it in before it
/// says to go ahead, and the engine reads it once, on the way out of the pause.
/// Episodes are named by the same `(feed, episode)` indices every [`Update`]
/// uses, so both sides are talking about the same thing without either having
/// to send the list back.
#[derive(Clone, Default)]
pub struct Skips(Arc<Mutex<HashSet<(usize, usize)>>>);

impl Skips {
    pub fn new() -> Self {
        Self::default()
    }

    /// What to leave out. Replaces whatever was there before, so the window can
    /// simply hand over the state of its tick boxes rather than a difference.
    pub fn set(&self, skips: HashSet<(usize, usize)>) {
        // A poisoned lock means a thread panicked mid-write. Nothing here is
        // load-bearing enough to bring the run down over: the worst a stale set
        // does is download an episode the user had un-ticked.
        if let Ok(mut held) = self.0.lock() {
            *held = skips;
        }
    }

    /// Whether this episode is one to leave out. `pub(crate)` so the window's
    /// own tests can check that what the user un-ticked is what was handed
    /// over — the wiring between the two is the whole point of this handle.
    pub(crate) fn holds(&self, feed: usize, episode: usize) -> bool {
        self.0
            .lock()
            .is_ok_and(|held| held.contains(&(feed, episode)))
    }
}

/// Shared context passed down to each feed/episode task.
struct Ctx {
    tx: UnboundedSender<Update>,
    notify: Arc<dyn Fn() + Send + Sync>,
    cancel: Cancel,
    proceed: Proceed,
    skips: Skips,
    client: reqwest::Client,
    permits: Arc<Semaphore>,
    settings: Settings,
}

impl Ctx {
    fn send(&self, update: Update) {
        // A send error just means the UI is gone; the run will wind down on its
        // own via the cancel flag.
        let _ = self.tx.send(update);
        (self.notify)();
    }

    fn log(&self, msg: impl Into<String>) {
        self.send(Update::Log(msg.into()));
    }

    /// Something that didn't work, for the window to colour and the output log
    /// to file as a failure.
    fn problem(&self, msg: impl Into<String>) {
        self.send(Update::Problem(msg.into()));
    }
}

/// Start a run on a background thread. Returns immediately.
pub fn spawn(
    settings: Settings,
    tx: UnboundedSender<Update>,
    cancel: Cancel,
    proceed: Proceed,
    skips: Skips,
    notify: Arc<dyn Fn() + Send + Sync>,
) {
    std::thread::Builder::new()
        .name("podbatch-engine".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Update::Problem(format!("Could not start the downloader: {e}")));
                    let _ = tx.send(Update::Finished { cancelled: false });
                    notify();
                    return;
                }
            };
            runtime.block_on(run(settings, tx, cancel, proceed, skips, notify));
        })
        .expect("spawn engine thread");
}

/// reqwest is built without a built-in crypto provider (see Cargo.toml), so one
/// has to be installed before the first client is built or the build panics.
///
/// Done here, behind a `Once`, rather than in `main`: this is the only module
/// that builds an HTTP client, and a test that reaches it without going through
/// `main` would otherwise fail on a detail it has nothing to do with. Installing
/// twice is harmless — the second call reports that one is already there, which
/// is just as good.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

async fn run(
    settings: Settings,
    tx: UnboundedSender<Update>,
    cancel: Cancel,
    proceed: Proceed,
    skips: Skips,
    notify: Arc<dyn Fn() + Send + Sync>,
) {
    install_crypto_provider();

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        // Guards against a stalled connection that never closes; the overall
        // transfer is deliberately untimed because episodes can be large.
        .read_timeout(Duration::from_secs(60))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Update::Problem(format!("Could not create HTTP client: {e}")));
            let _ = tx.send(Update::Finished { cancelled: false });
            notify();
            return;
        }
    };

    let concurrency = settings.concurrency.clamp(1, 16);
    let ctx = Arc::new(Ctx {
        tx,
        notify,
        cancel,
        proceed,
        skips,
        client,
        permits: Arc::new(Semaphore::new(concurrency)),
        settings,
    });

    let subs = ctx.settings.subscriptions.clone();
    if subs.is_empty() {
        ctx.log("Nothing selected.".to_string());
        ctx.send(Update::Finished { cancelled: false });
        return;
    }

    ctx.log(format!(
        "Downloading {} podcast{} into {}",
        subs.len(),
        if subs.len() == 1 { "" } else { "s" },
        ctx.settings.out_dir.display()
    ));

    if let Err(e) = tokio::fs::create_dir_all(&ctx.settings.out_dir).await {
        ctx.problem(format!(
            "Cannot create {}: {e}",
            ctx.settings.out_dir.display()
        ));
        ctx.send(Update::Finished { cancelled: false });
        return;
    }

    // Reserve every folder name up front so two podcasts that sanitise to the
    // same string can't collide once the tasks start running in parallel.
    let mut taken: Vec<String> = Vec::new();
    let folders: Vec<String> = subs
        .iter()
        .map(|s| unique_name(&util::sanitize(&s.title), "", &mut taken))
        .collect();

    for (i, name) in folders.iter().enumerate() {
        ctx.send(Update::FeedFolder { feed: i, name: name.clone() });
    }

    // Pass one: read the feeds and work out the job.
    let reads: Vec<FeedRead> =
        futures_util::stream::iter(subs.into_iter().zip(folders).enumerate().map(
            |(index, (sub, folder))| {
                let ctx = Arc::clone(&ctx);
                async move { plan_feed(ctx, index, sub, folder).await }
            },
        ))
        .buffer_unordered(concurrency)
        .collect()
        .await;

    // Every file the second pass could leave half-written, worked out while the
    // reads are still here to ask — the pass that fetches them consumes them,
    // and a run that is stopped part way through has to know what to sweep up
    // afterwards. Anything already on disk under one of these names is a
    // fragment of the same episode from an earlier interrupted run, and goes
    // the same way.
    let leftovers: Vec<PathBuf> = reads
        .iter()
        .filter_map(|read| read.plan.as_ref())
        .flat_map(|plan| &plan.episodes)
        .map(|(_, _, path)| part_path(path))
        .collect();

    if ctx.cancel.is_cancelled() {
        abandon(&ctx, &reads);
        collect_garbage(&ctx, &leftovers).await;
        ctx.log("Stopped.".to_string());
        ctx.send(Update::Finished { cancelled: true });
        return;
    }

    let mut plan = summarise(&reads);
    let outstanding = plan.episodes;

    // Only worth asking the line how fast it is when there is something to
    // fetch — and only then is anyone going to be shown the answer.
    if outstanding > 0 && !ctx.cancel.is_cancelled()
        && let Some(first) = reads
            .iter()
            .filter_map(|r| r.plan.as_ref())
            .flat_map(|p| &p.episodes)
            .map(|(_, episode, _)| episode)
            .next()
    {
        plan.probed_rate = probe_rate(&ctx, first).await;
        logs::debug(match plan.probed_rate {
            Some(rate) => format!("probed the line at {}", util::human_rate(rate)),
            None => "the probe measured nothing usable".to_string(),
        });
    }

    logs::debug(format!(
        "planned: {} episode(s) to fetch, {} of declared size, {} declaring none, \
         {} already here",
        plan.episodes,
        util::human_bytes(plan.bytes),
        plan.unsized_episodes,
        plan.skipped
    ));

    ctx.send(Update::Planned(plan));

    // Nothing to fetch is nothing to agree to, so a run that has already got
    // everything finishes rather than stopping to ask about no work.
    if outstanding > 0 && !wait_for_go(&ctx).await {
        abandon(&ctx, &reads);
        collect_garbage(&ctx, &leftovers).await;
        ctx.log("Stopped.".to_string());
        ctx.send(Update::Finished { cancelled: true });
        return;
    }

    // What the user un-ticked while the run was held. Read here and nowhere
    // else: from this line on the episodes are in flight, and a job that could
    // change under the transfers already running would be a job nobody — the
    // engine, the window, or the person watching it — could describe.
    let reads = drop_unticked(&ctx, reads);

    // Pass two: fetch it.
    futures_util::stream::iter(reads.into_iter().filter_map(|read| read.plan).map(|plan| {
        let ctx = Arc::clone(&ctx);
        async move { download_feed(ctx, plan).await }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<()>>()
    .await;

    let cancelled = ctx.cancel.is_cancelled();
    logs::debug(format!("run finished, cancelled: {cancelled}"));

    // Every transfer has returned by now, so the files below are nobody's any
    // more and the ones still ending in `.part` are the ones that were stopped
    // mid-flight.
    if cancelled {
        collect_garbage(&ctx, &leftovers).await;
    }

    ctx.log(if cancelled {
        "Stopped.".to_string()
    } else {
        "All done.".to_string()
    });
    ctx.send(Update::Finished { cancelled });
}

/// Hold until the window says to go ahead. `false` means it said stop instead,
/// or the user closed the window and the run should wind down.
///
/// Polled rather than signalled: this is the one place in the engine that waits
/// on a person, so a check twenty times a second costs nothing measurable and
/// keeps the handle the same shape as [`Cancel`] next to it.
async fn wait_for_go(ctx: &Ctx) -> bool {
    while !ctx.proceed.is_go() {
        if ctx.cancel.is_cancelled() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !ctx.cancel.is_cancelled()
}

/// Put the podcasts that were still owed episodes back to waiting.
///
/// A run given up between the two passes — stopped, or turned down at the
/// confirmation — leaves those feeds read but undownloaded. Without this they
/// keep the status they were last given and sit there saying "reading…" for as
/// long as the window is open, which is a lie about a run that has ended.
fn abandon(ctx: &Ctx, reads: &[FeedRead]) {
    for plan in reads.iter().filter_map(|r| r.plan.as_ref()) {
        ctx.send(Update::FeedStatus { feed: plan.index, status: FeedStatus::Pending });
    }
}

/// Take the episodes the user un-ticked out of the job, and say so for each one.
///
/// Each is announced as [`EpisodeStatus::Unticked`] rather than quietly dropped:
/// its row is already on screen from the reading pass, and a row left saying
/// "waiting" for something that will never be fetched is the sort of thing
/// people sit and watch.
///
/// A podcast left with nothing to fetch is finished rather than downloading —
/// and, because `download_feed` is what creates the folders, un-ticking a whole
/// podcast's episodes now leaves no empty folder behind either.
fn drop_unticked(ctx: &Ctx, reads: Vec<FeedRead>) -> Vec<FeedRead> {
    reads
        .into_iter()
        .map(|mut read| {
            let Some(plan) = read.plan.as_mut() else {
                return read;
            };
            let feed = plan.index;

            let before = plan.episodes.len();
            plan.episodes.retain(|(episode, _, _)| {
                let wanted = !ctx.skips.holds(feed, *episode);
                if !wanted {
                    ctx.send(Update::EpisodeStatus {
                        feed,
                        episode: *episode,
                        status: EpisodeStatus::Unticked,
                    });
                }
                wanted
            });

            let dropped = before - plan.episodes.len();
            if dropped > 0 {
                logs::debug(format!("feed {feed}: {dropped} episode(s) un-ticked"));
            }
            if plan.episodes.is_empty() {
                ctx.send(Update::FeedStatus { feed, status: FeedStatus::Done });
                read.plan = None;
            }
            read
        })
        .collect()
}

/// Add up what the feeds came back with, into the description of the job that
/// the confirmation box is built from.
fn summarise(reads: &[FeedRead]) -> Plan {
    let mut plan = Plan::default();

    for read in reads {
        plan.skipped += read.skipped;
        for (_, episode, _) in read.plan.iter().flat_map(|p| &p.episodes) {
            plan.episodes += 1;
            match episode.length {
                Some(length) => plan.bytes += length,
                None => plan.unsized_episodes += 1,
            }
        }
    }

    plan
}

/// Time a slice of a real episode, to have something to estimate with.
///
/// The feeds cannot answer this, though it is tempting to think they can. They
/// are XML, they arrive gzipped from almost every host, and reqwest inflates
/// them on the way in — at which point `content_length` is `None` and the bytes
/// handed back are several times the bytes that crossed the wire. Timing a feed
/// therefore measures either nothing or a fiction. Media is already compressed
/// and comes down untouched, so a piece of the very thing that is about to be
/// downloaded is the one honest measurement available.
///
/// One connection, so this is a floor for what the run will manage rather than
/// a prediction of it: several downloads run at once. A short read of an
/// episode that is about to be fetched in full anyway, and it is thrown away —
/// this leaves nothing behind on disk.
async fn probe_rate(ctx: &Ctx, episode: &feed::Episode) -> Option<f64> {
    let _permit = ctx.permits.acquire().await.ok()?;

    let resp = ctx
        .client
        .get(&episode.url)
        .header(reqwest::header::RANGE, format!("bytes=0-{}", PROBE_BYTES - 1))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    let resp = resp.error_for_status().ok()?;
    // No length means the body was decoded on the way in, so what is counted
    // below is not what crossed the wire. Better no estimate than a wrong one.
    resp.content_length()?;

    let started = Instant::now();
    let mut bytes = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if ctx.cancel.is_cancelled() {
            return None;
        }
        bytes += chunk.ok()?.len() as u64;
        if bytes >= PROBE_BYTES || started.elapsed() >= PROBE_TIME {
            break;
        }
    }

    Sample { bytes, seconds: started.elapsed().as_secs_f64() }.rate()
}

/// A stretch of transfer, and what it says about the line.
#[derive(Debug, Clone, Copy)]
struct Sample {
    bytes: u64,
    seconds: f64,
}

impl Sample {
    /// Bytes per second, for a sample big enough and long enough to mean
    /// anything. A handful of bytes in a fraction of a millisecond measures the
    /// clock rather than the connection.
    fn rate(&self) -> Option<f64> {
        (self.bytes >= 4096 && self.seconds >= 0.005 && self.seconds.is_finite())
            .then(|| self.bytes as f64 / self.seconds)
    }
}

/// What reading one feed produced: work to do, and work that turned out to be
/// already done.
#[derive(Default)]
struct FeedRead {
    /// `None` when there is nothing to download from this podcast — it failed,
    /// it was abandoned, it lists no media, or every episode is already here.
    plan: Option<FeedPlan>,
    /// How many of its episodes were already on disk.
    skipped: usize,
}

/// A feed that has been read, with the episodes it still owes and where each
/// one goes.
struct FeedPlan {
    index: usize,
    /// Who the episodes belong to, for the tags written into each file.
    show: Arc<Show>,
    /// The episodes to download: the index each was announced under, what to
    /// fetch, and where it lands.
    episodes: Vec<(usize, feed::Episode, PathBuf)>,
}

/// The podcast an episode came from, as the tags will record it.
///
/// The title is the subscription's, not the feed's own — the same string the
/// folder is named after, so what a player shows and what the folder says stay
/// the same thing.
struct Show {
    title: String,
    feed_url: String,
    /// Whether this podcast has already been mentioned as one whose files can't
    /// be tagged. Said once, not once per episode.
    said_untaggable: AtomicBool,
}

async fn plan_feed(
    ctx: Arc<Ctx>,
    index: usize,
    sub: opml::Subscription,
    folder: String,
) -> FeedRead {
    if ctx.cancel.is_cancelled() {
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Pending });
        return FeedRead::default();
    }

    ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Fetching });

    logs::debug(format!("feed {index} \"{}\": fetching {}", sub.title, sub.url));

    let body = match fetch_feed(&ctx, &sub.url).await {
        Ok(body) => body,
        Err(e) => {
            // Stopping is not a failure. A feed abandoned mid-fetch goes back
            // to waiting rather than being reported — and coloured — as broken,
            // which is what every unread feed in the list would otherwise show
            // the moment the user pressed Stop.
            let status = if ctx.cancel.is_cancelled() {
                FeedStatus::Pending
            } else {
                ctx.problem(format!("{}: {e}", sub.title));
                FeedStatus::Failed(e)
            };
            ctx.send(Update::FeedStatus { feed: index, status });
            return FeedRead::default();
        }
    };

    let parsed = match feed::parse(&body) {
        Ok(f) => f,
        Err(e) => {
            let msg = e.to_string();
            ctx.problem(format!("{}: {msg}", sub.title));
            ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Failed(msg) });
            return FeedRead::default();
        }
    };

    // Feeds are conventionally newest-first, so "latest N" is simply the head.
    let mut episodes = parsed.episodes;
    logs::debug(format!(
        "feed {index} \"{}\": {} bytes of XML, {} episode(s) with media",
        sub.title,
        body.len(),
        episodes.len()
    ));
    if let Some(limit) = ctx.settings.limit {
        episodes.truncate(limit);
    }

    if episodes.is_empty() {
        ctx.log(format!("{}: no episodes with downloadable media", sub.title));
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Done });
        ctx.send(Update::Episodes { feed: index, episodes: Vec::new() });
        return FeedRead::default();
    }

    // Worked out but not created: a pass that only reads the feeds must leave
    // nothing behind, or turning the confirmation down would still litter the
    // output folder with a folder per podcast. `download_feed` makes it.
    let dir = ctx.settings.out_dir.join(&folder);

    // Name every file before downloading anything, so concurrent tasks in this
    // feed can't race to the same path.
    //
    // Two episodes published in the same minute are told apart by the ` (2)`
    // that `unique_name` adds, which goes to whichever of them the feed lists
    // second. Should the feed later list them the other way round, the two
    // names swap — and since both files are on disk at the right size, neither
    // is fetched again. That costs nothing: both episodes are still here, and
    // each carries its own title in its own tags, so the only thing that has
    // moved is which of two same-minute stamps sits on which.
    let mut used: Vec<String> = Vec::new();
    let planned: Vec<(feed::Episode, String)> = episodes
        .into_iter()
        .map(|ep| {
            let ext = util::extension_for(&ep.url, ep.mime.as_deref());
            let stem = match &ep.published {
                Some(published) => published.stamp(),
                // A feed that won't say when an episode came out leaves
                // nothing to stamp it with, and a made-up time would be a claim
                // about the episode that isn't true — one the next run would
                // then have to make identically to find the file again. Its
                // title is the one true thing left to name it by.
                None => util::sanitize(&ep.title),
            };
            let name = unique_name(&stem, &ext, &mut used);
            // The names are a timestamp apiece now, so which episode became
            // which file is a question only the debug log can answer.
            logs::debug(format!(
                "feed {index} \"{}\": \"{}\" -> {name}",
                sub.title, ep.title
            ));
            (ep, name)
        })
        .collect();

    ctx.send(Update::Episodes {
        feed: index,
        episodes: planned
            .iter()
            .map(|(ep, name)| EpisodeInfo {
                title: ep.title.clone(),
                file_name: name.clone(),
                size_hint: ep.length,
            })
            .collect(),
    });

    // Settle the episodes already on disk here rather than when their turn to
    // download would have come. They are not part of the job, so the estimate
    // the user is about to be shown must not include them — and there is no
    // sense in making them wait behind a question about work that isn't there.
    let mut to_fetch: Vec<(usize, feed::Episode, PathBuf)> = Vec::new();
    let mut skipped = 0;
    for (ep_index, (episode, name)) in planned.into_iter().enumerate() {
        let path = dir.join(&name);
        if let Some(size) = already_downloaded(&ctx, &episode, &path).await {
            logs::debug(format!(
                "{}: already here at {}, not fetching again",
                path.display(),
                util::human_bytes(size)
            ));
            skipped += 1;
            ctx.send(Update::Progress {
                feed: index,
                episode: ep_index,
                done: size,
                total: Some(size),
            });
            ctx.send(Update::EpisodeStatus {
                feed: index,
                episode: ep_index,
                status: EpisodeStatus::Skipped,
            });
        } else {
            to_fetch.push((ep_index, episode, path));
        }
    }

    logs::debug(format!(
        "feed {index} \"{}\": {} to fetch, {skipped} already here",
        sub.title,
        to_fetch.len()
    ));

    if to_fetch.is_empty() {
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Done });
        return FeedRead { plan: None, skipped };
    }

    let show = Arc::new(Show {
        title: sub.title,
        feed_url: sub.url,
        said_untaggable: AtomicBool::new(false),
    });
    FeedRead {
        plan: Some(FeedPlan { index, show, episodes: to_fetch }),
        skipped,
    }
}

/// Fetch one podcast's outstanding episodes. Everything here was decided in
/// `plan_feed`; this is only the moving of the bytes.
async fn download_feed(ctx: Arc<Ctx>, plan: FeedPlan) {
    let index = plan.index;

    // The folder the planning pass worked out but deliberately did not create.
    if let Some(dir) = plan.episodes.first().and_then(|(_, _, path)| path.parent())
        && let Err(e) = tokio::fs::create_dir_all(dir).await
    {
        let msg = format!("cannot create folder: {e}");
        ctx.problem(format!("{}: {msg}", dir.display()));
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Failed(msg) });
        return;
    }

    ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Downloading });

    let concurrency = ctx.settings.concurrency.clamp(1, 16);
    let show = plan.show;
    futures_util::stream::iter(plan.episodes.into_iter().map(|(ep_index, ep, path)| {
        let ctx = Arc::clone(&ctx);
        let show = Arc::clone(&show);
        async move {
            let status = download_episode(&ctx, index, ep_index, &show, &ep, &path).await;
            ctx.send(Update::EpisodeStatus { feed: index, episode: ep_index, status });
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<()>>()
    .await;

    let status = if ctx.cancel.is_cancelled() {
        FeedStatus::Pending
    } else {
        FeedStatus::Done
    };
    ctx.send(Update::FeedStatus { feed: index, status });
}

/// The size on disk of an episode that is already here in full, if it is.
///
/// A file shorter than the feed declares means the last run was interrupted
/// after the rename, or the publisher swapped the file for a shorter one;
/// either way it gets fetched again, and says so.
///
/// Short of it, rather than different from it: the tags written after a
/// download add bytes the feed's figure knows nothing about, so a complete
/// episode on disk is always the declared length or a little over. Reading that
/// as a mismatch would fetch every episode again on every run.
async fn already_downloaded(ctx: &Ctx, episode: &feed::Episode, path: &Path) -> Option<u64> {
    if !ctx.settings.skip_existing {
        return None;
    }

    let size = tokio::fs::metadata(path).await.ok()?.len();
    let complete = match episode.length {
        Some(declared) => size >= declared,
        None => size > 0,
    };
    if complete {
        return Some(size);
    }

    ctx.log(format!(
        "{}: on-disk size {} is short of the feed's {}, downloading again",
        path.file_name().unwrap_or_default().to_string_lossy(),
        util::human_bytes(size),
        episode.length.map(util::human_bytes).unwrap_or_default()
    ));
    None
}

async fn fetch_feed(ctx: &Ctx, url: &str) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        if ctx.cancel.is_cancelled() {
            return Err("cancelled".into());
        }

        let _permit = ctx.permits.acquire().await.map_err(|_| "shutting down")?;
        let result = async {
            let resp = ctx
                .client
                .get(url)
                .timeout(Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| short_err(&e))?;
            let resp = resp.error_for_status().map_err(|e| short_err(&e))?;
            resp.text().await.map_err(|e| short_err(&e))
        }
        .await;

        match result {
            Ok(body) => return Ok(body),
            Err(e) => {
                logs::debug(format!("{url}: attempt {attempt} of {ATTEMPTS} failed: {e}"));
                last = e;
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
                }
            }
        }
    }
    Err(last)
}

async fn download_episode(
    ctx: &Ctx,
    feed_index: usize,
    ep_index: usize,
    show: &Show,
    episode: &feed::Episode,
    path: &Path,
) -> EpisodeStatus {
    if ctx.cancel.is_cancelled() {
        return EpisodeStatus::Cancelled;
    }

    ctx.send(Update::EpisodeStatus {
        feed: feed_index,
        episode: ep_index,
        status: EpisodeStatus::Downloading,
    });

    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        if ctx.cancel.is_cancelled() {
            return EpisodeStatus::Cancelled;
        }

        match transfer(ctx, feed_index, ep_index, show, episode, path).await {
            Ok(status) => return status,
            Err(e) => {
                logs::debug(format!(
                    "{}: attempt {attempt} of {ATTEMPTS} failed: {e}",
                    path.display()
                ));
                last = e;
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(600 * attempt as u64)).await;
                }
            }
        }
    }

    // A note rather than a problem, and the only failure here that is: the
    // status returned on the next line is what the window turns into the
    // episode's own failure line, so calling this one a failure too would put
    // the same lost episode in the log twice.
    ctx.log(format!(
        "{}: {last}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    EpisodeStatus::Failed(last)
}

/// One attempt at moving the bytes. Downloads into a `.part` file and only
/// renames into place once the transfer completes, so an interrupted run never
/// leaves a truncated episode that looks finished.
async fn transfer(
    ctx: &Ctx,
    feed_index: usize,
    ep_index: usize,
    show: &Show,
    episode: &feed::Episode,
    path: &Path,
) -> Result<EpisodeStatus, String> {
    let _permit = ctx.permits.acquire().await.map_err(|_| "shutting down")?;

    let part = part_path(path);

    // Resume a previous attempt if we left one behind.
    let existing = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);

    let mut request = ctx.client.get(&episode.url);
    if existing > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
    }

    let resp = request.send().await.map_err(|e| short_err(&e))?;
    let resp = resp.error_for_status().map_err(|e| short_err(&e))?;

    // The server may ignore our Range header, in which case we start over.
    let resuming = existing > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let already = if resuming { existing } else { 0 };

    logs::debug(format!(
        "{}: {} said {}{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        episode.url,
        resp.status(),
        match (existing, resuming) {
            (0, _) => String::new(),
            (had, true) => format!(", resuming from {}", util::human_bytes(had)),
            (had, false) => format!(
                ", starting over — the {} already here was not resumable",
                util::human_bytes(had)
            ),
        }
    ));

    let total = resp
        .content_length()
        .map(|len| len + already)
        .or(episode.length);

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resuming)
        .truncate(!resuming)
        .open(&part)
        .await
        .map_err(|e| format!("cannot write {}: {e}", part.display()))?;

    let mut done = already;
    let mut last_report = Instant::now();
    ctx.send(Update::Progress { feed: feed_index, episode: ep_index, done, total });

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if ctx.cancel.is_cancelled() {
            // Flushed and dropped rather than simply abandoned, so the handle is
            // closed before `collect_garbage` comes past to delete the file —
            // Windows will not remove a file anything still has open.
            let _ = file.flush().await;
            return Ok(EpisodeStatus::Cancelled);
        }

        let chunk = chunk.map_err(|e| short_err(&e))?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;

        if last_report.elapsed() >= PROGRESS_INTERVAL {
            last_report = Instant::now();
            ctx.send(Update::Progress { feed: feed_index, episode: ep_index, done, total });
        }
    }

    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    tokio::fs::rename(&part, path)
        .await
        .map_err(|e| format!("cannot finish {}: {e}", path.display()))?;

    logs::debug(format!(
        "{}: {} written",
        path.display(),
        util::human_bytes(done)
    ));

    tag_episode(ctx, show, episode, path).await;

    ctx.send(Update::Progress {
        feed: feed_index,
        episode: ep_index,
        done,
        total: Some(done),
    });
    Ok(EpisodeStatus::Done)
}

/// Write what the feed said about the episode into the file that just landed.
///
/// The file is named after the minute it was published, so the tags are the
/// only place its title, show and blurb survive being copied onto a phone. A
/// tag that won't write is worth a line in the log and nothing more: the
/// episode itself is here and plays, which is what was asked for.
async fn tag_episode(ctx: &Ctx, show: &Show, episode: &feed::Episode, path: &Path) {
    let details = tags::Details {
        show: show.title.clone(),
        title: episode.title.clone(),
        published: episode.published,
        description: episode.description.clone(),
        feed_url: show.feed_url.clone(),
    };
    let target = path.to_path_buf();

    // Rewriting a file is blocking work, and on a large episode with an
    // existing tag it is not instant; it does not belong on the runtime.
    let written = tokio::task::spawn_blocking(move || tags::write(&target, &details)).await;
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();

    match written {
        Ok(Ok(tags::Tagged::Written)) => logs::debug(format!(
            "{name}: tagged as \"{}\" from \"{}\"",
            episode.title, show.title
        )),

        // Not every container carries ID3 — an .m4a keeps its metadata in an
        // atom of its own — and forcing a tag onto one would break the file.
        //
        // Worth saying out loud, once: for this podcast the file name is all
        // there is, and a folder of bare timestamps with nothing inside them to
        // explain it would otherwise look like the run had lost something.
        Ok(Ok(tags::Tagged::Unsupported)) => {
            logs::debug(format!("{name}: not a container that takes ID3 tags"));
            if !show.said_untaggable.swap(true, Ordering::Relaxed) {
                ctx.log(format!(
                    "{}: these episodes aren't in a format that takes ID3 tags, so they \
                     are named by publication time and nothing else",
                    show.title
                ));
            }
        }

        // The episode is here and plays; only the description of it was lost.
        // Said as a failure because it is one, and said plainly because
        // running again will not put it right — the file is complete, so the
        // next run skips it. Deleting it is what asks for another try.
        Ok(Err(e)) => ctx.problem(format!(
            "{name}: downloaded, but could not write its tags: {e}. \
             Delete it and run again to try once more."
        )),
        Err(e) => ctx.problem(format!("{name}: could not write its tags: {e}")),
    }
}

/// Where an episode is written while it is still arriving: the file it will
/// become, with `.part` on the end.
///
/// `episode.mp3` becomes `episode.mp3.part` rather than `episode.part`, so two
/// episodes whose names differ only by extension can be in flight at once and
/// so the original extension survives for anything reading the leftovers.
///
/// Appended rather than gone through `with_extension`, which would have to be
/// handed the old extension and the new one glued together — and produces
/// `episode..part` for a name that has none.
fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// Sweep up the half-finished files a stopped run left behind.
///
/// A `.part` file is the tail end of a transfer that was interrupted, and while
/// one can be resumed, a run the user stopped is one they wanted to stop —
/// leaving several gigabytes of unplayable fragments on their disk to make that
/// point is not a kindness. So stopping takes them with it, and only stopping
/// does: a run that ends on its own leaves any partial from a failed episode
/// exactly where it is, for the next run to pick up from.
///
/// Called after every download task has returned, so nothing here is deleting a
/// file another task still holds open — which on Windows would simply fail.
/// Deletions are best-effort for the same reason logging is: a file that won't
/// go away is worth a line in the log and nothing more.
async fn collect_garbage(ctx: &Ctx, leftovers: &[PathBuf]) {
    let mut files = 0usize;
    let mut bytes = 0u64;

    for path in leftovers {
        let Ok(size) = tokio::fs::metadata(path).await.map(|m| m.len()) else {
            continue;
        };
        match tokio::fs::remove_file(path).await {
            Ok(()) => {
                logs::debug(format!(
                    "swept up {} ({})",
                    path.display(),
                    util::human_bytes(size)
                ));
                files += 1;
                bytes += size;
            }
            Err(e) => logs::debug(format!("could not sweep up {}: {e}", path.display())),
        }
    }

    if files > 0 {
        ctx.log(format!(
            "Cleared away {files} unfinished file{} ({})",
            if files == 1 { "" } else { "s" },
            util::human_bytes(bytes)
        ));
    }
}

/// Build a name that hasn't been handed out yet, appending ` (2)`, ` (3)` ... on
/// collision. `ext` may be empty for folder names.
fn unique_name(stem: &str, ext: &str, taken: &mut Vec<String>) -> String {
    let assemble = |stem: &str| {
        if ext.is_empty() {
            stem.to_string()
        } else {
            format!("{stem}.{ext}")
        }
    };

    let mut candidate = assemble(stem);
    let mut n = 2;
    // Case-insensitive, because macOS and Windows filesystems usually are.
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&candidate)) {
        candidate = assemble(&format!("{stem} ({n})"));
        n += 1;
    }
    taken.push(candidate.clone());
    candidate
}

/// reqwest's Display includes the full URL, which makes for very wide UI rows.
fn short_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "could not connect".to_string()
    } else if let Some(status) = e.status() {
        format!("server said {status}")
    } else if e.is_decode() {
        "malformed response".to_string()
    } else {
        "network error".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};

    /// How `/dribble.mp3` is paid out: twenty-five pieces a tenth of a second
    /// apart, so reading the whole episode takes about two and a half seconds
    /// however fast the machine running the test is — the waiting is the server
    /// sleeping, so a slow runner does not shorten it. The timings in
    /// `stopping_a_run_sweeps_up_the_file_it_was_part_way_through` are read off
    /// these two numbers.
    const DRIBBLE_CHUNKS: usize = 25;
    const DRIBBLE_GAP: Duration = Duration::from_millis(100);

    /// The bytes every test episode is made of. Deterministic so a resumed
    /// download can be checked byte for byte against what it should have been.
    ///
    /// It opens with an MPEG frame sync because that is how the tagger tells an
    /// MP3 from something merely called one; without it these episodes would go
    /// untagged and the tests would be exercising a path real episodes don't
    /// take.
    fn media() -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFB, 0x90, 0x00];
        bytes.extend((0..100_000u32).map(|i| (i % 251) as u8));
        bytes
    }

    /// A single-purpose HTTP server, so the download path is exercised over a
    /// real socket rather than a mock. Serves the feeds and the media files, and
    /// honours `Range` so resuming can be tested too.
    ///
    /// Feeds go out gzipped whenever the client says it takes gzip, which
    /// reqwest always does — because that is what real feed hosts do, and a
    /// test server that serves plain XML is a test server that cannot see the
    /// difference between the two.
    ///
    /// Detached: the thread lives until the test binary exits, which is soon.
    fn start_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let base = format!("http://{addr}");

        let feed = format!(
            r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Ignored</title>
  <item>
    <title>Episode One</title>
    <pubDate>Tue, 05 Aug 2025 10:00:00 GMT</pubDate>
    <enclosure url="{base}/one.mp3" length="100000" type="audio/mpeg"/>
  </item>
  <item>
    <title>Episode: Two?</title>
    <enclosure url="{base}/two" type="audio/mpeg"/>
  </item>
  <item>
    <title>Gone</title>
    <enclosure url="{base}/missing.mp3" type="audio/mpeg"/>
  </item>
</channel></rss>"#
        );

        // One episode, served slowly enough that timing it means something.
        let probe_feed = format!(
            r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Ignored</title>
  <item>
    <title>Slow One</title>
    <enclosure url="{base}/slow.mp3" length="100000" type="audio/mpeg"/>
  </item>
</channel></rss>"#
        );

        // One episode from a host that only serves the real bytes to a range
        // request — see `/resume.mp3`.
        let resume_feed = format!(
            r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Ignored</title>
  <item>
    <title>Episode One</title>
    <pubDate>Tue, 05 Aug 2025 10:00:00 GMT</pubDate>
    <enclosure url="{base}/resume.mp3" length="100000" type="audio/mpeg"/>
  </item>
</channel></rss>"#
        );

        // One episode served slowly enough that a run can be stopped while it is
        // still arriving, which is the only way to catch a `.part` file in the
        // act of existing.
        let dribble_feed = format!(
            r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Ignored</title>
  <item>
    <title>Episode One</title>
    <pubDate>Tue, 05 Aug 2025 10:00:00 GMT</pubDate>
    <enclosure url="{base}/dribble.mp3" length="100000" type="audio/mpeg"/>
  </item>
</channel></rss>"#
        );

        let feeds: Vec<(String, String)> = vec![
            ("/feed.xml".to_string(), feed.clone()),
            ("/slow.xml".to_string(), feed),
            ("/probe.xml".to_string(), probe_feed),
            ("/resume.xml".to_string(), resume_feed),
            ("/dribble.xml".to_string(), dribble_feed),
        ];

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let feeds = feeds.clone();
                std::thread::spawn(move || handle(stream, &feeds));
            }
        });

        base
    }

    fn handle(mut stream: TcpStream, feeds: &[(String, String)]) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

        // Headers, so we can see a Range request and what encodings are taken.
        //
        // Matched lowercased, because that is how they arrive: hyper writes
        // every header name in lower case, so a server looking for `Range:`
        // finds nothing, quietly serves the whole file, and lets a test that
        // means to prove resumption prove only that starting over also works.
        let mut range_from = None;
        let mut takes_gzip = false;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            let line = line.to_ascii_lowercase();
            if let Some(value) = line.strip_prefix("range: bytes=") {
                range_from = value.trim().trim_end_matches('-').parse::<usize>().ok();
            }
            if let Some(value) = line.strip_prefix("accept-encoding: ") {
                takes_gzip = value.contains("gzip");
            }
        }

        let respond = |stream: &mut TcpStream, head: String, body: &[u8]| {
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        };

        // A feed that takes its time, so a test can stop a run while a fetch is
        // still in flight.
        if path == "/slow.xml" {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }

        let serve_feed = |stream: &mut TcpStream, xml: &str| {
            let (body, encoding) = if takes_gzip {
                (gzip(xml.as_bytes()), "Content-Encoding: gzip\r\n")
            } else {
                (xml.as_bytes().to_vec(), "")
            };
            respond(
                stream,
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\n{encoding}\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ),
                &body,
            );
        };

        if let Some((_, xml)) = feeds.iter().find(|(name, _)| name == &path) {
            serve_feed(&mut stream, xml);
            return;
        }

        match path.as_str() {
            // Only a range request gets the real bytes. A client that starts
            // over from zero is served rubbish of the right length instead, so
            // a test asserting the file is correct is asserting that it
            // resumed — and not merely that downloading it twice also works.
            "/resume.mp3" => {
                let body = media();
                match range_from {
                    Some(from) if from < body.len() => {
                        let slice = &body[from..];
                        respond(
                            &mut stream,
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                                 Content-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                                slice.len(),
                                from,
                                body.len() - 1,
                                body.len()
                            ),
                            slice,
                        );
                    }
                    _ => respond(
                        &mut stream,
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                             Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len()
                        ),
                        &vec![0u8; body.len()],
                    ),
                }
            }
            // The same bytes again, this time spread over a second and a half so
            // that a run stopped part way through is genuinely stopped part way
            // through. Range requests are ignored: every reader gets the whole
            // file from the start, slowly.
            "/dribble.mp3" => {
                let body = media();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let chunk = body.len().div_ceil(DRIBBLE_CHUNKS);
                for piece in body.chunks(chunk) {
                    if stream.write_all(piece).is_err() || stream.flush().is_err() {
                        return;
                    }
                    std::thread::sleep(DRIBBLE_GAP);
                }
            }
            // The same bytes as any other episode, dribbled out in two halves
            // so that timing the transfer measures something above the noise.
            "/slow.mp3" => {
                let body = media();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                     Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let (first, second) = body.split_at(body.len() / 2);
                let _ = stream.write_all(first);
                let _ = stream.flush();
                std::thread::sleep(std::time::Duration::from_millis(40));
                let _ = stream.write_all(second);
                let _ = stream.flush();
            }
            "/one.mp3" | "/two" => {
                let body = media();
                match range_from {
                    Some(from) if from < body.len() => {
                        let slice = &body[from..];
                        respond(
                            &mut stream,
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                                 Content-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                                slice.len(),
                                from,
                                body.len() - 1,
                                body.len()
                            ),
                            slice,
                        );
                    }
                    _ => respond(
                        &mut stream,
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                             Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                            body.len()
                        ),
                        &body,
                    ),
                }
            }
            _ => respond(
                &mut stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
                b"",
            ),
        }
    }

    /// Wrap bytes in a gzip stream, using deflate's "stored" blocks so nothing
    /// is actually compressed.
    ///
    /// A real host would compress; what matters to the code under test is that
    /// the response arrives `Content-Encoding: gzip` and has to be inflated,
    /// because that is what makes reqwest drop the length — the trap the speed
    /// probe exists to sidestep. Stored blocks are valid gzip and need no
    /// compression crate to produce.
    fn gzip(data: &[u8]) -> Vec<u8> {
        // Magic, deflate, no flags, no mtime, no extra flags, unknown OS.
        let mut out = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff];

        let mut chunks = data.chunks(0xffff).peekable();
        if chunks.peek().is_none() {
            out.extend_from_slice(&[0x01, 0, 0, 0xff, 0xff]);
        }
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            out.push(u8::from(last)); // BFINAL, with BTYPE 00 for stored
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }

        out.extend_from_slice(&crc32(data).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    /// A temp directory that cleans up after itself.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("podbatch-{tag}-{nanos}"));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run the engine to completion and hand back everything it reported. The
    /// confirmation is given up front, which is what a user clicking Download
    /// in the box amounts to.
    fn run_engine(settings: Settings) -> Vec<Update> {
        run_engine_with(settings, None, agreed(), Skips::new())
    }

    /// As above, but stopping the run after `delay`.
    fn run_engine_cancelling_after(settings: Settings, delay: Option<Duration>) -> Vec<Update> {
        run_engine_with(settings, delay, agreed(), Skips::new())
    }

    /// As above, with some of the episodes un-ticked before the confirmation is
    /// given — which is the state the window leaves behind when someone picks
    /// through the list and then presses Download.
    fn run_engine_without(settings: Settings, unticked: &[(usize, usize)]) -> Vec<Update> {
        let skips = Skips::new();
        skips.set(unticked.iter().copied().collect());
        run_engine_with(settings, None, agreed(), skips)
    }

    /// A confirmation that has already been given.
    fn agreed() -> Proceed {
        let proceed = Proceed::new();
        proceed.go();
        proceed
    }

    fn run_engine_with(
        settings: Settings,
        cancel_after: Option<Duration>,
        proceed: Proceed,
        skips: Skips,
    ) -> Vec<Update> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");

        let cancel = Cancel::new();
        if let Some(delay) = cancel_after {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                cancel.cancel();
            });
        }

        runtime.block_on(run(settings, tx, cancel, proceed, skips, Arc::new(|| {})));

        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }
        updates
    }

    /// Built by parsing a real OPML file, so the two halves the window joins up
    /// — the parser and the downloader — are still tested against each other.
    fn settings_for(dir: &Path, base: &str, skip_existing: bool) -> Settings {
        let opml = dir.join("subs.opml");
        std::fs::write(
            &opml,
            format!(
                r#"<opml version="1.0"><body>
                    <outline type="rss" text="My Show" xmlUrl="{base}/feed.xml"/>
                   </body></opml>"#
            ),
        )
        .expect("write opml");

        Settings {
            subscriptions: opml::parse_file(&opml).expect("parse opml"),
            out_dir: dir.join("out"),
            concurrency: 2,
            limit: None,
            skip_existing,
        }
    }

    /// The plan the run stopped to ask about.
    fn plan_of(updates: &[Update]) -> Plan {
        updates
            .iter()
            .find_map(|u| match u {
                Update::Planned(plan) => Some(plan.clone()),
                _ => None,
            })
            .expect("every run that reads its feeds reports a plan")
    }

    /// The statuses each episode ended on, keyed by file name.
    fn outcomes(updates: &[Update]) -> Vec<(String, EpisodeStatus)> {
        let mut names: Vec<String> = Vec::new();
        for update in updates {
            if let Update::Episodes { episodes, .. } = update {
                names = episodes.iter().map(|e| e.file_name.clone()).collect();
            }
        }

        let mut out: Vec<(String, EpisodeStatus)> = Vec::new();
        for update in updates {
            if let Update::EpisodeStatus { episode, status, .. } = update {
                // Only the terminal status matters; Downloading is transient.
                if matches!(status, EpisodeStatus::Downloading) {
                    continue;
                }
                let name = names.get(*episode).cloned().unwrap_or_default();
                out.retain(|(n, _)| n != &name);
                out.push((name, status.clone()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The media, as it is once the episode has been tagged: the tag goes on
    /// the front, and every byte that was downloaded is still behind it.
    fn assert_is_the_episode(path: &Path) {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(
            bytes.ends_with(&media()),
            "{} is not the episode that was downloaded",
            path.display()
        );
    }

    #[test]
    fn downloads_every_episode_into_a_folder_per_podcast() {
        let base = start_server();
        let dir = TempDir::new("download");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");

        let updates = run_engine(settings.clone());

        // The name is the minute the episode was published. The second has no
        // pubDate to make one from so it falls back to its title, and its
        // extension comes from the MIME type because the URL has none.
        let first = show.join("050825-1000.mp3");
        let second = show.join("Episode Two.mp3");

        assert_is_the_episode(&first);
        assert_is_the_episode(&second);

        // The 404 leaves no file behind, not a zero-byte one.
        assert!(!show.join("Gone.mp3").exists());
        assert_eq!(
            outcomes(&updates),
            vec![
                ("050825-1000.mp3".to_string(), EpisodeStatus::Done),
                ("Episode Two.mp3".to_string(), EpisodeStatus::Done),
                ("Gone.mp3".to_string(), EpisodeStatus::Failed("server said 404 Not Found".into())),
            ]
        );

        // The name says only when; everything else about the episode has to be
        // in the file, or the run has thrown it away.
        let tag = id3::Tag::read_from_path(&first).expect("tags on the episode");
        use id3::TagLike;
        assert_eq!(tag.title(), Some("Episode One"));
        assert_eq!(tag.album(), Some("My Show"));
        assert_eq!(tag.year(), Some(2025));

        // No half-finished files are left lying around.
        let leftovers = leftover_parts(&show);
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");

        // Second run over the same folder downloads nothing again.
        let again = outcomes(&run_engine(settings));
        assert_eq!(again[0].1, EpisodeStatus::Skipped);
        assert_eq!(again[1].1, EpisodeStatus::Skipped);
    }

    /// An episode the user un-ticked is not fetched, and the ones either side of
    /// it still are. The whole point of picking through the list is that it is
    /// per episode and not per podcast.
    #[test]
    fn an_unticked_episode_is_left_alone_and_the_rest_still_run() {
        let base = start_server();
        let dir = TempDir::new("unticked");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");

        // The middle of the three, which is the one with no pubDate and so the
        // one named after its title.
        let updates = run_engine_without(settings, &[(0, 1)]);

        assert_is_the_episode(&show.join("050825-1000.mp3"));
        assert!(
            !show.join("Episode Two.mp3").exists(),
            "an un-ticked episode was downloaded anyway"
        );

        assert_eq!(
            outcomes(&updates),
            vec![
                ("050825-1000.mp3".to_string(), EpisodeStatus::Done),
                ("Episode Two.mp3".to_string(), EpisodeStatus::Unticked),
                ("Gone.mp3".to_string(), EpisodeStatus::Failed("server said 404 Not Found".into())),
            ]
        );
    }

    /// Un-ticking everything a podcast had is not the same as a run that fetched
    /// it: nothing is downloaded, and — because the folders are made by the pass
    /// that fetches — no empty folder is left sitting there either.
    #[test]
    fn unticking_every_episode_leaves_no_folder_behind() {
        let base = start_server();
        let dir = TempDir::new("unticked-all");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");

        let updates = run_engine_without(settings, &[(0, 0), (0, 1), (0, 2)]);

        assert!(!show.exists(), "an empty folder was left behind");
        assert!(
            outcomes(&updates)
                .iter()
                .all(|(_, status)| *status == EpisodeStatus::Unticked),
            "{:?}",
            outcomes(&updates)
        );
        assert!(matches!(
            updates.last(),
            Some(Update::Finished { cancelled: false })
        ));
    }

    /// Resuming, proved rather than assumed.
    ///
    /// The episode comes from `/resume.mp3`, which serves the real bytes only
    /// to a range request and a block of zeros to anyone starting from the
    /// beginning. So the file being right at the end is the assertion that the
    /// range request was made and honoured — with a server that simply serves
    /// the file either way, this test passes just as happily when nothing
    /// resumes at all.
    #[test]
    fn resumes_a_part_file_instead_of_starting_over() {
        let base = start_server();
        let dir = TempDir::new("resume");
        let mut settings = settings_for(&dir.0, &base, true);
        settings.subscriptions = vec![opml::Subscription {
            title: "My Show".into(),
            url: format!("{base}/resume.xml"),
        }];
        let show = settings.out_dir.join("My Show");
        std::fs::create_dir_all(&show).expect("create show dir");

        // Half an episode, as an interrupted run would have left it.
        let media = media();
        let part = show.join("050825-1000.mp3.part");
        std::fs::write(&part, &media[..40_000]).expect("write part");

        run_engine(settings);

        // The resumed half plus the fetched half is the whole file, in order.
        assert_is_the_episode(&show.join("050825-1000.mp3"));
        assert!(!part.exists(), "the .part file should have been renamed away");
    }

    /// Stopping a run takes the half-finished file with it.
    ///
    /// The episode comes down over about a second and a half, and the run is
    /// stopped while it is still arriving — so there is a real `.part` file,
    /// with real bytes in it, open on a real socket at the moment the user says
    /// stop. What the sweep has to leave behind is nothing at all.
    ///
    /// The timings come from `DRIBBLE_CHUNKS` and `DRIBBLE_GAP`. The speed probe
    /// reads the same slow route first and gives up at `PROBE_TIME`, so the
    /// download cannot start before ~1.5s, and at a tenth of a second per piece
    /// it cannot finish before ~4s. Stopping at 2.5s therefore lands about a
    /// second into the transfer with a second and a half to spare after it —
    /// margins a slow machine only widens, since every wait here is the server
    /// sleeping rather than the client working.
    #[test]
    fn stopping_a_run_sweeps_up_the_file_it_was_part_way_through() {
        let base = start_server();
        let dir = TempDir::new("sweep-inflight");
        let mut settings = settings_for(&dir.0, &base, true);
        settings.subscriptions = vec![opml::Subscription {
            title: "My Show".into(),
            url: format!("{base}/dribble.xml"),
        }];
        let show = settings.out_dir.join("My Show");

        let updates =
            run_engine_cancelling_after(settings, Some(Duration::from_millis(2500)));

        assert!(matches!(
            updates.last(),
            Some(Update::Finished { cancelled: true })
        ));
        // Stopped mid-transfer, so this is a sweep of something that was there.
        assert!(
            !show.join("050825-1000.mp3").exists(),
            "the episode finished; this test proves nothing about the sweep"
        );
        // And it says so, which is also how the test knows there was a real
        // half-written file to sweep rather than nothing to do.
        let said: Vec<&String> = updates
            .iter()
            .filter_map(|u| match u {
                Update::Log(line) if line.contains("Cleared away") => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(
            said.len(),
            1,
            "expected one line about the sweep, got {said:?}"
        );
        assert!(said[0].starts_with("Cleared away 1 unfinished file ("), "{said:?}");
        assert_eq!(
            leftover_parts(&show),
            Vec::<String>::new(),
            "stopping left the half-finished file behind"
        );
    }

    /// And a fragment left by some earlier interrupted run goes the same way,
    /// even when this run never downloads a byte itself.
    ///
    /// Turning down the confirmation is stopping — the same code path, reached
    /// with the Cancel button rather than the Stop one — so the tidy-up it
    /// promises has to happen there too, and it has to reach the leftovers of
    /// runs that ended before the sweep existed.
    #[test]
    fn a_run_turned_down_at_the_question_sweeps_up_what_was_already_there() {
        let base = start_server();
        let dir = TempDir::new("sweep-declined");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");
        std::fs::create_dir_all(&show).expect("create show dir");

        // Half an episode, as an interrupted run used to leave it.
        let part = show.join("050825-1000.mp3.part");
        std::fs::write(&part, &media()[..40_000]).expect("write part");

        // Never agreed to, and stopped shortly after the feeds are read.
        let updates =
            run_engine_with(settings, Some(Duration::from_millis(600)), Proceed::new(), Skips::new());

        assert!(matches!(
            updates.last(),
            Some(Update::Finished { cancelled: true })
        ));
        assert!(!part.exists(), "the fragment outlived the run that was stopped");
    }

    /// A run that ends on its own is not a run anybody stopped, and it must not
    /// go through the folder deleting things — a `.part` file at that point
    /// belongs to an episode that failed, and it is what the next run resumes
    /// from.
    #[test]
    fn a_run_that_finishes_leaves_a_fragment_alone_to_be_resumed() {
        let base = start_server();
        let dir = TempDir::new("sweep-finished");
        let mut settings = settings_for(&dir.0, &base, true);
        settings.subscriptions = vec![opml::Subscription {
            title: "My Show".into(),
            url: format!("{base}/resume.xml"),
        }];
        let show = settings.out_dir.join("My Show");
        std::fs::create_dir_all(&show).expect("create show dir");

        let media = media();
        let part = show.join("050825-1000.mp3.part");
        std::fs::write(&part, &media[..40_000]).expect("write part");

        run_engine(settings);

        // Resumed and renamed into place rather than deleted, which is the same
        // thing `resumes_a_part_file_instead_of_starting_over` asserts — said
        // again here because the sweep is what would break it.
        assert_is_the_episode(&show.join("050825-1000.mp3"));
        assert!(!part.exists());
    }

    #[test]
    fn a_part_file_keeps_the_extension_it_will_end_up_with() {
        assert_eq!(
            part_path(Path::new("/tmp/show/050825-1000.mp3")),
            PathBuf::from("/tmp/show/050825-1000.mp3.part")
        );
        // A file with no extension still gets one, and picks up nothing else.
        assert_eq!(
            part_path(Path::new("/tmp/show/episode")),
            PathBuf::from("/tmp/show/episode.part")
        );
    }

    /// Everything in `dir` that is still only part of an episode.
    fn leftover_parts(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".part"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn re_downloads_when_the_file_on_disk_is_the_wrong_size() {
        let base = start_server();
        let dir = TempDir::new("mismatch");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");
        std::fs::create_dir_all(&show).expect("create show dir");

        // Truncated by an interrupted copy, but the feed declares 100000 bytes.
        let episode = show.join("050825-1000.mp3");
        std::fs::write(&episode, b"truncated").expect("write stub");

        let updates = run_engine(settings);

        assert_is_the_episode(&episode);
        assert_eq!(outcomes(&updates)[0].1, EpisodeStatus::Done);
    }

    /// Stopping a run is something the user asked for, not something that went
    /// wrong. A feed abandoned mid-fetch must not be reported as failed — which
    /// it was, in red, for every unread podcast in the list.
    #[test]
    fn stopping_a_run_does_not_report_the_unread_feeds_as_failures() {
        let base = start_server();
        let dir = TempDir::new("cancel");
        let mut settings = settings_for(&dir.0, &base, true);
        settings.subscriptions = vec![opml::Subscription {
            title: "Slow Show".into(),
            url: format!("{base}/slow.xml"),
        }];

        let updates =
            run_engine_cancelling_after(settings, Some(Duration::from_millis(300)));

        let failures: Vec<&FeedStatus> = updates
            .iter()
            .filter_map(|u| match u {
                Update::FeedStatus { status, .. } if matches!(status, FeedStatus::Failed(_)) => {
                    Some(status)
                }
                _ => None,
            })
            .collect();
        assert!(failures.is_empty(), "stopping reported failures: {failures:?}");

        // And it settles back to waiting rather than being left mid-fetch.
        let last = updates
            .iter()
            .filter_map(|u| match u {
                Update::FeedStatus { status, .. } => Some(status),
                _ => None,
            })
            .next_back();
        assert_eq!(last, Some(&FeedStatus::Pending));

        assert!(matches!(
            updates.last(),
            Some(Update::Finished { cancelled: true })
        ));
    }

    /// The whole point of the pause: nothing is fetched until the window says
    /// so, and a run that is stopped at the question downloads nothing at all.
    #[test]
    fn nothing_is_downloaded_until_the_run_is_agreed_to() {
        let base = start_server();
        let dir = TempDir::new("unconfirmed");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");

        // Never confirmed, and stopped shortly after the feeds are read.
        let updates = run_engine_with(
            settings,
            Some(Duration::from_millis(600)),
            Proceed::new(),
            Skips::new(),
        );

        assert_eq!(
            plan_of(&updates).episodes,
            3,
            "three episodes were waiting to be fetched"
        );

        assert!(matches!(
            updates.last(),
            Some(Update::Finished { cancelled: true })
        ));
        // Not so much as a folder: a pass that only reads the feeds has no
        // business writing anything, or turning the box down would still leave
        // an empty folder for every podcast that was ticked.
        assert!(
            !show.exists(),
            "planning left {} behind",
            show.display()
        );
    }

    /// The trap itself, pinned so it cannot be forgotten: a feed served gzipped
    /// — which is to say very nearly every feed — arrives with no length.
    ///
    /// reqwest inflates it on the way in and drops `Content-Length` when it
    /// does, so there is nothing to divide by, and the bytes handed back are
    /// the inflated ones rather than the ones that crossed the wire. Any
    /// attempt to time the line by timing a feed dies here, and it dies
    /// silently: no error, just no estimate.
    #[test]
    fn a_gzipped_feed_arrives_with_no_length_to_measure() {
        let base = start_server();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async {
            install_crypto_provider();
            let client = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .build()
                .expect("client");

            let resp = client
                .get(format!("{base}/feed.xml"))
                .send()
                .await
                .expect("fetch");

            // reqwest strips `Content-Encoding` along with the length once it
            // has inflated the body, so the response gives no sign it was ever
            // compressed. There is nothing here to notice the problem by.
            assert_eq!(resp.headers().get(reqwest::header::CONTENT_ENCODING), None);
            assert!(
                resp.content_length().is_none(),
                "a decoded body has no length — the whole reason the probe times an episode"
            );

            // And it really was compressed: the server sends a length, so an
            // uncompressed response would have kept one.
            assert!(resp.text().await.expect("body").contains("Episode One"));
        });
    }

    /// Where the estimate's speed comes from, and why it cannot come from the
    /// feeds.
    ///
    /// Feeds arrive gzipped from very nearly every host — this server sends
    /// them that way for exactly this reason — and reqwest inflates them on the
    /// way in, at which point the response has no length and the bytes handed
    /// back are several times the bytes that crossed the wire. Timing a feed
    /// therefore measures nothing, or measures a fiction. A slice of a real
    /// episode is the one honest measurement, and this is the test that says
    /// so: it fails, with `probed_rate: None`, against anything that goes back
    /// to timing the XML.
    #[test]
    fn the_line_is_measured_on_an_episode_rather_than_the_feed() {
        let base = start_server();
        let dir = TempDir::new("probe");
        let mut settings = settings_for(&dir.0, &base, true);
        settings.subscriptions = vec![opml::Subscription {
            title: "Slow Show".into(),
            url: format!("{base}/probe.xml"),
        }];

        let plan = plan_of(&run_engine(settings));
        assert_eq!(plan.episodes, 1);

        let rate = plan
            .probed_rate
            .expect("the probe should have timed the episode");
        assert!(rate.is_finite() && rate > 0.0, "nonsense rate: {rate}");
    }

    /// What the confirmation box is built from. The size has to be the size of
    /// the work left, or the box quotes half an hour for a run that has nothing
    /// to do but check thirty files it already has.
    #[test]
    fn the_plan_counts_only_what_is_left_to_fetch() {
        let base = start_server();
        let dir = TempDir::new("plan");
        let settings = settings_for(&dir.0, &base, true);

        let before = plan_of(&run_engine(settings.clone()));
        assert_eq!(before.episodes, 3);
        assert_eq!(before.skipped, 0);
        // One episode declares a length; the other two don't, and the 404 is
        // one of those.
        assert_eq!(before.bytes, 100_000);
        assert_eq!(before.unsized_episodes, 2);

        // Everything that could be fetched now has been.
        let after = plan_of(&run_engine(settings));
        assert_eq!(after.skipped, 2);
        assert_eq!(after.episodes, 1, "only the 404 is still outstanding");
        assert_eq!(after.bytes, 0);
    }

    /// A rate is only worth quoting if something was actually measured. A
    /// sample too small or too brief is timing the clock, not the line.
    #[test]
    fn a_rate_needs_a_sample_worth_measuring() {
        assert_eq!(Sample { bytes: 200, seconds: 0.5 }.rate(), None);
        assert_eq!(Sample { bytes: 500_000, seconds: 0.0 }.rate(), None);
        assert_eq!(Sample { bytes: 500_000, seconds: f64::NAN }.rate(), None);
        assert_eq!(Sample { bytes: 1_048_576, seconds: 1.0 }.rate(), Some(1_048_576.0));
    }

    #[test]
    fn unique_name_disambiguates() {
        let mut taken = Vec::new();
        assert_eq!(unique_name("Ep One", "mp3", &mut taken), "Ep One.mp3");
        assert_eq!(unique_name("Ep One", "mp3", &mut taken), "Ep One (2).mp3");
        // Case-insensitive: this collides with both names already handed out,
        // so it lands on (3) rather than reusing (2).
        assert_eq!(unique_name("EP ONE", "mp3", &mut taken), "EP ONE (3).mp3");
        assert_eq!(unique_name("Ep Two", "", &mut taken), "Ep Two");
        assert_eq!(unique_name("Ep Two", "", &mut taken), "Ep Two (2)");
    }
}
