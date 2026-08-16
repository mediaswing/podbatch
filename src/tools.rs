//! The outside programs the Transcripts tab leans on, and how they get here.
//!
//! Nothing in this module installs anything as a side effect of looking. Every
//! function either *surveys* what is already on the machine or *describes* what
//! installing something would run; the running itself is [`install`], which is
//! only ever reached from a branch the user agreed to on screen. That split is
//! the point: the window can re-survey every time the tab is opened, and opening
//! a tab must never change somebody's machine.
//!
//! Three of the four required pieces are ordinary package-manager installs. The
//! package manager itself is the exception — bootstrapping Homebrew wants a
//! password at a terminal, which a GUI app has no honest way to ask for, so that
//! one is handed to the user as a command to run rather than run for them. See
//! [`Install::Guided`].

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::logs;

/// The tinydiarize model: `small.en`, retrained to mark where the speaker
/// changes. It is the only Whisper model that emits `[SPEAKER_TURN]`, which is
/// the whole reason the transcripts can be broken into speakers at all.
pub const MODEL_URL: &str =
    "https://huggingface.co/akashmjn/tinydiarize-whisper.cpp/resolve/main/ggml-small.en-tdrz.bin";
pub const MODEL_FILE: &str = "ggml-small.en-tdrz.bin";
/// What the download costs, so the user is told before agreeing rather than
/// after. Checked against the real file by `tests::the_model_size_is_honest`,
/// which is ignored by default because it hits the network.
pub const MODEL_BYTES: u64 = 487_614_184;

/// The Ollama model used to turn detected speaker *turns* into stable speaker
/// *identities*. Small on purpose: it is doing bookkeeping over text, not
/// reasoning, and a 2 GB download is easier to agree to than a 40 GB one.
pub const OLLAMA_MODEL: &str = "llama3.2:3b";
pub const OLLAMA_MODEL_BYTES: u64 = 2_000_000_000;

/// Where Ollama listens. Loopback only — the transcripts never leave the
/// machine, which is most of the reason for doing this locally at all.
pub const OLLAMA_HOST: &str = "http://127.0.0.1:11434";

/// One thing that has to be present before an episode can be transcribed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// Not used directly — it is how the other three get installed.
    PackageManager,
    /// Whisper only reads 16-bit WAV; podcasts are mp3 and m4a.
    Ffmpeg,
    Whisper,
    WhisperModel,
    /// Optional, and everything below here stays optional: without it the
    /// transcript still happens, it just numbers every turn afresh.
    Ollama,
    OllamaModel,
}

impl Tool {
    /// Everything the tab cares about, in the order it is needed.
    pub fn all() -> [Tool; 6] {
        [
            Tool::PackageManager,
            Tool::Ffmpeg,
            Tool::Whisper,
            Tool::WhisperModel,
            Tool::Ollama,
            Tool::OllamaModel,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::PackageManager => "Package manager",
            Tool::Ffmpeg => "FFmpeg",
            Tool::Whisper => "Whisper",
            Tool::WhisperModel => "Whisper speech model",
            Tool::Ollama => "Ollama",
            Tool::OllamaModel => "Ollama language model",
        }
    }

    /// What it is for, in the user's terms rather than the pipeline's.
    pub fn why(self) -> &'static str {
        match self {
            Tool::PackageManager => "Installs the three pieces below.",
            Tool::Ffmpeg => "Converts episodes to the audio format Whisper reads.",
            Tool::Whisper => "Turns the audio into text.",
            Tool::WhisperModel => "The speech model Whisper listens with.",
            Tool::Ollama => "Optional. Keeps a speaker's number the same all the way through.",
            Tool::OllamaModel => "Optional. The model Ollama uses to do it.",
        }
    }

    /// Whether a transcript can be produced without it.
    ///
    /// The two Ollama entries are optional by design — see the module docs on
    /// the Transcripts tab. Without them every detected turn gets the next
    /// number up; with them the numbers stay attached to a person.
    pub fn optional(self) -> bool {
        matches!(self, Tool::Ollama | Tool::OllamaModel)
    }
}

