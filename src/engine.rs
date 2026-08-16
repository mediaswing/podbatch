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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc::UnboundedSender, Semaphore};

use crate::feed;
use crate::opml;
use crate::util;

const USER_AGENT: &str = concat!("PodBatch/", env!("CARGO_PKG_VERSION"));
/// Network hiccups are common on podcast CDNs; a couple of retries turns most
/// of them into a blip rather than a failed episode.
const ATTEMPTS: u32 = 3;
/// How often a running download reports progress upward.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(120);

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
    Skipped,
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
    /// Bytes per second one connection managed while the feeds were read, if
    /// the fetches gave anything worth measuring. A rough gauge of the line,
    /// and the only one available before a single episode has been fetched.
    pub sampled_rate: Option<f64>,
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
    fn is_cancelled(&self) -> bool {
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

/// Shared context passed down to each feed/episode task.
struct Ctx {
    tx: UnboundedSender<Update>,
    notify: Arc<dyn Fn() + Send + Sync>,
    cancel: Cancel,
    proceed: Proceed,
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
}

/// Start a run on a background thread. Returns immediately.
pub fn spawn(
    settings: Settings,
    tx: UnboundedSender<Update>,
    cancel: Cancel,
    proceed: Proceed,
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
                    let _ = tx.send(Update::Log(format!("Could not start the downloader: {e}")));
                    let _ = tx.send(Update::Finished { cancelled: false });
                    notify();
                    return;
                }
            };
            runtime.block_on(run(settings, tx, cancel, proceed, notify));
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
            let _ = tx.send(Update::Log(format!("Could not create HTTP client: {e}")));
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
        ctx.log(format!(
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

    if ctx.cancel.is_cancelled() {
        abandon(&ctx, &reads);
        ctx.log("Stopped.".to_string());
        ctx.send(Update::Finished { cancelled: true });
        return;
    }

    let plan = summarise(&reads, concurrency);
    let outstanding = plan.episodes;
    ctx.send(Update::Planned(plan));

    // Nothing to fetch is nothing to agree to, so a run that has already got
    // everything finishes rather than stopping to ask about no work.
    if outstanding > 0 && !wait_for_go(&ctx).await {
        abandon(&ctx, &reads);
        ctx.log("Stopped.".to_string());
        ctx.send(Update::Finished { cancelled: true });
        return;
    }

    // Pass two: fetch it.
    futures_util::stream::iter(reads.into_iter().filter_map(|read| read.plan).map(|plan| {
        let ctx = Arc::clone(&ctx);
        async move { download_feed(ctx, plan).await }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<()>>()
    .await;

    let cancelled = ctx.cancel.is_cancelled();
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

/// Add up what the feeds came back with, into the description of the job that
/// the confirmation box is built from.
fn summarise(reads: &[FeedRead], concurrency: usize) -> Plan {
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

    let samples: Vec<Sample> = reads.iter().filter_map(|r| r.sample).collect();
    plan.sampled_rate = rate_from(&samples, concurrency);
    plan
}

/// The line speed the feed fetches suggest, in bytes per second.
///
/// Each sample is one feed body: the bytes on the wire and how long they took
/// once the response headers had arrived, so connecting, TLS and the server's
/// own thinking time are left out — those are per-request costs that say
/// nothing about how fast a hundred-megabyte episode will come down.
///
/// The samples are taken with as many fetches in flight as the downloads will
/// use, so multiplying by that number turns one connection's share into the
/// aggregate the run should manage. It is still a guess made from a few hundred
/// kilobytes of XML, which is why everything built on it is worded as one.
fn rate_from(samples: &[Sample], concurrency: usize) -> Option<f64> {
    let (bytes, seconds) = samples
        .iter()
        .filter(|s| s.is_usable())
        .fold((0u64, 0.0f64), |(b, s), sample| {
            (b + sample.bytes, s + sample.seconds)
        });

    (seconds > 0.0 && bytes > 0).then(|| (bytes as f64 / seconds) * concurrency as f64)
}

/// One feed's worth of "how fast did that come down".
#[derive(Debug, Clone, Copy)]
struct Sample {
    bytes: u64,
    seconds: f64,
}

impl Sample {
    /// Small or brief transfers measure the clock more than the connection.
    fn is_usable(&self) -> bool {
        self.bytes >= 4096 && self.seconds >= 0.005 && self.seconds.is_finite()
    }
}

/// What reading one feed produced: work to do, work that turned out to be
/// already done, and how fast the feed itself came down.
#[derive(Default)]
struct FeedRead {
    /// `None` when there is nothing to download from this podcast — it failed,
    /// it was abandoned, it lists no media, or every episode is already here.
    plan: Option<FeedPlan>,
    /// How many of its episodes were already on disk.
    skipped: usize,
    sample: Option<Sample>,
}

/// A feed that has been read, with the episodes it still owes and where each
/// one goes.
struct FeedPlan {
    index: usize,
    /// The episodes to download: the index each was announced under, what to
    /// fetch, and where it lands.
    episodes: Vec<(usize, feed::Episode, PathBuf)>,
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

    let (body, sample) = match fetch_feed(&ctx, &sub.url).await {
        Ok(fetched) => fetched,
        Err(e) => {
            // Stopping is not a failure. A feed abandoned mid-fetch goes back
            // to waiting rather than being reported — and coloured — as broken,
            // which is what every unread feed in the list would otherwise show
            // the moment the user pressed Stop.
            let status = if ctx.cancel.is_cancelled() {
                FeedStatus::Pending
            } else {
                ctx.log(format!("{}: {e}", sub.title));
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
            ctx.log(format!("{}: {msg}", sub.title));
            ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Failed(msg) });
            return FeedRead { sample, ..FeedRead::default() };
        }
    };

    // Feeds are conventionally newest-first, so "latest N" is simply the head.
    let mut episodes = parsed.episodes;
    if let Some(limit) = ctx.settings.limit {
        episodes.truncate(limit);
    }

    if episodes.is_empty() {
        ctx.log(format!("{}: no episodes with downloadable media", sub.title));
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Done });
        ctx.send(Update::Episodes { feed: index, episodes: Vec::new() });
        return FeedRead { sample, ..FeedRead::default() };
    }

    let dir = ctx.settings.out_dir.join(&folder);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        let msg = format!("cannot create folder: {e}");
        ctx.log(format!("{}: {msg}", sub.title));
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Failed(msg) });
        return FeedRead { sample, ..FeedRead::default() };
    }

    // Name every file before downloading anything, so concurrent tasks in this
    // feed can't race to the same path.
    let mut used: Vec<String> = Vec::new();
    let planned: Vec<(feed::Episode, String)> = episodes
        .into_iter()
        .map(|ep| {
            let ext = util::extension_for(&ep.url, ep.mime.as_deref());
            let stem = match &ep.date {
                Some(date) => format!("{date} - {}", util::sanitize(&ep.title)),
                None => util::sanitize(&ep.title),
            };
            let name = unique_name(&stem, &ext, &mut used);
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

    if to_fetch.is_empty() {
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Done });
        return FeedRead { plan: None, skipped, sample };
    }

    FeedRead {
        plan: Some(FeedPlan { index, episodes: to_fetch }),
        skipped,
        sample,
    }
}

