//! Turning downloaded episodes into readable transcripts.
//!
//! Three programs, in a line, none of them ours:
//!
//! ```text
//! episode.mp3 --ffmpeg--> 16 kHz mono wav --whisper-cli--> timestamped text
//!                                                             |
//!                                            [optional] --ollama--> stable speakers
//! ```
//!
//! FFmpeg is there because whisper.cpp reads 16-bit WAV and nothing else, and a
//! podcast is an mp3 or an m4a. Whisper does the listening. The `-tdrz` model
//! marks the moments the speaker changes — that is all it can do; it hears *a*
//! change, not *who* changed — so on its own the best the transcript can say is
//! that the voice has swapped, and turns alternate between two speakers.
//!
//! Ollama is what turns those turns into people. It never hears the audio; it
//! reads the turns Whisper already found and decides which of them are the same
//! voice, which is a bookkeeping job over text rather than a listening one. It
//! is optional throughout, and a machine without it still gets a full transcript
//! — see [`Settings::label_speakers`]. Nothing in here fails because Ollama is
//! absent, and nothing waits on it.
//!
//! Like the download engine, this runs on its own thread and reports over a
//! channel; the window never blocks on a subprocess.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;

use crate::document;
use crate::engine::Cancel;
use crate::logs;
use crate::tools;

/// 16 kHz, mono, 16-bit: what whisper.cpp expects, and what the byte count of
/// the converted file can therefore be turned back into a duration with.
const SAMPLE_RATE: u64 = 16_000;
const BYTES_PER_SECOND: u64 = SAMPLE_RATE * 2;

/// How many turns to hand Ollama at once, and how many of the previous chunk's
/// decisions to repeat as context.
///
/// A whole episode is far too much to put in one prompt and would be answered
/// worse even if it fitted. The overlap is what stops the numbering restarting
/// at each chunk boundary: the model is shown who was who a moment ago and asked
/// to carry on from there.
const CHUNK_TURNS: usize = 30;
const CHUNK_OVERLAP: usize = 4;

/// What one run of the tab was asked to do.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The audio files that were ticked, in the order they are listed.
    pub files: Vec<PathBuf>,
    pub ffmpeg: PathBuf,
    pub whisper: PathBuf,
    pub model: PathBuf,
    /// Whether to spend the extra time asking Ollama to keep a speaker's number
    /// attached to the same person. Off, or Ollama missing, means every turn is
    /// numbered afresh — which is still a transcript, just a blunter one.
    pub label_speakers: bool,
    /// Leave a file alone when its transcript is already sitting next to it.
    pub skip_existing: bool,
    /// What the transcript is written as, and how big and bold the text is.
    pub format: document::Format,
    pub style: document::Style,
}

/// Where a file has got to. `Working` carries which of the three programs is
/// running, because they take very different lengths of time and "Transcribing"
/// sitting there for ten minutes is only alarming if you think it means
/// "Converting".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Working(Stage),
    Done,
    Skipped,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Converting,
    Transcribing,
    Labelling,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Converting => "converting…",
            Stage::Transcribing => "transcribing…",
            Stage::Labelling => "naming speakers…",
        }
    }
}

#[derive(Debug)]
pub enum Update {
    Status { file: usize, status: Status },
    /// How far through the audio Whisper has got, 0.0 to 1.0.
    Progress { file: usize, fraction: f32 },
    /// A line of transcript as it appears, so the box fills while it works.
    ///
    /// Not tagged with the file it came from: the output box is one running
    /// column and the progress bar underneath already names the episode in
    /// hand, so the tag would have nowhere to be shown.
    Line { text: String },
    Log(String),
    Problem(String),
    Finished { cancelled: bool },
}

/// One stretch of speech between two `[SPEAKER_TURN]` marks.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub start: f64,
    pub text: String,
    /// Which person this is. Filled in by the numbering step; before that it is
    /// simply the turn's own position.
    pub speaker: usize,
}

struct Ctx {
    tx: UnboundedSender<Update>,
    notify: Arc<dyn Fn() + Send + Sync>,
    cancel: Cancel,
}