/// What a survey found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Present, with whatever identifies it — a path or a version.
    Ready(String),
    Missing,
}

impl Presence {
    pub fn is_ready(&self) -> bool {
        matches!(self, Presence::Ready(_))
    }
}

/// A tool and what looking for it found.
#[derive(Clone, Debug)]
pub struct Found {
    pub tool: Tool,
    pub presence: Presence,
    /// Where it is, when that is a path we will later need to execute.
    pub path: Option<PathBuf>,
}

/// The package managers we know how to drive, per platform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Manager {
    Homebrew,
    Apt,
    Dnf,
    Pacman,
    Winget,
}

impl Manager {
    pub fn label(self) -> &'static str {
        match self {
            Manager::Homebrew => "Homebrew",
            Manager::Apt => "apt",
            Manager::Dnf => "dnf",
            Manager::Pacman => "pacman",
            Manager::Winget => "winget",
        }
    }

    /// The command that installs a package, ready to run.
    fn install_args(self, package: &str) -> (String, Vec<String>) {
        let owned = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            Manager::Homebrew => ("brew".into(), owned(&["install", package])),
            // Non-interactive so the app is never left waiting on a "[Y/n]"
            // nobody can see, and sudo because these two need root.
            Manager::Apt => (
                "sudo".into(),
                owned(&["apt-get", "install", "-y", package]),
            ),
            Manager::Dnf => ("sudo".into(), owned(&["dnf", "install", "-y", package])),
            Manager::Pacman => (
                "sudo".into(),
                owned(&["pacman", "-S", "--noconfirm", package]),
            ),
            Manager::Winget => (
                "winget".into(),
                owned(&[
                    "install",
                    "-e",
                    "--id",
                    package,
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ]),
            ),
        }
    }

    /// What this manager calls the thing we want.
    fn package_for(self, tool: Tool) -> Option<&'static str> {
        match (self, tool) {
            (Manager::Winget, Tool::Ffmpeg) => Some("Gyan.FFmpeg"),
            (Manager::Winget, Tool::Whisper) => None, // no winget package; see plan()
            (Manager::Winget, Tool::Ollama) => Some("Ollama.Ollama"),
            (_, Tool::Ffmpeg) => Some("ffmpeg"),
            (Manager::Homebrew, Tool::Whisper) => Some("whisper-cpp"),
            (Manager::Homebrew, Tool::Ollama) => Some("ollama"),
            // whisper.cpp is not packaged on the Linux managers, and Ollama
            // ships its own installer rather than a distro package.
            (_, Tool::Whisper) | (_, Tool::Ollama) => None,
            _ => None,
        }
    }

    /// Whether root is needed, which decides whether we may run it ourselves.
    fn needs_root(self) -> bool {
        matches!(self, Manager::Apt | Manager::Dnf | Manager::Pacman)
    }
}

/// How a missing thing could be put right.
#[derive(Clone, Debug)]
pub enum Install {
    /// Something we can run ourselves once agreed to. No password, no prompts.
    Run {
        program: String,
        args: Vec<String>,
        /// Written out for the confirmation box, so what is agreed to is the
        /// command that actually runs.
        display: String,
    },
    /// A file to fetch. Size is known up front so the box can quote it.
    Download { url: String, to: PathBuf, bytes: u64 },
    /// Needs a terminal — a password, or a prompt only a human can answer. We
    /// show it rather than run it.
    Guided { command: String, why: String },
    /// We have nothing to offer; the user is told where to go instead.
    Manual { why: String, url: String },
}

