//! The two cues.
//!
//! A run can take a long time and gets on with it without anyone watching,
//! which is the whole reason for a sound: it says how the run is going to
//! someone who has gone to do something else, and says it without them having
//! to come back and read a screen. One plays as each episode lands and the
//! other when something fails, so a run that is quietly failing sounds
//! different from one that is quietly working; the last of them says the run
//! has ended, and which of the two ways it ended.
//!
//! Which cue is played when, and how closely two of them may follow each
//! other, is the window's business — see `app::PodBatchApp::play`.
//!
//! Both are CC0 recordings from freesound.org, levelled to the same loudness so
//! that neither is the startling one — see `assets/sounds/CREDITS.txt`. They are
//! the same recordings the accessengine app uses, so the two sound alike as
//! well as look alike.

use std::io::Cursor;

const SUCCESS: &[u8] = include_bytes!("../assets/sounds/success.wav");
const ERROR: &[u8] = include_bytes!("../assets/sounds/error.wav");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cue {
    /// Everything that was asked for is on disk.
    Success,
    /// The run finished, but something in it didn't.
    Failure,
}

/// Play a cue, without blocking the caller.
///
/// The audio device has to stay open until the sound has finished, so the
/// thread this spawns holds it and waits rather than returning a handle for the
/// UI to keep alive. A cue is under a second long and nothing can cancel it,
/// so there is nothing useful the caller could do with such a handle anyway.
pub fn play(cue: Cue) {
    let wav = match cue {
        Cue::Success => SUCCESS,
        Cue::Failure => ERROR,
    };

    // A machine with no sound card, or one whose device is busy, is not a
    // failure worth reporting: the cue is a courtesy on top of a UI that
    // already says the same thing in writing.
    let _ = std::thread::Builder::new()
        .name("podbatch-cue".into())
        .spawn(move || {
            let _ = play_blocking(wav);
        });
}

fn play_blocking(wav: &'static [u8]) -> Result<(), Box<dyn std::error::Error>> {
    let decoder = rodio::Decoder::builder()
        .with_data(Cursor::new(wav))
        .with_byte_len(wav.len() as u64)
        .build()?;

    let mut device = rodio::DeviceSinkBuilder::open_default_sink()?;
    // rodio warns that dropping the sink ends playback, which is exactly what
    // we intend to do once the sound has finished.
    device.log_on_drop(false);

    let player = rodio::Player::connect_new(device.mixer());
    player.append(decoder);
    player.sleep_until_end();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both cues are real, decodable WAVs. Cheap insurance against an asset
    /// that got truncated or replaced with something rodio can't open — which
    /// would otherwise only show up as silence on a user's machine.
    #[test]
    fn both_cues_decode() {
        for wav in [SUCCESS, ERROR] {
            let decoder = rodio::Decoder::builder()
                .with_data(Cursor::new(wav))
                .with_byte_len(wav.len() as u64)
                .build()
                .expect("cue should decode");
            assert!(
                rodio::Source::total_duration(&decoder).is_some_and(|d| !d.is_zero()),
                "cue should have a length"
            );
        }
    }
}
