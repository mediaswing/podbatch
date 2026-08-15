# Podcast Downloader

Point it at the OPML file your podcast app exports and it downloads the
episodes, giving every podcast a folder of its own under `~/podcasts`.

It is a desktop app rather than a script because choosing which of forty
subscriptions to fetch, and watching sixty large files come down over a slow
connection, are both things a window does better than a terminal.

## The window

Two tabs, both spanning the full width of the window.

**Downloads** is where the work happens. The subscription list goes in at the
top — choose an OPML file, or drop one onto the window. Underneath, the pane is
split: the podcasts the file contains are listed on the left with a tick beside
each, and what the run is doing is written on the right. The progress bar spans
the width beneath both.

Everything arrives ticked, because the usual answer is "all of them"; untick the
few you don't want. **All** and **None** are there for when it isn't.

**Settings** holds what gets set once and then left alone:

| Setting | Default | What it does |
| --- | --- | --- |
| Save episodes in | `~/podcasts` | Each podcast gets a subfolder of its own in here. |
| Skip episodes already downloaded | on | Leaves a file alone when it is already on disk at the size the feed claims. Turn it off to fetch everything again. |
| Only the newest *N* episodes | off | Takes the first *N* items in each feed, which is the newest *N* — feeds are published newest-first. |
| Downloads at once | 4 | How many files are fetched in parallel, across all podcasts. |
| Sound when finished | on | One cue if everything downloaded, another if anything failed. |

## What it writes

```
~/podcasts/
  Some Podcast/
    2025-08-05 - The Episode Title.mp3
    2025-07-29 - An Earlier One.mp3
```

Folder names come from the OPML entry rather than the feed's own title, so they
are the same on every run and can be worked out before a single feed is fetched.
Episode files are named from the publication date and title, so they sort into
order. Anything a filesystem would object to is replaced, and two episodes with
the same name get `(2)`, `(3)` and so on.

The extension comes from the enclosure URL when it has a believable one and from
the MIME type otherwise — plenty of feeds serve episodes from a tracking URL
with no extension at all.

### Interrupted downloads

An episode is written to a `.part` file and only renamed into place once it has
arrived in full, so a run that is stopped or killed never leaves a truncated
file that looks finished. Start again and it resumes from where it stopped,
using an HTTP range request when the server supports one and starting over when
it doesn't.

If a file on disk is a different size from the one the feed declares, it is
downloaded again — that mismatch is what a copy interrupted after the rename
looks like.

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
the right bytes, that a half-finished `.part` file resumes rather than starting
over, and that a wrong-sized file is fetched again.

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
