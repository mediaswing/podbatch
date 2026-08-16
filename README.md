# Podcast Downloader

Point it at the OPML file your podcast app exports and it downloads the
episodes, giving every podcast a folder of its own under `~/Podbatch/Downloads`.

It is a desktop app rather than a script because choosing which of forty
subscriptions to fetch, and watching sixty large files come down over a slow
connection, are both things a window does better than a terminal.

## The window

Three tabs, all spanning the full width of the window.

**Downloads** is where the work happens. The subscription list goes in at the
top — choose an OPML file, or drop one onto the window. Underneath, the pane is
split: the podcasts the file contains are listed on the left with a tick beside
each, and what the run is doing is written on the right. The progress bar spans
the width beneath both.

Everything arrives ticked, because the usual answer is "all of them"; untick the
few you don't want. **All** and **None** are there for when it isn't.

**Transcripts** turns episodes you already have into text. Choose a folder, tick
the episodes, and each one is written out as a `.txt` beside its audio.

It leans on programs it does not ship. FFmpeg converts the audio, because
Whisper reads 16 kHz WAV and a podcast is an mp3; whisper.cpp does the
listening. The tab lists what it needs, says what each piece is for, and offers
to install the missing ones — **nothing is installed without being asked**, and
every question names the exact command it is about to run. Where a step needs an
administrator password, the app will not run it for you: it shows the command
and opens a terminal, because a window has no honest place to collect a
password.

Speakers are marked "Speaker 1", "Speaker 2" and so on. Two things are worth
knowing about those numbers:

- Whisper hears **that** the voice changed, never **whose** it is. On its own the
  transcript therefore alternates between Speaker 1 and Speaker 2 — which is the
  only thing a turn mark forces, and is right for two people talking. Three or
  more, and it has no way to tell them apart. One missed change also swaps the
  two names from that point on.
- Ollama, if you have it, can read the turns afterwards and work out which are
  the same person, so a number stays attached to somebody. This is optional and
  off the critical path: without it you still get a full transcript, just with
  the blunter numbering. It never hears the audio — it is reading words — so it
  can get this wrong, and the transcript says which of the two kinds of
  numbering it got.

How well the turns come out depends a great deal on the recording. A
conversation between two or three people works well. A narrated documentary with
pre-recorded inserts may yield no turn marks at all, which is the model being
honest rather than the app failing.

**Settings** holds what gets set once and then left alone:

| Setting | Default | What it does |
| --- | --- | --- |
| Save episodes in | `~/Podbatch/Downloads` | Each podcast gets a subfolder of its own in here. Point it anywhere you like — an external drive, say, since a full subscription list runs to gigabytes. |
| Skip episodes already downloaded | on | Leaves a file alone when it is already on disk at the size the feed claims. Turn it off to fetch everything again. |
| Only the newest *N* episodes | off | Takes the first *N* items in each feed, which is the newest *N* — feeds are published newest-first. |
| Downloads at once | 4 | How many files are fetched in parallel, across all podcasts. |
| Sound as episodes land | on | A cue as each episode finishes, a different one when something fails, and a last one when the run ends. |
| Appearance | System | Light, dark, or whatever this computer is set to. Both looks are built to the same contrast standard. |

## Before it starts

**Download episodes** doesn't start downloading. It reads the feeds first —
which is the only way anyone, the app included, can know how many episodes there
are and what they weigh — and then asks:

> **Download 47 episodes?**
> 3.2 GB to fetch, plus 4 episodes that don't say how big they are.
> At around 2.4 MB/s on one connection, that is at least 23 minutes — likely quicker, with 4 downloading at once.
> 12 episodes already downloaded, and will be left alone.

"Download the lot" can mean four minutes or four hours, and the difference
matters before it starts rather than after. The size is what the feeds declare,
counting only the episodes that aren't already on disk.

The speed is measured rather than assumed. Before the box appears, a quarter of
a megabyte of one episode is fetched and timed — over in a moment on a fast
line, and cut off after a second and a half on a slow one, so the question
itself never costs much of the time it is there to save. It cannot be measured
from the feeds: those arrive gzipped from nearly every host and are inflated
before anything can count them, which leaves no honest number to divide by.

That measurement is one connection's worth while the run puts several in
flight, so the time it gives is the slow end of what to expect rather than a
promise — hence "likely quicker". Once a run has finished, the speed that run
actually managed replaces it, and being a whole-run figure it needs no such
allowance. A run that could not be measured at all says so, rather than
printing a number with nothing behind it. And when everything is already
downloaded there is no question to ask, so none is asked.

Answering **Cancel**, or pressing Escape, ends the run there. Nothing has been
written: not an episode, not a `.part` file, not so much as a folder — reading
the feeds is all that has happened, and it happens entirely in memory. It counts
as stopping the run, so the sweep below runs too, and any fragment an earlier
interrupted run left behind goes with it.

Stopping asks too. Escape during a run — like the **Stop** button, which asks
the same question — puts up a box rather than abandoning the run on a keypress
that is as often hit by accident as on purpose. Saying yes stops the run and
sweeps up after it: every episode that has already landed stays where it is, and
the ones that were still arriving are deleted rather than left as unplayable
fragments. Starting again picks up from the last complete episode.

## What it writes

```
~/Podbatch/Downloads/
  Some Podcast/
    050825-1000.mp3
    290725-0730.mp3
```

Folder names come from the OPML entry rather than the feed's own title, so they
are the same on every run and can be worked out before a single feed is fetched.

Episode files are named after the minute the episode was published, as
`ddmmyy-hhmm`. Every name is the same width and holds nothing a filesystem could
object to, and two episodes published in the same minute get `(2)`, `(3)` and so
on. Publication times are normalised to UTC first, so shows in different zones
are named on the same clock.