/// Fetch one podcast's outstanding episodes. Everything here was decided in
/// `plan_feed`; this is only the moving of the bytes.
async fn download_feed(ctx: Arc<Ctx>, plan: FeedPlan) {
    let index = plan.index;
    ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Downloading });

    let concurrency = ctx.settings.concurrency.clamp(1, 16);
    futures_util::stream::iter(plan.episodes.into_iter().map(|(ep_index, ep, path)| {
        let ctx = Arc::clone(&ctx);
        async move {
            let status = download_episode(&ctx, index, ep_index, &ep, &path).await;
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
/// A size that doesn't match what the feed declares means the last run was
/// interrupted after the rename, or the publisher swapped the file; either way
/// it gets fetched again, and says so.
async fn already_downloaded(ctx: &Ctx, episode: &feed::Episode, path: &Path) -> Option<u64> {
    if !ctx.settings.skip_existing {
        return None;
    }

    let size = tokio::fs::metadata(path).await.ok()?.len();
    let complete = match episode.length {
        Some(declared) => declared == size,
        None => size > 0,
    };
    if complete {
        return Some(size);
    }

    ctx.log(format!(
        "{}: on-disk size {} doesn't match the feed's {}, downloading again",
        path.file_name().unwrap_or_default().to_string_lossy(),
        util::human_bytes(size),
        episode.length.map(util::human_bytes).unwrap_or_default()
    ));
    None
}

/// Fetch a feed, and time its body while we are there.
///
/// The timing starts once the response headers are in, so connecting, the
/// handshake and the server's own delay are left out of a measurement that is
/// meant to say how fast bytes move. The size comes from `Content-Length`
/// because that is what crossed the wire — the body itself may have arrived
/// compressed and been inflated on the way past.
async fn fetch_feed(ctx: &Ctx, url: &str) -> Result<(String, Option<Sample>), String> {
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
            let wire = resp.content_length();
            let started = Instant::now();
            let body = resp.text().await.map_err(|e| short_err(&e))?;
            let sample = wire.map(|bytes| Sample {
                bytes,
                seconds: started.elapsed().as_secs_f64(),
            });
            Ok((body, sample))
        }
        .await;

        match result {
            Ok(fetched) => return Ok(fetched),
            Err(e) => {
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

        match transfer(ctx, feed_index, ep_index, episode, path).await {
            Ok(status) => return status,
            Err(e) => {
                last = e;
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(600 * attempt as u64)).await;
                }
            }
        }
    }

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
    episode: &feed::Episode,
    path: &Path,
) -> Result<EpisodeStatus, String> {
    let _permit = ctx.permits.acquire().await.map_err(|_| "shutting down")?;

    let part = path.with_extension(format!(
        "{}.part",
        path.extension().unwrap_or_default().to_string_lossy()
    ));

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
            // Flush what we have so the next run can resume from here.
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

    ctx.send(Update::Progress {
        feed: feed_index,
        episode: ep_index,
        done,
        total: Some(done),
    });
    Ok(EpisodeStatus::Done)
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

    /// The bytes every test episode is made of. Deterministic so a resumed
    /// download can be checked byte for byte against what it should have been.
    fn media() -> Vec<u8> {
        (0..100_000u32).map(|i| (i % 251) as u8).collect()
    }

    /// A single-purpose HTTP server, so the download path is exercised over a
    /// real socket rather than a mock. Serves one feed and two media files, and
    /// honours `Range` so resuming can be tested too.
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

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let feed = feed.clone();
                std::thread::spawn(move || handle(stream, &feed));
            }
        });

        base
    }

    fn handle(mut stream: TcpStream, feed: &str) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

        // Headers, so we can see a Range request.
        let mut range_from = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Range: bytes=") {
                range_from = value.trim().trim_end_matches('-').parse::<usize>().ok();
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

        match path.as_str() {
            "/feed.xml" | "/slow.xml" => respond(
                &mut stream,
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    feed.len()
                ),
                feed.as_bytes(),
            ),
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
        run_engine_with(settings, None, agreed())
    }

    /// As above, but stopping the run after `delay`.
    fn run_engine_cancelling_after(settings: Settings, delay: Option<Duration>) -> Vec<Update> {
        run_engine_with(settings, delay, agreed())
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

        runtime.block_on(run(settings, tx, cancel, proceed, Arc::new(|| {})));

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

    #[test]
    fn downloads_every_episode_into_a_folder_per_podcast() {
        let base = start_server();
        let dir = TempDir::new("download");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");

        let updates = run_engine(settings.clone());

        // The date prefix comes from pubDate; the second episode has none, and
        // its extension comes from the MIME type because the URL has none.
        let first = show.join("2025-08-05 - Episode One.mp3");
        let second = show.join("Episode Two.mp3");

        assert_eq!(std::fs::read(&first).expect("first episode"), media());
        assert_eq!(std::fs::read(&second).expect("second episode"), media());

        // The 404 leaves no file behind, not a zero-byte one.
        assert!(!show.join("Gone.mp3").exists());
        assert_eq!(
            outcomes(&updates),
            vec![
                ("2025-08-05 - Episode One.mp3".to_string(), EpisodeStatus::Done),
                ("Episode Two.mp3".to_string(), EpisodeStatus::Done),
                ("Gone.mp3".to_string(), EpisodeStatus::Failed("server said 404 Not Found".into())),
            ]
        );

        // No half-finished files are left lying around.
        let leftovers: Vec<_> = std::fs::read_dir(&show)
            .expect("read show dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");

        // Second run over the same folder downloads nothing again.
        let again = outcomes(&run_engine(settings));
        assert_eq!(again[0].1, EpisodeStatus::Skipped);
        assert_eq!(again[1].1, EpisodeStatus::Skipped);
    }

    #[test]
    fn resumes_a_part_file_instead_of_starting_over() {
        let base = start_server();
        let dir = TempDir::new("resume");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");
        std::fs::create_dir_all(&show).expect("create show dir");

        // Half an episode, as an interrupted run would have left it.
        let media = media();
        let part = show.join("2025-08-05 - Episode One.mp3.part");
        std::fs::write(&part, &media[..40_000]).expect("write part");

        run_engine(settings);

        // The resumed half plus the fetched half is the whole file, in order.
        assert_eq!(
            std::fs::read(show.join("2025-08-05 - Episode One.mp3")).expect("episode"),
            media
        );
        assert!(!part.exists(), "the .part file should have been renamed away");
    }

    #[test]
    fn re_downloads_when_the_file_on_disk_is_the_wrong_size() {
        let base = start_server();
        let dir = TempDir::new("mismatch");
        let settings = settings_for(&dir.0, &base, true);
        let show = settings.out_dir.join("My Show");
        std::fs::create_dir_all(&show).expect("create show dir");

        // Truncated by an interrupted copy, but the feed declares 100000 bytes.
        let episode = show.join("2025-08-05 - Episode One.mp3");
        std::fs::write(&episode, b"truncated").expect("write stub");

        let updates = run_engine(settings);

        assert_eq!(std::fs::read(&episode).expect("episode"), media());
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
        );

        let plan = updates
            .iter()
            .find_map(|u| match u {
                Update::Planned(plan) => Some(plan.clone()),
                _ => None,
            })
            .expect("the feeds were read, so a plan was made");
        assert_eq!(plan.episodes, 3, "three episodes were waiting to be fetched");

        assert!(matches!(
            updates.last(),
            Some(Update::Finished { cancelled: true })
        ));
        // The folder may exist — planning creates it — but nothing landed in it.
        let files: Vec<_> = std::fs::read_dir(&show)
            .map(|entries| entries.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(files.is_empty(), "downloaded without being asked: {files:?}");
    }

    /// What the confirmation box is built from. The size has to be the size of
    /// the work left, or the box quotes half an hour for a run that has nothing
    /// to do but check thirty files it already has.
    #[test]
    fn the_plan_counts_only_what_is_left_to_fetch() {
        let base = start_server();
        let dir = TempDir::new("plan");
        let settings = settings_for(&dir.0, &base, true);

        let first = run_engine(settings.clone());
        let plan = |updates: &[Update]| {
            updates
                .iter()
                .find_map(|u| match u {
                    Update::Planned(plan) => Some(plan.clone()),
                    _ => None,
                })
                .expect("a plan")
        };

        let before = plan(&first);
        assert_eq!(before.episodes, 3);
        assert_eq!(before.skipped, 0);
        // One episode declares a length; the other two don't, and the 404 is
        // one of those.
        assert_eq!(before.bytes, 100_000);
        assert_eq!(before.unsized_episodes, 2);

        // Everything that could be fetched now has been.
        let after = plan(&run_engine(settings));
        assert_eq!(after.skipped, 2);
        assert_eq!(after.episodes, 1, "only the 404 is still outstanding");
        assert_eq!(after.bytes, 0);
    }

    /// A rate is only worth quoting if something was actually measured, and the
    /// samples too small to mean anything have to be thrown away rather than
    /// averaged in.
    #[test]
    fn a_rate_needs_samples_worth_measuring() {
        let tiny = Sample { bytes: 200, seconds: 0.5 };
        let instant = Sample { bytes: 500_000, seconds: 0.0 };
        assert_eq!(rate_from(&[], 4), None);
        assert_eq!(rate_from(&[tiny, instant], 4), None);

        // 1 MB in a second, on each of four connections.
        let real = Sample { bytes: 1_048_576, seconds: 1.0 };
        assert_eq!(rate_from(&[real], 4), Some(4_194_304.0));
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
