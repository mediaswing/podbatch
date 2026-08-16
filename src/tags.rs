//! Writing what the feed says about an episode into the file itself.
//!
//! Episodes are named after the minute they were published, which sorts and
//! stays unique but tells a reader nothing. The tags are where the episode's
//! identity lives instead: the show, the title, the date and the blurb, in the
//! frames every podcast player and music library already reads.
//!
//! Only the containers that actually carry ID3 are written to. An `.m4a` or an
//! `.ogg` keeps its metadata somewhere else entirely, and prepending an ID3
//! header to one would corrupt it rather than describe it.
//!
//! Which container a file is, is decided by reading the front of it. The
//! extension can't be trusted for this: it is a guess made from the enclosure
//! URL and the MIME type, and [`crate::util::extension_for`] falls back to
//! `mp3` whenever the feed says nothing useful — so a `.mp3` here is quite
//! often an M4A from a tracking URL that declared no type at all.

use std::io::Read;
use std::path::Path;

use id3::frame::{Comment, Timestamp};
use id3::{Frame, Tag, TagLike, Version};

use crate::util::Published;

/// Everything worth saying about an episode, gathered from the feed and the
/// subscription it came from. Owned, because tagging happens on a blocking
/// thread after the download that produced it has finished.
#[derive(Debug, Clone)]
pub struct Details {
    /// The podcast, as the OPML file names it — the same string the folder is
    /// named after.
    pub show: String,
    pub title: String,
    pub published: Option<Published>,
    pub description: Option<String>,
    pub feed_url: String,
}

/// What became of an episode's tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tagged {
    /// The tags are in the file.
    Written,
    /// The file isn't one that carries ID3, and was left exactly as it landed.
    Unsupported,
}

/// Enough bytes to tell the containers apart: the longest test looks at the
/// four at offset 8.
const MAGIC: usize = 12;

/// Whether the bytes at the front of a file are a container that takes an ID3
/// tag — MP3, which carries one at the front of the stream, or AIFF and WAV,
/// which carry one in a chunk of their own.
///
/// Anything else is left alone. The tag writer works the layout out from these
/// same bytes and treats what it doesn't recognise as a bare stream, so handing
/// it an M4A would move the `ftyp` box off the front of the file and leave
/// something no player will open.
fn takes_tags(head: &[u8]) -> bool {
    let at = |offset: usize, magic: &[u8]| head.len() >= offset + magic.len()
        && &head[offset..offset + magic.len()] == magic;

    // An MP3 either opens with an ID3 tag or with an MPEG frame sync: eleven
    // set bits, which is the same in every version and layer of MPEG audio.
    let mpeg_sync = head.len() >= 2 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0;

    at(0, b"ID3")
        || mpeg_sync
        || (at(0, b"FORM") && (at(8, b"AIFF") || at(8, b"AIFC")))
        || (at(0, b"RIFF") && at(8, b"WAVE"))
}

/// The first bytes of a file, however few of them there are.
fn head_of(path: &Path) -> Result<Vec<u8>, String> {
    let mut head = vec![0u8; MAGIC];
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;

    // A short read is not an error: a file smaller than the probe simply isn't
    // any of the containers being looked for.
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    head.truncate(filled);

    Ok(head)
}