impl Ctx {
    fn send(&self, update: Update) {
        let _ = self.tx.send(update);
        (self.notify)();
    }
    fn log(&self, msg: impl Into<String>) {
        self.send(Update::Log(msg.into()));
    }
    fn problem(&self, msg: impl Into<String>) {
        self.send(Update::Problem(msg.into()));
    }
    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Start transcribing on a background thread. Returns immediately.
pub fn spawn(
    settings: Settings,
    tx: UnboundedSender<Update>,
    cancel: Cancel,
    notify: Arc<dyn Fn() + Send + Sync>,
) {
    std::thread::Builder::new()
        .name("podbatch-transcribe".into())
        .spawn(move || {
            let ctx = Ctx { tx, notify, cancel };
            run(&settings, &ctx);
        })
        .expect("spawn transcribe thread");
}

fn run(settings: &Settings, ctx: &Ctx) {
    // One scratch directory for the whole run, emptied at the end. The WAVs are
    // large — an hour of audio is a little over 100 MB once converted — and
    // leaving them behind would quietly fill a disk over a few runs.
    let scratch = match make_scratch_dir() {
        Ok(dir) => dir,
        Err(e) => {
            ctx.problem(format!("Nowhere to put the converted audio — {e}"));
            ctx.send(Update::Finished { cancelled: false });
            return;
        }
    };

    // Asked once, not once per file: the answer cannot change mid-run, and a
    // run that quietly stopped labelling half way would be worse than one that
    // never started.
    let labeller = if settings.label_speakers {
        match Labeller::available() {
            Some(l) => {
                ctx.log(format!(
                    "Speakers will be kept consistent using Ollama ({})",
                    tools::OLLAMA_MODEL
                ));
                Some(l)
            }
            None => {
                ctx.problem(
                    "Ollama is not answering, so speakers will be numbered turn by turn \
                     instead of being followed through the episode."
                        .to_string(),
                );
                None
            }
        }
    } else {
        None
    };

    for (index, path) in settings.files.iter().enumerate() {
        if ctx.cancelled() {
            break;
        }

        let destination = transcript_path(path, settings.format);
        if settings.skip_existing && destination.is_file() {
            ctx.send(Update::Status { file: index, status: Status::Skipped });
            ctx.log(format!("{} — already transcribed", name_of(path)));
            continue;
        }

        match transcribe_one(index, path, &destination, settings, labeller.as_ref(), &scratch, ctx) {
            Ok(()) => {
                ctx.send(Update::Status { file: index, status: Status::Done });
                ctx.log(format!("{} → {}", name_of(path), name_of(&destination)));
            }
            Err(e) => {
                // A cancelled run is not a failed file. Marking it failed would
                // leave the list claiming something went wrong with an episode
                // that was simply never reached.
                if ctx.cancelled() {
                    break;
                }
                ctx.send(Update::Status {
                    file: index,
                    status: Status::Failed(e.clone()),
                });
                ctx.problem(format!("{} — {e}", name_of(path)));
            }
        }
    }

    std::fs::remove_dir_all(&scratch).ok();
    ctx.send(Update::Finished { cancelled: ctx.cancelled() });
}

fn transcribe_one(
    index: usize,
    source: &Path,
    destination: &Path,
    settings: &Settings,
    labeller: Option<&Labeller>,
    scratch: &Path,
    ctx: &Ctx,
) -> Result<(), String> {
    ctx.send(Update::Status { file: index, status: Status::Working(Stage::Converting) });

    let wav = scratch.join(format!("{index}.wav"));
    convert(&settings.ffmpeg, source, &wav, ctx)?;

    let seconds = std::fs::metadata(&wav)
        .map(|m| m.len() as f64 / BYTES_PER_SECOND as f64)
        .unwrap_or(0.0);

    ctx.send(Update::Status { file: index, status: Status::Working(Stage::Transcribing) });
    let mut turns = listen(index, &settings.whisper, &settings.model, &wav, seconds, ctx)?;
    std::fs::remove_file(&wav).ok();

    if turns.is_empty() {
        return Err("no speech found".into());
    }

    if let Some(labeller) = labeller {
        ctx.send(Update::Status { file: index, status: Status::Working(Stage::Labelling) });
        match labeller.assign(&turns, ctx) {
            Ok(assigned) => turns = assigned,
            // A labelling failure costs the nice version of the answer, not the
            // answer: the turn-numbered transcript is already in hand and is
            // written out rather than thrown away over an optional extra.
            Err(e) => ctx.problem(format!(
                "Could not follow the speakers through {} ({e}) — numbering each turn instead",
                name_of(source)
            )),
        }
    }

    write_transcript(
        destination,
        source,
        &turns,
        labeller.is_some(),
        settings.format,
        settings.style,
    )
}

/// Step one: whatever the episode is, into the one format Whisper reads.
fn convert(ffmpeg: &Path, source: &Path, wav: &Path, ctx: &Ctx) -> Result<(), String> {
    let output = Command::new(ffmpeg)
        .args(["-nostdin", "-y", "-i"])
        .arg(source)
        .args(["-ar", &SAMPLE_RATE.to_string(), "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run FFmpeg — {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    // FFmpeg says why on stderr, and the last line of it is nearly always the
    // reason. The whole thing goes to the debug log; the window gets the line.
    let stderr = String::from_utf8_lossy(&output.stderr);
    logs::debug(format!("ffmpeg failed on {}: {stderr}", source.display()));
    ctx.log(format!("FFmpeg could not read {}", name_of(source)));
    Err(stderr
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("FFmpeg could not read it")
        .trim()
        .to_string())
}

/// Step two: the listening. Streams Whisper's output as it arrives so the
/// window fills up during the several minutes this takes.
fn listen(
    index: usize,
    whisper: &Path,
    model: &Path,
    wav: &Path,
    seconds: f64,
    ctx: &Ctx,
) -> Result<Vec<Turn>, String> {
    let mut child = Command::new(whisper)
        .arg("-m")
        .arg(model)
        .arg("-f")
        .arg(wav)
        // The flag this whole design rests on: without it the model never emits
        // a speaker mark and every episode comes out as one long block.
        .arg("-tdrz")
        // Greedy decoding rather than the default five-beam search. Measured on
        // three minutes of a two-host podcast: beam search found five speaker
        // turns where greedy found nine, on identical audio. The beams optimise
        // the sentence, and the speaker mark is a token that rarely helps the
        // sentence — so the better-scoring path is usually the one that drops
        // it. Transcription quality is near enough identical either way, and
        // greedy is the faster of the two.
        .args(["-bs", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run Whisper — {e}"))?;

    // Whisper writes its banner and any complaint to stderr while writing
    // segments to stdout. Drained on a thread of its own: a full stderr pipe
    // stops the process dead, and it would stop it part way through an episode
    // with no indication of why.
    let stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut kept = Vec::new();
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if kept.len() < 40 {
                    kept.push(line);
                }
            }
        }
        kept
    });

    let mut segments = Vec::new();
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if ctx.cancelled() {
                child.kill().ok();
                break;
            }
            let Some(segment) = parse_segment(&line) else {
                continue;
            };
            if seconds > 0.0 {
                ctx.send(Update::Progress {
                    file: index,
                    fraction: (segment.start / seconds).clamp(0.0, 1.0) as f32,
                });
            }
            ctx.send(Update::Line { text: segment.text.clone() });
            segments.push(segment);
        }
    }

    let status = child.wait().map_err(|e| format!("Whisper did not finish — {e}"))?;
    let stderr = errors.join().unwrap_or_default();

    if ctx.cancelled() {
        return Ok(Vec::new());
    }
    if !status.success() {
        logs::debug(format!("whisper failed: {}", stderr.join("\n")));
        return Err(stderr
            .iter()
            .rev()
            .find(|l| l.contains("error") || l.contains("failed"))
            .cloned()
            .unwrap_or_else(|| "Whisper stopped without transcribing".into()));
    }

    Ok(into_turns(segments))
}