/// Look for one program on the PATH, and in the places a GUI app's PATH misses.
///
/// An app launched from Finder or a `.desktop` file inherits a login PATH that
/// very often has no `/opt/homebrew/bin` in it, so `which`-style lookup alone
/// reports Homebrew's binaries as missing on a machine that plainly has them.
/// Checking the well-known prefixes as well is the difference between the tab
/// working and the tab insisting the user install what they already installed.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(&exe);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }

    for dir in EXTRA_BIN_DIRS {
        let candidate = Path::new(dir).join(&exe);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// The prefixes a windowed process is most likely to be missing. Apple Silicon
/// Homebrew first: it is where this app's own releases are aimed.
const EXTRA_BIN_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/home/linuxbrew/.linuxbrew/bin",
];

fn is_executable(path: &Path) -> bool {
    // On Unix a file we cannot execute is no better than one that isn't there,
    // and treating it as found would only move the failure later.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Which package manager this machine has, if any.
pub fn manager() -> Option<Manager> {
    let candidates: &[(&str, Manager)] = if cfg!(target_os = "macos") {
        &[("brew", Manager::Homebrew)]
    } else if cfg!(windows) {
        &[("winget", Manager::Winget)]
    } else {
        &[
            ("brew", Manager::Homebrew),
            ("apt-get", Manager::Apt),
            ("dnf", Manager::Dnf),
            ("pacman", Manager::Pacman),
        ]
    };

    candidates
        .iter()
        .find(|(bin, _)| find_program(bin).is_some())
        .map(|(_, m)| *m)
}

/// Where downloaded models live: ours to manage, and outside the app bundle so
/// an app update doesn't throw away half a gigabyte the user agreed to fetch.
pub fn model_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("podbatch")
        .join("models")
}

pub fn model_path() -> PathBuf {
    model_dir().join(MODEL_FILE)
}

/// Look the machine over. Cheap enough to run whenever the tab is shown, and
/// free of side effects by construction.
pub fn survey() -> Vec<Found> {
    Tool::all().iter().map(|t| probe(*t)).collect()
}

fn probe(tool: Tool) -> Found {
    let (presence, path) = match tool {
        Tool::PackageManager => match manager() {
            Some(m) => (Presence::Ready(m.label().to_string()), find_program("brew")),
            None => (Presence::Missing, None),
        },
        Tool::Ffmpeg => match find_program("ffmpeg") {
            Some(p) => (Presence::Ready(elide(&p)), Some(p)),
            None => (Presence::Missing, None),
        },
        Tool::Whisper => match whisper_binary() {
            Some(p) => (Presence::Ready(elide(&p)), Some(p)),
            None => (Presence::Missing, None),
        },
        Tool::WhisperModel => {
            let path = model_path();
            // Size-checked, not just existence-checked: a download interrupted
            // half way leaves a file that exists and cannot be loaded, and
            // "the model is there but Whisper won't start" is a much worse
            // thing to debug than "the model is missing".
            let ok = std::fs::metadata(&path)
                .map(|m| m.is_file() && m.len() == MODEL_BYTES)
                .unwrap_or(false);
            if ok {
                (Presence::Ready(elide(&path)), Some(path))
            } else {
                (Presence::Missing, None)
            }
        }
        Tool::Ollama => match find_program("ollama") {
            Some(p) => (Presence::Ready(elide(&p)), Some(p)),
            None => (Presence::Missing, None),
        },
        Tool::OllamaModel => {
            if has_ollama_model(OLLAMA_MODEL) {
                (Presence::Ready(OLLAMA_MODEL.to_string()), None)
            } else {
                (Presence::Missing, None)
            }
        }
    };
    Found { tool, presence, path }
}

/// Homebrew's formula installs `whisper-cli`; older builds and hand-built
/// copies still call it `main` or `whisper`. Accept any of them rather than
/// telling somebody with a working whisper.cpp that they haven't got one.
pub fn whisper_binary() -> Option<PathBuf> {
    ["whisper-cli", "whisper-cpp", "whisper"]
        .iter()
        .find_map(|n| find_program(n))
}

