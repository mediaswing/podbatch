//! Small helpers: filename hygiene, byte formatting, date parsing.
//!
//! Publication times are what episode file names are made of, so the parsing
//! here is a little more careful than a nicety would need to be: a feed's
//! timestamp carries a zone, and the same instant written from two zones must
//! not come out as two different names — nor an episode published late at night
//! land on the wrong day. Everything is normalised to UTC before it becomes a
//! name.

/// Characters that are illegal on Windows, plus the path separators and control
/// characters. We sanitise for the strictest common denominator so that a
/// library copied onto another machine keeps working.
const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Windows treats these as device names rather than files, whatever the case
/// and even with an extension after them — `nul.mp3` is exactly as unusable
/// as `nul`. A show or episode titled just one of these words — "Con",
/// "Aux" and "Null" all being names real podcasts use — would otherwise sail
/// through the character check and then fail to ever be created.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Whether `name` is, up to its first dot, one of the names Windows reserves
/// for devices — the part an extension gets appended after.
fn is_windows_reserved(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name);
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(base))
}

/// Turn arbitrary feed text into something safe to use as a single path
/// component. Never returns an empty string.
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_space = false;

    for ch in name.chars() {
        let mapped = if ILLEGAL.contains(&ch) || ch.is_control() {
            ' '
        } else {
            ch
        };
        // Collapse runs of whitespace so replaced characters don't leave gaps.
        if mapped.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(mapped);
            last_was_space = false;
        }
    }

    // Trailing dots and spaces are stripped by Windows; leading dots hide the
    // file on Unix. Neither is what the feed author meant.
    let trimmed = out.trim().trim_end_matches('.').trim_start_matches('.').trim();

    let cleaned = if trimmed.is_empty() {
        "untitled".to_string()
    } else if is_windows_reserved(trimmed) {
        // An underscore keeps the word readable rather than replacing it
        // outright, and moves it off the exact name Windows refuses.
        format!("{trimmed}_")
    } else {
        trimmed.to_string()
    };

    // Leave room for a date prefix, an extension and a ".part" suffix inside the
    // usual 255-byte limit.
    truncate_chars(&cleaned, 150)
}

/// Truncate on a character boundary, not a byte boundary.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>().trim_end().to_string()
}

/// Extensions we'll accept from a URL. An allowlist rather than a shape test,
/// because plenty of enclosure URLs end in something that merely looks like an
/// extension — a redirector ending in `/example.com` would otherwise write
/// every episode of that show as a `.com` file.
const MEDIA_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "ogg", "oga", "opus", "wav", "flac", "aif", "aiff", "wma", "caf",
    "mp4", "m4v", "mov", "webm", "mkv", "avi",
];

/// Pull a file extension out of an enclosure URL, falling back to the MIME type.
/// Many feeds hand out tracking URLs with no extension at all.
pub fn extension_for(url: &str, mime: Option<&str>) -> String {
    // Strip query and fragment before looking for a dot.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if let Some(file) = path.rsplit('/').next()
        && let Some((_, ext)) = file.rsplit_once('.')
    {
        let ext = ext.trim().to_ascii_lowercase();
        if MEDIA_EXTENSIONS.contains(&ext.as_str()) {
            return ext;
        }
    }

    match mime.map(|m| m.split(';').next().unwrap_or(m).trim().to_ascii_lowercase()) {
        Some(m) => match m.as_str() {
            "audio/mpeg" | "audio/mp3" => "mp3",
            "audio/mp4" | "audio/x-m4a" | "audio/m4a" => "m4a",
            "audio/aac" => "aac",
            "audio/ogg" | "application/ogg" => "ogg",
            "audio/opus" => "opus",
            "audio/wav" | "audio/x-wav" => "wav",
            "audio/flac" | "audio/x-flac" => "flac",
            "video/mp4" => "mp4",
            "video/quicktime" => "mov",
            "video/x-m4v" => "m4v",
            _ => "mp3",
        }
        .to_string(),
        None => "mp3".to_string(),
    }
}