/// One line of Whisper's output.
#[derive(Clone, Debug, PartialEq)]
struct Segment {
    start: f64,
    text: String,
    /// Whether the speaker changed at the end of this segment.
    turn_ends: bool,
}

/// Pull a segment out of a line like
/// `[00:01:02.000 --> 00:01:05.000]   Hello there. [SPEAKER_TURN]`.
///
/// Anything that is not shaped like that — the banner, the timing summary, a
/// blank line — returns `None` and is skipped, so the parser cannot be derailed
/// by Whisper printing something new around the edges.
fn parse_segment(line: &str) -> Option<Segment> {
    let rest = line.trim().strip_prefix('[')?;
    let (stamps, text) = rest.split_once(']')?;
    let (start, _) = stamps.split_once("-->")?;
    let start = parse_timestamp(start.trim())?;

    let text = text.trim();
    let (text, turn_ends) = match text.strip_suffix("[SPEAKER_TURN]") {
        Some(before) => (before.trim(), true),
        None => (text, false),
    };

    // A segment that is only a speaker mark still ends the turn, but has
    // nothing to add to it.
    Some(Segment {
        start,
        text: text.to_string(),
        turn_ends,
    })
}

/// `HH:MM:SS.mmm` to seconds.
fn parse_timestamp(stamp: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut parts = stamp.split(':');
    let hours: f64 = parts.next()?.trim().parse().ok()?;
    let minutes: f64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().replace(',', ".").parse().ok()?;
    total += hours * 3600.0 + minutes * 60.0 + seconds;
    Some(total)
}