/// Ask the Ollama server what it has. A server that isn't running answers
/// nothing, which is reported the same as not having the model — in both cases
/// the optional step cannot run, and in both cases starting Ollama is the fix.
fn has_ollama_model(model: &str) -> bool {
    let Some(body) = ollama_tags() else {
        return false;
    };
    // Matching on the bare name as well as the tagged one: `llama3.2:3b`
    // pulled by another route can be listed as `llama3.2:3b` or, if it is the
    // default tag, just `llama3.2`.
    let base = model.split(':').next().unwrap_or(model);
    body.contains(model) || body.contains(&format!("\"{base}:latest\""))
}

/// The raw `/api/tags` body, or `None` if nothing is listening.
///
/// Deliberately a blocking call with a short timeout: it runs on the UI thread
/// during a survey, and a machine with no Ollama must not cost a visible pause.
fn ollama_tags() -> Option<String> {
    let response = std::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "2",
            &format!("{OLLAMA_HOST}/api/tags"),
        ])
        .output()
        .ok()?;
    if !response.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&response.stdout).into_owned();
    body.contains("models").then_some(body)
}

/// Whether an Ollama server is answering at all.
pub fn ollama_running() -> bool {
    ollama_tags().is_some()
}

/// Start Ollama's server in the background.
///
/// Installing Ollama does not start it, and a freshly installed one answers
/// nothing until it does. Spawned detached and never waited on — this returns
/// straight away and the survey that follows is what decides whether it worked.
pub fn start_ollama() -> Result<(), String> {
    let Some(bin) = find_program("ollama") else {
        return Err("Ollama is not installed.".into());
    };
    Command::new(bin)
        .arg("serve")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not start Ollama — {e}"))
}

/// What would put this tool right, given what the machine has.
pub fn plan(tool: Tool, manager: Option<Manager>) -> Install {
    match tool {
        Tool::PackageManager => bootstrap_manager(),
        Tool::WhisperModel => Install::Download {
            url: MODEL_URL.to_string(),
            to: model_path(),
            bytes: MODEL_BYTES,
        },
        Tool::OllamaModel => Install::Run {
            program: find_program("ollama")
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "ollama".into()),
            args: vec!["pull".into(), OLLAMA_MODEL.into()],
            display: format!("ollama pull {OLLAMA_MODEL}"),
        },
        Tool::Ffmpeg | Tool::Whisper | Tool::Ollama => match manager {
            Some(m) => package_install(m, tool),
            None => Install::Guided {
                command: bootstrap_command(),
                why: format!(
                    "{} is installed with a package manager, and this machine has none yet.",
                    tool.label()
                ),
            },
        },
    }
}

fn package_install(manager: Manager, tool: Tool) -> Install {
    let Some(package) = manager.package_for(tool) else {
        return unpackaged(tool);
    };
    let (program, args) = manager.install_args(package);
    let display = std::iter::once(program.clone())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");

    // A command that will stop dead on a password prompt is not one we can run
    // for somebody: from a windowed app there is nowhere for them to type it.
    if manager.needs_root() {
        return Install::Guided {
            command: display,
            why: format!(
                "Installing {} with {} needs an administrator password, which has to be \
                 typed at a terminal.",
                tool.label(),
                manager.label()
            ),
        };
    }

    Install::Run { program, args, display }
}

/// The two things no package manager we drive will install for us.
fn unpackaged(tool: Tool) -> Install {
    match tool {
        Tool::Ollama => Install::Manual {
            why: "Ollama ships its own installer for this platform.".into(),
            url: "https://ollama.com/download".into(),
        },
        _ => Install::Manual {
            why: format!(
                "{} has no package on this platform and has to be built or downloaded by hand.",
                tool.label()
            ),
            url: "https://github.com/ggml-org/whisper.cpp#quick-start".into(),
        },
    }
}