/// Tag a downloaded episode in place.
///
/// Blocking: call it off the async runtime. Any tag already in the file is kept
/// and written back with ours on top, so cover art a publisher embedded — which
/// we have no way of putting back — survives.
pub fn write(path: &Path, details: &Details) -> Result<Tagged, String> {
    if !takes_tags(&head_of(path)?) {
        return Ok(Tagged::Unsupported);
    }

    // A file with no tag yet is the normal case, not a failure.
    let mut tag = Tag::read_from_path(path).unwrap_or_default();

    tag.set_title(details.title.clone());
    // Players file podcasts under the show, not under a per-episode artist, and
    // the feed's own author field is as often the network as the presenter.
    tag.set_artist(details.show.clone());
    tag.set_album_artist(details.show.clone());
    tag.set_album(details.show.clone());
    tag.set_genre("Podcast");

    if let Some(published) = details.published {
        tag.set_year(published.year);
        tag.set_date_recorded(Timestamp {
            year: published.year,
            month: Some(published.month as u8),
            day: Some(published.day as u8),
            hour: Some(published.hour as u8),
            minute: Some(published.minute as u8),
            second: None,
        });
    }

    if let Some(description) = details.description.as_deref().filter(|d| !d.is_empty()) {
        tag.add_frame(Comment {
            lang: "eng".to_string(),
            description: String::new(),
            text: description.to_string(),
        });
        // The frame podcast players look in for the episode notes.
        tag.add_frame(Frame::text("TDES", description));
    }

    // Where the episode came from, so a file that has been moved somewhere else
    // still says which podcast to go back to.
    tag.add_frame(Frame::link("WFED", details.feed_url.clone()));

    // The reader and the writer both work the layout out from the file itself,
    // so this one call covers all three containers.
    tag.write_to_path(path, Version::Id3v24)
        .map(|()| Tagged::Written)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details() -> Details {
        Details {
            show: "My Show".into(),
            title: "Episode One".into(),
            published: Some(Published { year: 2025, month: 8, day: 5, hour: 10, minute: 0 }),
            description: Some("What happened, and to whom.".into()),
            feed_url: "https://one.test/feed.xml".into(),
        }
    }

    /// An MP3 stream, near enough: an MPEG frame sync and then some bytes.
    fn mp3(len: usize) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFB, 0x90, 0x00];
        bytes.extend((0..len).map(|i| (i % 251) as u8));
        bytes
    }

    /// An MP4 container — the shape an `.m4a` episode arrives in.
    fn m4a(len: usize) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 0x18];
        bytes.extend(b"ftypM4A ");
        bytes.extend((0..len).map(|i| (i % 251) as u8));
        bytes
    }

    #[test]
    fn containers_are_told_apart_by_their_first_bytes() {
        assert!(takes_tags(&mp3(64)));
        assert!(takes_tags(b"ID3\x04\x00\x00\x00\x00\x00\x00rest"));
        assert!(takes_tags(b"FORM\0\0\0\0AIFF"));
        assert!(takes_tags(b"RIFF\0\0\0\0WAVE"));

        assert!(!takes_tags(&m4a(64)));
        assert!(!takes_tags(b"OggS\0\x02\0\0\0\0\0\0"));
        // "FORM" alone is not enough — that is IFF, and only some of it is AIFF.
        assert!(!takes_tags(b"FORM\0\0\0\08SVX"));
        assert!(!takes_tags(b"\xff"));
        assert!(!takes_tags(b""));
    }

    /// Cleaned up on drop, so a failing assertion doesn't leave a file behind.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("podbatch-tags-{nanos}"));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The tag is the only place the episode's identity survives, so it is
    /// worth reading back rather than assuming the write took.
    #[test]
    fn writes_what_the_feed_said_and_leaves_the_audio_alone() {
        let dir = TempDir::new();
        let path = dir.0.join("050825-1000.mp3");
        let audio = mp3(2000);
        std::fs::write(&path, &audio).expect("write audio");

        assert_eq!(write(&path, &details()), Ok(Tagged::Written));

        let tag = Tag::read_from_path(&path).expect("read back");
        assert_eq!(tag.title(), Some("Episode One"));
        assert_eq!(tag.album(), Some("My Show"));
        assert_eq!(tag.artist(), Some("My Show"));
        assert_eq!(tag.year(), Some(2025));
        assert_eq!(
            tag.date_recorded().map(|d| (d.year, d.month, d.day, d.hour, d.minute)),
            Some((2025, Some(8), Some(5), Some(10), Some(0)))
        );
        assert_eq!(
            tag.comments().next().map(|c| c.text.as_str()),
            Some("What happened, and to whom.")
        );

        // Tagging adds to the file; it must not rewrite what was downloaded.
        let tagged = std::fs::read(&path).expect("read file");
        assert!(tagged.len() > audio.len());
        assert!(tagged.ends_with(&audio), "the audio was altered");
    }

    /// The extension is a guess — `util::extension_for` says `mp3` for any
    /// enclosure whose URL and MIME type give nothing away — so an M4A arrives
    /// here called `.mp3` often enough. Tagging it on the strength of that name
    /// would push the `ftyp` box off the front of the file and leave an episode
    /// no player will open, which is worse than an episode with no tags.
    #[test]
    fn a_file_that_only_looks_like_an_mp3_is_left_exactly_as_it_landed() {
        let dir = TempDir::new();
        let path = dir.0.join("050825-1000.mp3");
        let audio = m4a(2000);
        std::fs::write(&path, &audio).expect("write audio");

        assert_eq!(write(&path, &details()), Ok(Tagged::Unsupported));
        assert_eq!(std::fs::read(&path).expect("read file"), audio);
    }

    /// A file too short to identify is not a file to write a tag onto.
    #[test]
    fn a_truncated_file_is_left_alone_rather_than_guessed_at() {
        let dir = TempDir::new();
        let path = dir.0.join("050825-1000.mp3");
        std::fs::write(&path, b"no").expect("write stub");

        assert_eq!(write(&path, &details()), Ok(Tagged::Unsupported));
        assert_eq!(std::fs::read(&path).expect("read file"), b"no");
    }
}
