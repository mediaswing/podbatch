//! Small helpers: filename hygiene, byte formatting, date parsing.

/// Characters that are illegal on Windows, plus the path separators and control
/// characters. We sanitise for the strictest common denominator so that a
/// library copied onto another machine keeps working.
const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

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

    let cleaned = if trimmed.is_empty() { "untitled" } else { trimmed };

    // Leave room for a date prefix, an extension and a ".part" suffix inside the
    // usual 255-byte limit.
    truncate_chars(cleaned, 150)
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

/// Parse the date portion of an RFC 2822 timestamp (the format RSS `pubDate`
/// uses) into `YYYY-MM-DD`. Returns `None` for anything we don't recognise --
/// the date prefix is a nicety, never a hard requirement.
pub fn rfc2822_date(input: &str) -> Option<String> {
    let s = input.trim();
    // Optional "Tue, " day-of-week prefix.
    let s = match s.split_once(", ") {
        Some((_, rest)) => rest.trim(),
        None => s,
    };

    let mut parts = s.split_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()?.to_ascii_lowercase().as_str() {
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
    };
    let year_raw: u32 = parts.next()?.parse().ok()?;
    // RFC 822 allowed two-digit years and some feeds still emit them.
    let year = match year_raw {
        0..=49 => 2000 + year_raw,
        50..=99 => 1900 + year_raw,
        y => y,
    };

    if !(1..=31).contains(&day) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
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

    #[test]
    fn parses_rss_dates() {
        assert_eq!(
            rfc2822_date("Tue, 05 Aug 2025 10:00:00 GMT").as_deref(),
            Some("2025-08-05")
        );
        assert_eq!(rfc2822_date("5 Aug 25 10:00 -0400").as_deref(), Some("2025-08-05"));
        assert_eq!(rfc2822_date("last thursday"), None);
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