/// Gather segments into turns, splitting where Whisper heard the speaker change.
///
/// Turns alternate between Speaker 1 and Speaker 2. All a turn mark tells us is
/// that the voice changed, so the only thing it forces is that this turn is not
/// the previous one — and with two voices, "not the previous one" has exactly
/// one answer. That makes alternating correct for the two-hander, which is most
/// podcasts, and it is why the numbering stops at two: with three voices "not
/// the previous one" has two answers and nothing here can choose between them.
/// Guessing a third would be inventing information.
///
/// An earlier version numbered every turn in sequence, which was defensible and
/// useless: an hour of conversation came out claiming 285 different people. If
/// there really are three or more, that is what [`Labeller`] is for — it reads
/// the words and can tell speakers apart in a way a turn mark cannot.
fn into_turns(segments: Vec<Segment>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut current: Option<Turn> = None;

    for segment in segments {
        if !segment.text.is_empty() {
            match current.as_mut() {
                Some(turn) => {
                    turn.text.push(' ');
                    turn.text.push_str(&segment.text);
                }
                None => {
                    current = Some(Turn {
                        start: segment.start,
                        text: segment.text.clone(),
                        speaker: turns.len() % 2 + 1,
                    })
                }
            }
        }
        if segment.turn_ends && let Some(turn) = current.take() {
            turns.push(turn);
        }
    }
    if let Some(turn) = current.take() {
        turns.push(turn);
    }
    turns
}

// ---- speakers -------------------------------------------------------------

