//! podbatch — the Podcast Downloader.
//!
//! Point it at an OPML subscription list exported from a podcast app and it
//! fetches every feed in the list and downloads every episode, giving each
//! podcast a subfolder of its own under `~/podcasts`.
//!
//! The window is in [`app`], the network and disk work in [`engine`]; they only
//! ever speak through a channel, so a slow feed can't freeze the UI.

// Don't open a console window alongside the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod engine;
mod feed;
mod opml;
mod sound;
mod theme;
mod util;

/// The name shown to the user, which is not the name of the binary.
const APP_TITLE: &str = "Podcast Downloader";

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            // Wide enough for an episode title, its progress bar and the byte
            // counts on one line without truncation doing the work, and short
            // enough to fit on a laptop screen with room for the dock — a
            // window taller than the display gets clamped, and what it loses is
            // the episode list at the bottom.
            .with_inner_size([900.0, 680.0])
            .with_min_inner_size([680.0, 460.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    // An OPML path on the command line pre-fills the file field, so the app can
    // be opened straight from a subscription file rather than by launching it
    // and then going to find the same file in a dialog.
    let opml = opml_argument(std::env::args_os());

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(app::PodBatchApp::new(cc, opml)))),
    )
}

/// The first argument, if it names a file that exists.
///
/// Takes the arguments as a parameter rather than reading them itself, so the
/// parsing can be tested without a real process command line to point it at.
fn opml_argument(args: impl Iterator<Item = std::ffi::OsString>) -> Option<std::path::PathBuf> {
    args.skip(1)
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(items: &[&str]) -> impl Iterator<Item = OsString> + use<> {
        items
            .iter()
            .map(|s| OsString::from(*s))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn takes_an_existing_file_and_ignores_anything_else() {
        // argv[0] is the binary, which exists but is never the OPML.
        let exe = std::env::current_exe().expect("current exe");
        let exe = exe.to_string_lossy().into_owned();

        assert_eq!(opml_argument(args(&[&exe])), None);
        assert_eq!(opml_argument(args(&[&exe, "/no/such/file.opml"])), None);
        assert_eq!(
            opml_argument(args(&[&exe, &exe])),
            Some(std::path::PathBuf::from(&exe))
        );
    }
}