/// When an episode was published, in UTC and to the minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Published {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

impl Published {
    /// The file name an episode gets: `ddmmyy-hhmm`.
    pub fn stamp(&self) -> String {
        format!(
            "{:02}{:02}{:02}-{:02}{:02}",
            self.day,
            self.month,
            self.year.rem_euclid(100),
            self.hour,
            self.minute
        )
    }

    /// The same instant `minutes` earlier or later, rolling over days, months
    /// and years as it needs to. Used to take a feed's zone offset back off.
    fn shifted(self, minutes: i64) -> Self {
        let total = days_from_civil(self.year, self.month, self.day) * 1440
            + i64::from(self.hour) * 60
            + i64::from(self.minute)
            + minutes;
        let (year, month, day) = civil_from_days(total.div_euclid(1440));
        let time = total.rem_euclid(1440);
        Published {
            year,
            month,
            day,
            hour: (time / 60) as u32,
            minute: (time % 60) as u32,
        }
    }
}

/// Days since 1970-01-01, by Howard Hinnant's civil-date algorithm. Only ever
/// used to move a timestamp across a day boundary, so no calendar table and no
/// date library is needed for it.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { y + 1 } else { y } as i32, month, day)
}

/// A moment written for a log line: `2026-08-16 14:03:22.123Z`.
///
/// UTC, and marked as UTC. Finding the local zone offset portably needs either
/// a calendar library or a call into libc that isn't sound to make once threads
/// are running, and a log timestamped an hour out is worse than an honest `Z`.
pub fn utc_stamp(now: std::time::SystemTime) -> String {
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs() as i64;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time = secs.rem_euclid(86_400);

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60,
        since_epoch.subsec_millis()
    )
}

/// Parse an RFC 2822 timestamp — the format RSS `pubDate` uses. Returns `None`
/// for anything we don't recognise; a feed that won't say when an episode came
/// out doesn't get a timestamped name.
pub fn rfc2822(input: &str) -> Option<Published> {
    let s = input.trim();
    // Optional "Tue, " day-of-week prefix.
    let s = match s.split_once(',') {
        Some((_, rest)) => rest.trim(),
        None => s,
    };

    let mut parts = s.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = month_from_name(parts.next()?)?;
    let year_raw: i32 = parts.next()?.parse().ok()?;
    // RFC 822 allowed two-digit years and some feeds still emit them.
    let year = match year_raw {
        0..=49 => 2000 + year_raw,
        50..=99 => 1900 + year_raw,
        y => y,
    };

    if !(1..=31).contains(&day) {
        return None;
    }

    // A time is optional in RFC 822 and missing from the odd feed; midnight is
    // the conventional reading of a bare date.
    let (hour, minute) = match parts.next() {
        Some(time) => clock(time)?,
        None => (0, 0),
    };

    let published = Published { year, month, day, hour, minute };
    Some(published.shifted(-zone_offset(parts.next())))
}

/// Accept an ISO 8601 / RFC 3339 timestamp, which is what Atom's `published`
/// and `updated` carry.
pub fn iso8601(input: &str) -> Option<Published> {
    let s = input.trim();
    let (date, rest) = match s.split_once(['T', 't', ' ']) {
        Some((date, rest)) => (date, Some(rest)),
        None => (s, None),
    };

    let mut fields = date.split('-');
    let year: i32 = fields.next()?.parse().ok()?;
    let month: u32 = fields.next()?.parse().ok()?;
    let day: u32 = fields.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let Some(rest) = rest else {
        return Some(Published { year, month, day, hour: 0, minute: 0 });
    };

    // Split the time from its zone, which is `Z`, `+hh:mm` or `-hh:mm`.
    let split = rest.find(['Z', 'z', '+']).or_else(|| {
        // A minus can only be the zone here; the date is already off the front.
        rest.find('-')
    });
    let (time, zone) = match split {
        Some(at) => (&rest[..at], Some(&rest[at..])),
        None => (rest, None),
    };

    let (hour, minute) = clock(time)?;
    let published = Published { year, month, day, hour, minute };
    Some(published.shifted(-zone_offset(zone)))
}