/// The optional step: deciding which turns are the same person.
pub struct Labeller {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Labeller {
    /// A labeller, if Ollama is running and has the model. `None` is a perfectly
    /// ordinary answer and the caller carries on without one.
    pub fn available() -> Option<Self> {
        if !tools::ollama_running() {
            return None;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        Some(Self {
            client: reqwest::Client::new(),
            runtime,
        })
    }

    /// Re-number the turns so a number follows a person.
    ///
    /// Done in overlapping chunks: the model is shown how the last few turns
    /// were labelled and asked to keep going, which is what stops the numbering
    /// restarting every thirty turns.
    fn assign(&self, turns: &[Turn], ctx: &Ctx) -> Result<Vec<Turn>, String> {
        let mut out: Vec<Turn> = Vec::with_capacity(turns.len());

        let mut start = 0;
        while start < turns.len() {
            if ctx.cancelled() {
                return Err("stopped".into());
            }
            let end = (start + CHUNK_TURNS).min(turns.len());
            let context: Vec<&Turn> = out
                .iter()
                .rev()
                .take(CHUNK_OVERLAP)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();

            let assigned = self.assign_chunk(&context, &turns[start..end])?;
            for (turn, speaker) in turns[start..end].iter().zip(assigned) {
                out.push(Turn { speaker, ..turn.clone() });
            }
            start = end;
        }
        Ok(out)
    }

    /// One chunk. Returns a speaker number for each turn handed in.
    fn assign_chunk(&self, context: &[&Turn], chunk: &[Turn]) -> Result<Vec<usize>, String> {
        let mut prompt = String::new();
        prompt.push_str(
            "You are labelling the speakers in a podcast transcript. Each numbered line is \
             one uninterrupted turn by one person. Decide which turns are the same person.\n\n",
        );
        if !context.is_empty() {
            prompt.push_str("Already decided, keep these numbers meaning the same people:\n");
            for turn in context {
                prompt.push_str(&format!("Speaker {}: {}\n", turn.speaker, clip(&turn.text)));
            }
            prompt.push('\n');
        }
        prompt.push_str("Label these turns:\n");
        for (i, turn) in chunk.iter().enumerate() {
            prompt.push_str(&format!("{}. {}\n", i + 1, clip(&turn.text)));
        }
        // The wording here matters more than it looks. An earlier draft said
        // "consecutive turns are almost always different people", which is true
        // and useless: it pushed the model into numbering every turn 1, 2, 3,
        // 4 — precisely the per-turn numbering this step exists to replace.
        // What has to be said is the opposite half of the truth: the cast is
        // small and closed, so a number is there to be *reused*.
        prompt.push_str(&format!(
            "\nA podcast has only a handful of people in it and they take it in turns, so the \
             same numbers come round again and again. Give a turn the number you already gave \
             that person earlier; only use a new number when you are sure it is somebody new. \
             Two people talking to each other should come out 1, 2, 1, 2.\n\
             \nReply with JSON only: {{\"speakers\": [n1, n2, ...]}} with exactly {} whole \
             numbers, one per turn in order, starting at 1.\n",
            chunk.len()
        ));

        let body = serde_json::json!({
            "model": tools::OLLAMA_MODEL,
            "prompt": prompt,
            "stream": false,
            // Constrains the decoder to emit valid JSON, which turns "parse
            // whatever the model felt like saying" into an ordinary parse.
            "format": "json",
            "options": { "temperature": 0 },
        });

        // Serialised here rather than with reqwest's `json` helper: that needs
        // a reqwest feature this app does not otherwise build, and the body is
        // already a `serde_json::Value`.
        let body = serde_json::to_string(&body)
            .map_err(|e| format!("Could not build the request — {e}"))?;

        let url = format!("{}/api/generate", tools::OLLAMA_HOST);
        let response: String = self.runtime.block_on(async {
            let sent = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
                // Generous: a chunk on a small model on a busy machine is slow,
                // and giving up early would throw away work already done.
                .timeout(std::time::Duration::from_secs(180))
                .send()
                .await
                .map_err(|e| format!("Ollama did not answer — {e}"))?;
            sent.text()
                .await
                .map_err(|e| format!("Ollama's answer could not be read — {e}"))
        })?;

        parse_speakers(&response, chunk.len())
    }
}

/// Pull the speaker numbers out of Ollama's reply.
///
/// Two layers of JSON: the API's envelope, whose `response` field is itself a
/// JSON string because that is what we asked the model for. Anything that comes
/// back the wrong length is rejected rather than padded — a short list silently
/// filled in would put words in the wrong person's mouth, which is exactly the
/// failure this step exists to avoid.
fn parse_speakers(body: &str, expected: usize) -> Result<Vec<usize>, String> {
    let envelope: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("Ollama sent something unreadable — {e}"))?;

    if let Some(error) = envelope.get("error").and_then(|e| e.as_str()) {
        return Err(error.to_string());
    }

    let inner = envelope
        .get("response")
        .and_then(|r| r.as_str())
        .ok_or("Ollama's answer had no content")?;

    let parsed: serde_json::Value =
        serde_json::from_str(inner).map_err(|e| format!("Ollama's JSON did not parse — {e}"))?;

    let list = parsed
        .get("speakers")
        .and_then(|s| s.as_array())
        .ok_or("Ollama's answer had no speaker list")?;

    if list.len() != expected {
        return Err(format!(
            "Ollama labelled {} turns out of {expected}",
            list.len()
        ));
    }

    list.iter()
        .map(|v| {
            v.as_u64()
                .filter(|n| *n >= 1)
                .map(|n| n as usize)
                .ok_or_else(|| "Ollama gave a speaker that is not a number".to_string())
        })
        .collect()
}

/// Keep a turn short enough that thirty of them fit in a prompt. Who is speaking
/// is clear from the first sentence or two; the rest is just tokens.
fn clip(text: &str) -> String {
    const MAX: usize = 240;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX).collect();
    out.push('…');
    out
}

// ---- output ---------------------------------------------------------------

/// Where a transcript goes: beside the audio, same name, the chosen extension.
pub fn transcript_path(audio: &Path, format: document::Format) -> PathBuf {
    audio.with_extension(format.extension())
}