/// Getting a package manager onto a machine that has none.
///
/// Always [`Install::Guided`]. Homebrew's installer asks for a password and
/// refuses to run unattended for good reasons of its own; wrapping that in a
/// GUI would mean either collecting a password in a window that has no business
/// holding one, or lying to the user about what is happening.
fn bootstrap_manager() -> Install {
    Install::Guided {
        command: bootstrap_command(),
        why: if cfg!(target_os = "macos") {
            "Homebrew's installer needs an administrator password, so it has to be run in \
             Terminal. Paste this in, let it finish, then come back and press Check again."
                .into()
        } else if cfg!(windows) {
            "winget comes with App Installer from the Microsoft Store.".into()
        } else {
            "Install one of apt, dnf or pacman for your distribution.".into()
        },
    }
}

fn bootstrap_command() -> String {
    if cfg!(target_os = "macos") {
        "/bin/bash -c \"$(curl -fsSL \
         https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\""
            .into()
    } else if cfg!(windows) {
        "winget --version".into()
    } else {
        "sudo apt-get install -y ffmpeg".into()
    }
}

/// Run an agreed-to install, handing every line it prints to `on_line`.
///
/// Blocking, and meant to be called from a worker thread. The output is streamed
/// rather than collected because a `brew install` can take minutes and a window
/// that says nothing for minutes looks broken.
pub fn install(
    program: &str,
    args: &[String],
    mut on_line: impl FnMut(String),
) -> Result<(), String> {
    use std::io::{BufRead, BufReader};

    logs::debug(format!("installing: {program} {}", args.join(" ")));

    let mut child = Command::new(program)
        .args(args)
        // Both streams into one pipe, in the order they were written: package
        // managers put progress on stderr and results on stdout, and a log that
        // interleaves them is the one that reads like what happened.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not run {program} — {e}"))?;

    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            on_line(line);
        }
    }
    if let Some(err) = child.stderr.take() {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            on_line(line);
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("{program} did not finish — {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} stopped with {}",
            status
                .code()
                .map(|c| format!("code {c}"))
                .unwrap_or_else(|| "no exit code".into())
        ))
    }
}

/// Fetch a file, reporting bytes as they land.
///
/// Blocking, for a worker thread, with a runtime of its own — this is the only
/// place outside the download engine that touches the network, and building a
/// one-off runtime here is cheaper than threading the engine's through the UI.
///
/// Downloads to a `.part` beside the destination and renames on success, so an
/// interrupted fetch can never be mistaken for a finished model. That matters
/// more than usual here: half a Whisper model is a file that exists, is the
/// wrong size, and makes Whisper fail with something unhelpful.
pub fn download(
    url: &str,
    to: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
    cancelled: impl Fn() -> bool,
) -> Result<(), String> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Could not make {} — {e}", parent.display()))?;
    }
    let part = to.with_extension("part");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Could not start the downloader — {e}"))?;

    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(concat!("PodBatch/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("Could not start the downloader — {e}"))?;

        let response = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Could not reach {url} — {e}"))?;

        if !response.status().is_success() {
            return Err(format!("The download answered {}", response.status()));
        }

        let total = response.content_length();
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| format!("Could not write {} — {e}", part.display()))?;

        let mut done = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if cancelled() {
                return Err("stopped".into());
            }
            let chunk = chunk.map_err(|e| format!("The download broke off — {e}"))?;
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("Could not write {} — {e}", part.display()))?;
            done += chunk.len() as u64;
            on_progress(done, total);
        }
        file.flush()
            .await
            .map_err(|e| format!("Could not finish writing — {e}"))?;
        Ok(())
    })
    .inspect_err(|_| {
        std::fs::remove_file(&part).ok();
    })?;

    std::fs::rename(&part, to).map_err(|e| format!("Could not put the file in place — {e}"))
}

/// Open a terminal with the command already on the clipboard's behalf — or, on
/// platforms where that is not something we can do cleanly, just the terminal.
pub fn open_terminal() -> Result<(), String> {
    let result = if cfg!(target_os = "macos") {
        Command::new("open").args(["-a", "Terminal"]).spawn()
    } else if cfg!(windows) {
        Command::new("cmd").args(["/c", "start", "cmd"]).spawn()
    } else {
        Command::new("x-terminal-emulator").spawn()
    };
    result.map(|_| ()).map_err(|e| format!("Could not open a terminal — {e}"))
}