fn month_from_name(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    })
}

/// `hh:mm` or `hh:mm:ss`, with the seconds thrown away — a name to the minute
/// is as fine-grained as anyone wants to read.
fn clock(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.trim().split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.trim().parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

/// Minutes east of UTC. An unreadable or absent zone counts as UTC, which is
/// what the timestamp then gets taken as saying.
fn zone_offset(zone: Option<&str>) -> i64 {
    let Some(zone) = zone.map(str::trim).filter(|z| !z.is_empty()) else {
        return 0;
    };

    // Numeric offsets: `+0100`, `-04:00`, `+01`.
    if let Some(sign) = match zone.as_bytes()[0] {
        b'+' => Some(1),
        b'-' => Some(-1),
        _ => None,
    } {
        let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
        let (hours, minutes) = match digits.len() {
            2 => (digits.parse::<i64>().ok(), Some(0)),
            4 => (
                digits[..2].parse::<i64>().ok(),
                digits[2..].parse::<i64>().ok(),
            ),
            _ => (None, None),
        };
        return match (hours, minutes) {
            (Some(h), Some(m)) if h < 24 && m < 60 => sign * (h * 60 + m),
            _ => 0,
        };
    }

    // The named zones RFC 822 allowed, which plenty of feeds still use.
    60 * match zone.to_ascii_uppercase().as_str() {
        "EST" => -5,
        "EDT" => -4,
        "CST" => -6,
        "CDT" => -5,
        "MST" => -7,
        "MDT" => -6,
        "PST" => -8,
        "PDT" => -7,
        // "GMT", "UT", "UTC", "Z" and anything unrecognised.
        _ => 0,
    }
}

/// Feed descriptions are routinely HTML, and an ID3 tag is plain text. Strip
/// the markup, put the handful of entities that survive it back, and collapse
/// the whitespace the markup was holding apart.
pub fn strip_html(input: &str) -> String {
    let mut text = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Only a `<` that starts something tag-shaped opens a tag. A blurb
        // reading "Is 3 < 5?" is prose, and treating that `<` as a tag would
        // swallow the rest of the description — which is the whole of what the
        // file's tags would have said about the episode.
        let opens_a_tag = chars[i] == '<'
            && chars
                .get(i + 1)
                .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));

        if !opens_a_tag {
            text.push(chars[i]);
            i += 1;
            continue;
        }

        match chars[i..].iter().position(|&c| c == '>') {
            Some(end) => {
                i += end + 1;
                // A tag stood between two words; keep them apart.
                text.push(' ');
            }
            // Markup left unclosed — truncated show notes usually. Everything
            // from here on is inside a tag that never ends, so there is nothing
            // further to keep.
            None => break,
        }
    }

    // Only the entities that survive the XML parser: those in a CDATA section,
    // which is how most feeds carry their HTML. Ordinary element text has
    // already been decoded once by roxmltree, so a publisher who wrote
    // `&amp;amp;` to mean a literal "&amp;" gets a bare "&" here. That is the
    // wrong answer for the rarer case and the right one for the common one,
    // which is the trade the other way round from leaving these alone.
    let text = text
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        // Last, or it would turn "&amp;lt;" into a tag bracket.
        .replace("&amp;", "&");

    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Human-readable byte count, e.g. `4.2 MB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A length of time written the way someone waiting for it would say it.
///
/// Deliberately coarse. This is only ever used for an estimate built on a
/// guessed connection speed, and "about 23 minutes" claims exactly as much
/// precision as that guess can support — "23m 41s" would claim far more.
pub fn human_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "no time at all".to_string();
    }

    let total = seconds.round() as u64;
    if total < 60 {
        return "less than a minute".to_string();
    }

    let minutes = (total as f64 / 60.0).round().max(1.0) as u64;
    if minutes < 90 {
        return format!("{minutes} minute{}", plural(minutes));
    }

    let hours = minutes / 60;
    let rest = minutes % 60;
    if rest == 0 {
        format!("{hours} hour{}", plural(hours))
    } else {
        format!("{hours} hour{} {rest} minute{}", plural(hours), plural(rest))
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Bytes per second as a rate someone can read, e.g. `2.4 MB/s`.
pub fn human_rate(bytes_per_second: f64) -> String {
    format!("{}/s", human_bytes(bytes_per_second.max(0.0) as u64))
}

/// How long `bytes` will take at `bytes_per_second`, in seconds.
///
/// `None` when there is no usable rate to divide by, which is the honest answer
/// rather than a number made up to fill the space.
pub fn transfer_seconds(bytes: u64, bytes_per_second: f64) -> Option<f64> {
    (bytes_per_second.is_finite() && bytes_per_second > 0.0)
        .then(|| bytes as f64 / bytes_per_second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_separators_and_collapses_space() {
        assert_eq!(sanitize("AC/DC:  Live?"), "AC DC Live");
        assert_eq!(sanitize("   "), "untitled");
        assert_eq!(sanitize(".hidden."), "hidden");
    }

    /// `CON`, `NUL` and the rest name a device on Windows, not a file — even
    /// with an extension after them — so a show or episode that sanitises to
    /// exactly one of these must not be handed back unchanged.
    #[test]
    fn sanitize_steers_clear_of_windows_device_names() {
        assert_eq!(sanitize("CON"), "CON_");
        assert_eq!(sanitize("con"), "con_");
        assert_eq!(sanitize("Nul"), "Nul_");
        assert_eq!(sanitize("Aux"), "Aux_");
        assert_eq!(sanitize("COM1"), "COM1_");
        assert_eq!(sanitize("Lpt3"), "Lpt3_");
        // A word that only starts with a reserved name is a real title, not a
        // device — "Console" must be left exactly as it was written.
        assert_eq!(sanitize("Console"), "Console");
        assert_eq!(sanitize("Nullable Podcast"), "Nullable Podcast");
    }

    #[test]
    fn sanitize_respects_char_boundaries() {
        let long = "é".repeat(400);
        let out = sanitize(&long);
        assert_eq!(out.chars().count(), 150);
    }

    #[test]
    fn extension_prefers_url_then_mime() {
        assert_eq!(extension_for("https://x.test/a/b.MP3?t=1", None), "mp3");
        assert_eq!(extension_for("https://x.test/track/9281", Some("audio/mp4")), "m4a");
        assert_eq!(extension_for("https://x.test/track/9281", None), "mp3");
        // A dotted hostname path segment isn't an extension, whatever it looks
        // like; fall through to the MIME type instead.
        assert_eq!(extension_for("https://x.test/redirect/example.com", None), "mp3");
        assert_eq!(
            extension_for("https://x.test/r/example.com", Some("audio/ogg")),
            "ogg"
        );
    }

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Published {
        Published { year, month, day, hour, minute }
    }

    #[test]
    fn parses_rss_dates() {
        assert_eq!(
            rfc2822("Tue, 05 Aug 2025 10:00:00 GMT"),
            Some(at(2025, 8, 5, 10, 0))
        );
        // Two-digit year, no seconds, and an offset that has to come back off.
        assert_eq!(rfc2822("5 Aug 25 10:00 -0400"), Some(at(2025, 8, 5, 14, 0)));
        // A bare date is midnight, not a refusal.
        assert_eq!(rfc2822("5 Aug 2025"), Some(at(2025, 8, 5, 0, 0)));
        assert_eq!(rfc2822("last thursday"), None);
    }

    #[test]
    fn parses_atom_dates() {
        assert_eq!(iso8601("2025-08-05T10:00:00Z"), Some(at(2025, 8, 5, 10, 0)));
        assert_eq!(
            iso8601("2025-08-05T10:30:00.500+02:30"),
            Some(at(2025, 8, 5, 8, 0))
        );
        assert_eq!(iso8601("2025-08-05"), Some(at(2025, 8, 5, 0, 0)));
        assert_eq!(iso8601("whenever"), None);
    }

    /// Taking the zone off can move the date, the month and the year, and a
    /// file name that got this wrong is wrong for good — it is what the episode
    /// is called from then on, and what the next run looks for to decide it is
    /// already here.
    #[test]
    fn zones_roll_the_calendar_over() {
        assert_eq!(
            rfc2822("Wed, 31 Dec 2025 23:30:00 -0100"),
            Some(at(2026, 1, 1, 0, 30))
        );
        assert_eq!(
            rfc2822("Thu, 1 Jan 2026 00:30:00 +0100"),
            Some(at(2025, 12, 31, 23, 30))
        );
        // A leap day, backwards over the year boundary it only has every four.
        assert_eq!(
            iso8601("2024-03-01T00:15:00+01:00"),
            Some(at(2024, 2, 29, 23, 15))
        );
    }

    #[test]
    fn stamps_are_ddmmyy_hhmm() {
        assert_eq!(at(2025, 8, 5, 10, 0).stamp(), "050825-1000");
        assert_eq!(at(2026, 12, 31, 23, 59).stamp(), "311226-2359");
        // Midnight is 0000, not blank: every stamp is the same width.
        assert_eq!(at(2025, 1, 1, 0, 0).stamp(), "010125-0000");
    }

    #[test]
    fn stamps_log_lines_in_utc() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(utc_stamp(UNIX_EPOCH), "1970-01-01 00:00:00.000Z");
        assert_eq!(
            utc_stamp(UNIX_EPOCH + Duration::from_millis(1_000_000_000_500)),
            "2001-09-09 01:46:40.500Z"
        );
    }

    #[test]
    fn strips_markup_from_descriptions() {
        assert_eq!(
            strip_html("<p>First.</p><p>Second &amp; last.</p>"),
            "First. Second & last."
        );
        assert_eq!(strip_html("a<br/>b"), "a b");
        assert_eq!(strip_html("  plain  text \n here "), "plain text here");
    }

    /// A `<` in prose is not a tag, and reading it as one used to drop
    /// everything after it — the rest of the episode's notes, gone from the
    /// file's tags with nothing to say it had been there.
    #[test]
    fn a_less_than_sign_in_prose_keeps_the_prose() {
        assert_eq!(strip_html("Is 3 < 5? Yes, and so is 2."), "Is 3 < 5? Yes, and so is 2.");
        assert_eq!(strip_html("a < b and <b>c</b>"), "a < b and c");
        // Markup that never closes takes the rest with it; there is nothing
        // outside the tag left to keep.
        assert_eq!(strip_html("kept <b broken"), "kept");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }

    #[test]
    fn formats_durations_coarsely() {
        assert_eq!(human_duration(0.0), "no time at all");
        assert_eq!(human_duration(12.0), "less than a minute");
        assert_eq!(human_duration(61.0), "1 minute");
        assert_eq!(human_duration(1500.0), "25 minutes");
        // 90 minutes is where it starts counting in hours.
        assert_eq!(human_duration(5400.0), "1 hour 30 minutes");
        assert_eq!(human_duration(7200.0), "2 hours");
        // A rate of zero produces an infinite estimate upstream; it must not
        // reach the user as "inf minutes".
        assert_eq!(human_duration(f64::INFINITY), "no time at all");
    }

    #[test]
    fn an_estimate_needs_a_real_rate_to_stand_on() {
        assert_eq!(transfer_seconds(1_000_000, 1_000_000.0), Some(1.0));
        assert_eq!(transfer_seconds(1_000_000, 0.0), None);
        assert_eq!(transfer_seconds(1_000_000, f64::NAN), None);
    }

    #[test]
    fn formats_rates() {
        assert_eq!(human_rate(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
    }
}