A feed that gives no publication date leaves nothing to stamp — those episodes
keep a name made from their title instead, rather than being given an invented
time that claims something about them that isn't true.

Note that `ddmmyy` is written the way a date is read here, not the way a
filesystem sorts one: ordered by name, 1 January 2025 comes before 2 January
2024. Sort by date modified, or by the date in the file's tags, to get episodes
in the order they were published.

The extension comes from the enclosure URL when it has a believable one and from
the MIME type otherwise — plenty of feeds serve episodes from a tracking URL
with no extension at all.

**Upgrading from 1.1 or earlier:** episodes downloaded before this used
`YYYY-MM-DD - Episode Title.mp3`, and nothing looks for those names any more, so
the first run after upgrading fetches a library it already has and leaves the
old files beside the new ones. Move the old folders aside first if that matters
to you.

### What the file says about itself

The name says only when, so everything else goes into the file's ID3 tags,
where podcast players and music libraries already look for it:

| Frame | What goes in it |
| --- | --- |
| Title | The episode's own title |
| Artist, Album artist, Album | The podcast, as the OPML file names it |
| Genre | `Podcast` |
| Year, Recording time | The publication date and time |
| Comment, `TDES` | The episode's blurb, with the HTML taken out |
| `WFED` | The feed URL, so a file that has been moved still says where it came from |

Any tag the publisher already put in the file is kept and written back
underneath ours, so embedded cover art survives. Tags are only written to the
containers that carry ID3 — MP3, AIFF and WAV. An `.m4a` or an `.ogg` keeps its
metadata somewhere else entirely and is left alone rather than corrupted, and
which is which is decided by reading the front of the file rather than by
trusting its extension: that extension is a guess made from the enclosure URL
and the MIME type, and a feed that gives neither gets `.mp3` by default. If a
tag can't be written the episode still counts as downloaded, with a line in the
output saying why.

### Interrupted downloads

An episode is written to a `.part` file and only renamed into place once it has
arrived in full, so a run that is stopped or killed never leaves a truncated
file that looks finished. Start again and a `.part` file that is still there
resumes from where it stopped, using an HTTP range request when the server
supports one and starting over when it doesn't.

Stopping a run deliberately is the one case where nothing is kept. When the run
ends because you stopped it, a sweep goes through the episodes it was fetching
and deletes their `.part` files — a run you stopped should not cost you disk
space in fragments you cannot play. Everything that had already been renamed
into place is a complete episode and is left alone, so the next run has less to
do rather than more. A run that ends by itself sweeps nothing: a `.part` file at
that point belongs to an episode that failed, and it is what the next run
resumes from.

If a file on disk is shorter than the feed declares, it is downloaded again —
that is what a copy interrupted after the rename looks like. Shorter rather than
different, because the tags written after a download add bytes the feed's figure
knows nothing about.

## The logs

Two files, in `~/Podbatch/Logging` — next door to the episodes, so everything
the app writes is under one folder you can find without being told where your
platform hides application data. The window says where they went the moment it
opens, or why there are none.

`output.log` is the record of what the run did — one line per operation, tagged
`DONE`, `SKIP`, `FAIL` or `----` for the notes in between. It is the same
account the output box gives, except that it keeps: the box holds the last 2000
lines of the current run and empties when the window closes.

`debug.log` is how it did it: every feed fetched, every retry and why, which
episode became which file name, every resume and range request, every tag
written, and any panic on the way out. It also carries everything `output.log`
has, so it reads as one story rather than half of one.

Both are stamped in UTC — a portable program has no reliable way to find the
local zone offset, and a log timestamped an hour out is worse than an honest
`Z`. The size is checked as the app starts: a file already past 2 MB is moved
aside to `.log.old` and a fresh one begun, so the folder keeps two generations
and a machine that runs this nightly doesn't grow a log for ever. A single
session that writes past 2 MB carries on writing until it is restarted.

The window says where the logs are going in its first line of output, and says
so plainly if they couldn't be opened — a read-only home directory costs you the
logs, and nothing else.

## Running it

```sh
cargo run --release
```

It also takes an OPML path on the command line, which fills the file in for you:

```sh
cargo run --release -- ~/Downloads/subscriptions.opml
```

## Building

Needs a Rust toolchain, and nothing else — no system libraries, no CMake, no
platform SDK beyond the one your compiler already uses.

```sh
cargo build --release
cargo test
```

The tests include the download engine end to end: they stand up a real HTTP
server on a loopback port and check that episodes land in the right folders with
the right bytes and the right tags, that a half-finished `.part` file resumes
rather than starting over, that a short file is fetched again, and that stopping
a run mid-transfer leaves no fragment behind.

## Feeds it understands

RSS 2.0 with `<enclosure>`, which is what essentially every podcast publishes,
and Atom with `<link rel="enclosure">`. Namespaced extensions such as `itunes:`
are ignored in favour of the plain elements. Items with no media attached are
skipped, and an episode listed twice under one `guid` is downloaded once.

OPML is read permissively: any `outline` with an `xmlUrl` counts as a
subscription, whatever it is nested inside and whatever `type` it claims, since
every app exports a slightly different dialect.

## Credits

The theme, the bundled Ubuntu Bold face and the progress bar come from
[accessengine](https://github.com/mediaswing/accessengine), so the two apps look
and sound like they come from the same place. The two sound cues are CC0
recordings from freesound.org — see `assets/sounds/CREDITS.txt`. The font is
under the Ubuntu Font Licence; see `assets/fonts/`.

## Licence

MIT — see [LICENSE](LICENSE).