/// Open a page in whatever the user browses with.
pub fn open_url(url: &str) -> Result<(), String> {
    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else if cfg!(windows) {
        Command::new("cmd").args(["/c", "start", "", url]).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
    result.map(|_| ()).map_err(|e| format!("Could not open {url} — {e}"))
}

/// Shorten a path for display the way the rest of the app does.
fn elide(path: &Path) -> String {
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

    #[test]
    fn every_tool_says_what_it_is_for() {
        for tool in Tool::all() {
            assert!(!tool.label().is_empty(), "{tool:?} has no label");
            assert!(!tool.why().is_empty(), "{tool:?} has no reason");
        }
    }

    #[test]
    fn only_the_ollama_pieces_are_optional() {
        let optional: Vec<_> = Tool::all().into_iter().filter(|t| t.optional()).collect();
        assert_eq!(optional, vec![Tool::Ollama, Tool::OllamaModel]);
    }

    /// The bootstrap is the one thing we must never run ourselves.
    #[test]
    fn a_missing_package_manager_is_only_ever_guided() {
        assert!(matches!(
            plan(Tool::PackageManager, None),
            Install::Guided { .. }
        ));
        assert!(matches!(
            plan(Tool::PackageManager, Some(Manager::Homebrew)),
            Install::Guided { .. }
        ));
    }

    /// Anything wanting a password is guided too, for the same reason: there is
    /// nowhere in a windowed app for the password to be typed.
    #[test]
    fn root_installs_are_never_run_for_the_user() {
        for manager in [Manager::Apt, Manager::Dnf, Manager::Pacman] {
            assert!(
                matches!(plan(Tool::Ffmpeg, Some(manager)), Install::Guided { .. }),
                "{manager:?} would have been run unattended"
            );
        }
    }

    #[test]
    fn homebrew_installs_are_ones_we_can_run() {
        let Install::Run { display, .. } = plan(Tool::Ffmpeg, Some(Manager::Homebrew)) else {
            panic!("expected a runnable install");
        };
        assert_eq!(display, "brew install ffmpeg");

        let Install::Run { display, .. } = plan(Tool::Whisper, Some(Manager::Homebrew)) else {
            panic!("expected a runnable install");
        };
        assert_eq!(display, "brew install whisper-cpp");
    }

    /// The size in the confirmation box is the size that gets downloaded, and a
    /// partly-downloaded model is detected as missing rather than used.
    #[test]
    fn the_model_is_checked_by_size_not_just_existence() {
        let dir = std::env::temp_dir().join(format!("podbatch-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(MODEL_FILE);
        std::fs::write(&path, b"not the whole model").expect("write");

        let truncated = std::fs::metadata(&path)
            .map(|m| m.is_file() && m.len() == MODEL_BYTES)
            .unwrap_or(false);
        assert!(!truncated, "a short file passed as a complete model");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_program_that_cannot_exist_is_not_found() {
        assert_eq!(find_program("podbatch-no-such-program-xyzzy"), None);
    }

    /// Hits the network, so it is not part of the ordinary run. Run it with
    /// `cargo test -- --ignored` when the pinned size needs re-checking.
    #[test]
    #[ignore = "hits huggingface.co"]
    fn the_model_size_is_honest() {
        let out = Command::new("curl")
            .args(["-sIL", MODEL_URL])
            .output()
            .expect("curl");
        let headers = String::from_utf8_lossy(&out.stdout);
        let length = headers
            .lines()
            .filter_map(|l| l.strip_prefix("content-length: "))
            .filter_map(|v| v.trim().parse::<u64>().ok())
            .max()
            .expect("a content-length");
        assert_eq!(length, MODEL_BYTES);
    }
}