fn write_transcript(
    destination: &Path,
    source: &Path,
    turns: &[Turn],
    labelled: bool,
    format: document::Format,
    style: document::Style,
) -> Result<(), String> {
    let mut out = String::new();
    out.push_str(&format!("{}\n", name_of(source)));
    out.push_str(&format!(
        "Transcribed by {} {} — whisper.cpp, {}\n",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        tools::MODEL_FILE
    ));
    // Said in the file itself, because a transcript outlives the window that
    // made it and the difference between these two matters when you read it.
    out.push_str(if labelled {
        "Speakers followed through the episode by Ollama — a number should mean the same \
         person throughout. Check anything that matters.\n"
    } else {
        "Speakers alternate between two voices. Whisper heard where the voice changed but \
         not whose it is, so this is right for two people talking and wrong for three or \
         more — and one missed change swaps the two names from that point on.\n"
    });
    out.push('\n');

    for turn in turns {
        out.push_str(&format!(
            "[{}] Speaker {}: {}\n\n",
            clock(turn.start),
            turn.speaker,
            turn.text
        ));
    }

    document::write(destination, format, style, &out)
}

/// Seconds as `HH:MM:SS`, which is how you find your place in an episode.
fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", total / 3600, (total % 3600) / 60, total % 60)
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A scratch directory of our own, made fresh and readable only by us.
///
/// The name carries randomness rather than the process id. A pid is a small
/// number and a guessable one, and the temp directory is shared between users
/// on most Linux systems — not on macOS or Windows, where it is per-user, but
/// this app builds and runs on all three. A guessable name in a shared
/// directory is one somebody else can create first, and then an episode's audio
/// is converted into a folder of their choosing rather than ours.
///
/// [`std::fs::create_dir`] rather than `create_dir_all` is the other half of
/// that: it fails on a path that already exists, so a directory somebody
/// prepared in advance — or a symlink pointing at one they can read — is an
/// error rather than the place the WAVs quietly go. On Unix the mode is
/// narrowed to 0700 as well, because the default leaves a run's converted audio
/// world-readable for as long as the run lasts.
fn make_scratch_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let mut collision = None;

    // Eight attempts is seven more than should ever be needed; the loop is here
    // because the one thing we must not do on a name that is taken is use it.
    for _ in 0..8 {
        let path = base.join(format!("podbatch-transcribe-{:016x}", random_suffix()));
        match create_private_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => collision = Some(e),
            Err(e) => return Err(e),
        }
    }

    Err(collision
        .unwrap_or_else(|| std::io::Error::other("could not find an unused name")))
}

/// Make one directory, failing if it is already there, and on Unix keeping it
/// to ourselves.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        // Not `recursive`, so an existing path is the error it needs to be.
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

