//! The download engine.
//!
//! Runs on its own thread with a Tokio runtime and reports everything it does
//! back to the UI over a channel, so the GUI never blocks on I/O. The engine
//! knows nothing about egui; it just calls `notify` after each message so the
//! front end can wake up and repaint.

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

/// Shared context passed down to each feed/episode task.
struct Ctx {
    tx: UnboundedSender<Update>,
    notify: Arc<dyn Fn() + Send + Sync>,
    cancel: Cancel,
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
            runtime.block_on(run(settings, tx, cancel, notify));
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

    futures_util::stream::iter(subs.into_iter().zip(folders).enumerate().map(
        |(index, (sub, folder))| {
            let ctx = Arc::clone(&ctx);
            async move { process_feed(ctx, index, sub, folder).await }
        },
    ))
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

async fn process_feed(ctx: Arc<Ctx>, index: usize, sub: opml::Subscription, folder: String) {
    if ctx.cancel.is_cancelled() {
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Pending });
        return;
    }

    ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Fetching });

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
                ctx.log(format!("{}: {e}", sub.title));
                FeedStatus::Failed(e)
            };
            ctx.send(Update::FeedStatus { feed: index, status });
            return;
        }
    };

    let parsed = match feed::parse(&body) {
        Ok(f) => f,
        Err(e) => {
            let msg = e.to_string();
            ctx.log(format!("{}: {msg}", sub.title));
            ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Failed(msg) });
            return;
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
        return;
    }

    let dir = ctx.settings.out_dir.join(&folder);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        let msg = format!("cannot create folder: {e}");
        ctx.log(format!("{}: {msg}", sub.title));
        ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Failed(msg) });
        return;
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
    ctx.send(Update::FeedStatus { feed: index, status: FeedStatus::Downloading });

    let concurrency = ctx.settings.concurrency.clamp(1, 16);
    futures_util::stream::iter(planned.into_iter().enumerate().map(|(ep_index, (ep, name))| {
        let ctx = Arc::clone(&ctx);
        let path = dir.join(&name);
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

    // Already have it?
    if ctx.settings.skip_existing
        && let Ok(meta) = tokio::fs::metadata(path).await
    {
        let size = meta.len();
        let complete = match episode.length {
            // A size mismatch means the previous run was interrupted after the
            // rename, or the publisher replaced the file.
            Some(declared) => declared == size,
            None => size > 0,
        };
        if complete {
            ctx.send(Update::Progress {
                feed: feed_index,
                episode: ep_index,
                done: size,
                total: Some(size),
            });
            return EpisodeStatus::Skipped;
        }
        ctx.log(format!(
            "{}: on-disk size {} doesn't match the feed's {}, downloading again",
            path.file_name().unwrap_or_default().to_string_lossy(),
            util::human_bytes(size),
            episode.length.map(util::human_bytes).unwrap_or_default()
        ));
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

    /// Run the engine to completion and hand back everything it reported.
    fn run_engine(settings: Settings) -> Vec<Update> {
        run_engine_cancelling_after(settings, None)
    }

    /// As above, but stopping the run after `delay`.
    fn run_engine_cancelling_after(settings: Settings, delay: Option<Duration>) -> Vec<Update> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");

        let cancel = Cancel::new();
        if let Some(delay) = delay {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                cancel.cancel();
            });
        }

        runtime.block_on(run(settings, tx, cancel, Arc::new(|| {})));

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