/// Sixty-four bits that cannot be guessed, without taking a dependency for it.
///
/// `RandomState` is the standard library's hash seed, which it takes from the
/// operating system — the property wanted here — and mixing the clock in as
/// well means two directories asked for in the same process are distinct even
/// if the seed somehow were not.
fn random_suffix() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// The audio files in a folder, sorted, ignoring anything we cannot feed to
/// FFmpeg.
///
/// One level only. A podcast library is a folder of folders, and walking the lot
/// would turn "open this show" into "transcribe everything you have ever
/// downloaded" — which is hours of work nobody asked for.
pub fn audio_in(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && crate::util::is_media(p))
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, text: &str, turn_ends: bool) -> Segment {
        Segment { start, text: text.into(), turn_ends }
    }

    /// Two runs must not be handed the same directory, and neither must a
    /// directory somebody else prepared: the name is unguessable and making it
    /// fails outright if it is already there.
    #[test]
    fn a_scratch_directory_is_new_and_ours_alone() {
        let first = make_scratch_dir().expect("a scratch directory");
        let second = make_scratch_dir().expect("a second scratch directory");
        assert_ne!(first, second, "two runs were given the same directory");

        // The name of one that already exists is refused rather than reused.
        assert_eq!(
            create_private_dir(&first).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::AlreadyExists)
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&first).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "the converted audio was left readable");
        }

        std::fs::remove_dir_all(&first).ok();
        std::fs::remove_dir_all(&second).ok();
    }

    #[test]
    fn reads_a_segment_line() {
        let line = "[00:00:02.000 --> 00:00:05.000]   Welcome back to the show.";
        assert_eq!(
            parse_segment(line),
            Some(seg(2.0, "Welcome back to the show.", false))
        );
    }

    #[test]
    fn a_speaker_mark_ends_the_turn_and_leaves_the_text_clean() {
        let line = "[00:01:02.500 --> 00:01:06.000]   Glad to be here. [SPEAKER_TURN]";
        let parsed = parse_segment(line).expect("a segment");
        assert_eq!(parsed.text, "Glad to be here.");
        assert!(parsed.turn_ends);
        assert!((parsed.start - 62.5).abs() < 1e-9);
    }

    #[test]
    fn anything_that_is_not_a_segment_is_ignored() {
        for line in [
            "",
            "whisper_init_from_file_with_params_no_state: loading model",
            "system_info: n_threads = 4",
            "[not a timestamp] hello",
        ] {
            assert_eq!(parse_segment(line), None, "{line:?} was taken for a segment");
        }
    }

    #[test]
    fn segments_gather_into_turns_at_the_speaker_marks() {
        let segments = vec![
            seg(0.0, "Welcome back.", false),
            seg(2.0, "Good to see you.", true),
            seg(4.0, "Glad to be here.", true),
            seg(6.0, "Let's get into it.", false),
        ];
        let turns = into_turns(segments);

        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].text, "Welcome back. Good to see you.");
        assert_eq!(turns[1].text, "Glad to be here.");
        assert_eq!(turns[2].text, "Let's get into it.");
        // Alternating, not counting up: the third turn is the first speaker
        // again, which for two voices is the only thing a turn mark can mean.
        assert_eq!(
            turns.iter().map(|t| t.speaker).collect::<Vec<_>>(),
            vec![1, 2, 1]
        );
    }

    /// The bug this replaced: an hour of two people talking came out claiming
    /// 285 of them, because every turn took the next number up.
    #[test]
    fn a_long_conversation_has_two_speakers_not_hundreds() {
        let segments: Vec<Segment> = (0..300)
            .map(|i| seg(i as f64, "Something said.", true))
            .collect();
        let turns = into_turns(segments);

        assert_eq!(turns.len(), 300, "every turn should survive");
        let mut speakers: Vec<usize> = turns.iter().map(|t| t.speaker).collect();
        speakers.sort_unstable();
        speakers.dedup();
        assert_eq!(speakers, vec![1, 2], "only two voices can be inferred");

        // And they alternate rather than clumping.
        assert_eq!(turns[0].speaker, 1);
        assert_eq!(turns[1].speaker, 2);
        assert_eq!(turns[2].speaker, 1);
    }

    #[test]
    fn a_trailing_turn_with_no_final_mark_is_still_kept() {
        let turns = into_turns(vec![seg(0.0, "The last word.", false)]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text, "The last word.");
    }

    #[test]
    fn reads_ollamas_speaker_list() {
        let body = r#"{"response":"{\"speakers\":[1,2,1,3]}","done":true}"#;
        assert_eq!(parse_speakers(body, 4).expect("speakers"), vec![1, 2, 1, 3]);
    }

    /// The important one. A short list quietly padded out would shift every
    /// following turn onto the wrong person.
    #[test]
    fn a_list_of_the_wrong_length_is_refused() {
        let body = r#"{"response":"{\"speakers\":[1,2]}","done":true}"#;
        let error = parse_speakers(body, 5).expect_err("should have been refused");
        assert!(error.contains("2 turns out of 5"), "{error}");
    }

    #[test]
    fn nonsense_from_ollama_is_an_error_rather_than_a_guess() {
        for body in [
            r#"{"response":"not json at all","done":true}"#,
            r#"{"response":"{\"speakers\":[0,1]}","done":true}"#,
            r#"{"response":"{\"speakers\":[\"one\",\"two\"]}","done":true}"#,
            r#"{"error":"model not found"}"#,
            "{}",
        ] {
            assert!(
                parse_speakers(body, 2).is_err(),
                "{body} was accepted"
            );
        }
    }

    /// The real thing, end to end, on a real episode.
    ///
    /// Ignored by default because it needs FFmpeg, Whisper and the model to be
    /// installed and takes a minute or so to run. Point it at an episode and
    /// run it when the pipeline itself is what changed:
    ///
    /// ```text
    /// PODBATCH_TEST_AUDIO="/path/to/episode.mp3" cargo test -- --ignored e2e
    /// ```
    ///
    /// Deliberately does not assert that any speaker turn was found: whether
    /// there are any is a property of the audio, not of this code. A narrated
    /// documentary yields none and that is the correct answer for it.
    #[test]
    #[ignore = "needs ffmpeg, whisper and the model, and a real episode"]
    fn e2e_transcribes_a_real_episode() {
        let Ok(source) = std::env::var("PODBATCH_TEST_AUDIO") else {
            panic!("set PODBATCH_TEST_AUDIO to an audio file");
        };
        let source = PathBuf::from(source);
        assert!(source.is_file(), "{} is not a file", source.display());

        // `PODBATCH_TEST_FORMAT=docx` etc., so the same run can be pointed at
        // whichever writer is being checked.
        let format = match std::env::var("PODBATCH_TEST_FORMAT").as_deref() {
            Ok("docx") => document::Format::Docx,
            Ok("pdf") => document::Format::Pdf,
            _ => document::Format::Text,
        };

        let ffmpeg = tools::find_program("ffmpeg").expect("ffmpeg");
        let whisper = tools::whisper_binary().expect("whisper");
        let model = tools::model_path();
        assert!(model.is_file(), "model missing at {}", model.display());

        // Into a scratch copy, so the test never writes beside the user's audio.
        let dir = std::env::temp_dir().join(format!("podbatch-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let audio = dir.join("episode.mp3");
        std::fs::copy(&source, &audio).expect("copy");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Cancel::new();
        spawn(
            Settings {
                files: vec![audio.clone()],
                ffmpeg,
                whisper,
                model,
                label_speakers: false,
                skip_existing: false,
                format,
                style: document::Style::default(),
            },
            tx,
            cancel,
            Arc::new(|| {}),
        );

        let mut finished = false;
        let mut problems = Vec::new();
        while let Some(update) = rx.blocking_recv() {
            match update {
                Update::Problem(p) => problems.push(p),
                Update::Finished { cancelled } => {
                    assert!(!cancelled);
                    finished = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(finished, "the run never finished");
        assert!(problems.is_empty(), "problems: {problems:?}");

        let transcript = transcript_path(&audio, format);
        let bytes = std::fs::read(&transcript).expect("a transcript");
        assert!(!bytes.is_empty(), "the transcript is empty");

        // Kept where it can be opened and looked at, which for a format written
        // by hand is the only check that really counts.
        let kept = std::env::temp_dir().join(format!("podbatch-e2e.{}", format.extension()));
        std::fs::copy(&transcript, &kept).expect("keep a copy");
        println!("wrote {} ({} bytes)", kept.display(), bytes.len());

        if format == document::Format::Text {
            let written = String::from_utf8(bytes).expect("utf8");
            assert!(written.contains("Speaker 1:"), "no speaker: {written:.400}");
            assert!(written.contains("[00:00:"), "no timestamps: {written:.400}");
            println!("--- transcript head ---\n{:.600}", written);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timestamps_become_a_clock() {
        assert_eq!(clock(0.0), "00:00:00");
        assert_eq!(clock(65.4), "00:01:05");
        assert_eq!(clock(3725.0), "01:02:05");
    }

    #[test]
    fn a_transcript_goes_next_to_its_audio() {
        assert_eq!(
            transcript_path(
                Path::new("/shows/ep/2024-01-01 Episode.mp3"),
                document::Format::Text
            ),
            PathBuf::from("/shows/ep/2024-01-01 Episode.txt")
        );
        assert_eq!(
            transcript_path(
                Path::new("/shows/ep/2024-01-01 Episode.mp3"),
                document::Format::Docx
            ),
            PathBuf::from("/shows/ep/2024-01-01 Episode.docx")
        );
    }

    #[test]
    fn only_audio_is_offered_for_transcription() {
        let dir = std::env::temp_dir().join(format!("podbatch-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in ["a.mp3", "b.m4a", "notes.txt", "cover.jpg", "c.flac"] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }

        let found: Vec<String> = audio_in(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, vec!["a.mp3", "b.m4a", "c.flac"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Whichever way it is written, the file says which kind of numbering it
    /// got — the two are not interchangeable and a reader has to be able to tell.
    #[test]
    fn the_transcript_says_which_kind_of_numbering_it_has() {
        let dir = std::env::temp_dir().join(format!("podbatch-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let turns = vec![Turn { start: 1.0, text: "Hello.".into(), speaker: 1 }];

        for (labelled, expected) in [
            (true, "same person throughout"),
            (false, "alternate between two voices"),
        ] {
            let out = dir.join("ep.txt");
            write_transcript(
                &out,
                Path::new("ep.mp3"),
                &turns,
                labelled,
                document::Format::Text,
                document::Style::default(),
            )
            .expect("write");
            let written = std::fs::read_to_string(&out).expect("read");
            assert!(written.contains(expected), "{written}");
            assert!(written.contains("[00:00:01] Speaker 1: Hello."), "{written}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
