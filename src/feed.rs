//! RSS / Atom podcast feed parsing.
//!
//! We only care about the part of a feed that describes downloadable media:
//! each item's title, enclosure URL, size and date.

const ATOM_NS: &str = "http://www.w3.org/2005/Atom";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub title: String,
    pub url: String,
    /// Declared size in bytes, when the feed provides a believable one.
    pub length: Option<u64>,
    pub mime: Option<String>,
    /// When it was published, when we could parse the date. This is what the
    /// file is named after, so it is wanted to the minute rather than the day.
    pub published: Option<crate::util::Published>,
    /// The episode's own blurb, with any markup taken out of it. Goes into the
    /// file's tags, which is where a reader will look for it now that the file
    /// name is a timestamp.
    pub description: Option<String>,
}

/// Deliberately not carrying the channel's own `<title>`: folder names come
/// from the OPML entry instead, so they can be worked out before a single feed
/// is fetched. That keeps them stable from run to run and lets collisions be
/// resolved up front rather than by whichever feed happens to finish first.
#[derive(Debug, Clone, Default)]
pub struct Feed {
    pub episodes: Vec<Episode>,
}

#[derive(Debug)]
pub enum FeedError {
    Xml(roxmltree::Error),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedError::Xml(e) => write!(f, "feed is not valid XML: {e}"),
        }
    }
}

impl std::error::Error for FeedError {}

pub fn parse(text: &str) -> Result<Feed, FeedError> {
    let text = text.trim_start_matches('\u{feff}');
    let doc = roxmltree::Document::parse(text).map_err(FeedError::Xml)?;
    let root = doc.root_element();

    // RSS nests everything under <channel>; Atom puts it on the root <feed>.
    let container = root
        .children()
        .find(|n| n.is_element() && local_name(n) == "channel")
        .unwrap_or(root);

    let mut episodes = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for item in container
        .children()
        .filter(|n| n.is_element() && matches!(local_name(n), "item" | "entry"))
    {
        let Some((url, length, mime)) = enclosure(&item) else {
            // An item with no media attached (a text post, or a broken entry).
            continue;
        };

        // A feed may list the same media twice; the guid is the author's own
        // identity claim, so prefer it and fall back to the URL.
        let key = child_text(&item, "guid")
            .unwrap_or_else(|| url.clone())
            .to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);

        let title = child_text(&item, "title").unwrap_or_else(|| "Untitled Episode".to_string());

        let published = child_text(&item, "pubDate")
            .and_then(|d| crate::util::rfc2822(&d))
            .or_else(|| child_text(&item, "published").and_then(|d| crate::util::iso8601(&d)))
            .or_else(|| child_text(&item, "updated").and_then(|d| crate::util::iso8601(&d)));

        // `itunes:summary` is plain text where `description` is often HTML, but
        // it is also the one publishers leave stale; prefer the feed's own
        // description and take the markup out of it.
        let description = child_text(&item, "description")
            .or_else(|| child_text(&item, "summary"))
            .or_else(|| child_text(&item, "content"))
            .map(|d| crate::util::strip_html(&d))
            .filter(|d| !d.is_empty());

        episodes.push(Episode { title, url, length, mime, published, description });
    }

    Ok(Feed { episodes })
}

/// Find the media attachment for an item: an RSS `<enclosure>`, or an Atom
/// `<link rel="enclosure">`.
fn enclosure(item: &roxmltree::Node) -> Option<(String, Option<u64>, Option<String>)> {
    for child in item.children().filter(|n| n.is_element()) {
        let (url_attr, is_candidate) = match local_name(&child) {
            "enclosure" => ("url", true),
            "link" => (
                "href",
                attr(&child, "rel").is_some_and(|r| r.eq_ignore_ascii_case("enclosure")),
            ),
            _ => continue,
        };
        if !is_candidate {
            continue;
        }

        let url = attr(&child, url_attr)?.trim();
        // The same allowlist the subscription list is held to. A feed is a file
        // from outside, and an enclosure that names a `file://` is asking us to
        // read the disk and write what we find into a podcast folder.
        if url.is_empty() || !crate::util::looks_like_http(url) {
            continue;
        }

        // Feeds routinely declare a length of 0, or a placeholder, when they
        // don't actually know the size. Treat those as unknown.
        let length = attr(&child, "length")
            .and_then(|l| l.trim().parse::<u64>().ok())
            .filter(|&l| l > 0);

        let mime = attr(&child, "type")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);

        return Some((url.to_string(), length, mime));
    }
    None
}

/// Text of the first child with this local name, preferring an unnamespaced or
/// Atom element over one from an extension namespace such as `itunes:`.
fn child_text(node: &roxmltree::Node, name: &str) -> Option<String> {
    let mut fallback = None;
    for child in node.children().filter(|n| n.is_element()) {
        if !local_name(&child).eq_ignore_ascii_case(name) {
            continue;
        }
        let preferred = matches!(child.tag_name().namespace(), None | Some(ATOM_NS));
        let text = element_text(&child);
        if preferred {
            return text;
        }
        if fallback.is_none() {
            fallback = text;
        }
    }
    fallback
}

/// Concatenate an element's text, which CDATA sections split into several nodes.
fn element_text(node: &roxmltree::Node) -> Option<String> {
    let mut buf = String::new();
    for child in node.descendants().filter(|n| n.is_text()) {
        buf.push_str(child.text().unwrap_or_default());
    }
    let trimmed = buf.trim();
    // Atom's `<link>`-style empty elements shouldn't count as a value.
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn local_name<'a>(node: &roxmltree::Node<'a, 'a>) -> &'a str {
    node.tag_name().name()
}

fn attr<'a>(node: &roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name().eq_ignore_ascii_case(name))
        .map(|a| a.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <title>My Show</title>
    <itunes:title>Wrong Title</itunes:title>
    <item>
      <title><![CDATA[Episode ]]><![CDATA[One]]></title>
      <description><![CDATA[<p>A blurb &amp; a half.</p>]]></description>
      <pubDate>Tue, 05 Aug 2025 10:00:00 GMT</pubDate>
      <guid isPermaLink="false">ep-1</guid>
      <enclosure url="https://cdn.test/1.mp3" length="1234" type="audio/mpeg"/>
    </item>
    <item>
      <title>Duplicate of one</title>
      <guid>ep-1</guid>
      <enclosure url="https://cdn.test/1.mp3" length="1234" type="audio/mpeg"/>
    </item>
    <item>
      <title>No media here</title>
    </item>
    <item>
      <title>Unknown size</title>
      <enclosure url="https://cdn.test/2.m4a" length="0" type="audio/mp4"/>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parses_rss_items() {
        let feed = parse(RSS).unwrap();
        assert_eq!(feed.episodes.len(), 2);

        let first = &feed.episodes[0];
        assert_eq!(first.title, "Episode One");
        assert_eq!(first.url, "https://cdn.test/1.mp3");
        assert_eq!(first.length, Some(1234));
        assert_eq!(
            first.published,
            Some(crate::util::Published { year: 2025, month: 8, day: 5, hour: 10, minute: 0 })
        );
        assert_eq!(first.description.as_deref(), Some("A blurb & a half."));

        // length="0" means "we don't know", not "empty file".
        assert_eq!(feed.episodes[1].length, None);
    }

    #[test]
    fn parses_atom_entries() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom">
            <title>Atom Show</title>
            <entry>
              <title>Atom Ep</title>
              <updated>2025-08-05T10:00:00Z</updated>
              <link rel="alternate" href="https://x.test/page"/>
              <link rel="enclosure" href="https://cdn.test/a.mp3" length="99" type="audio/mpeg"/>
            </entry>
          </feed>"#;
        let feed = parse(atom).unwrap();
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(feed.episodes[0].url, "https://cdn.test/a.mp3");
        assert_eq!(
            feed.episodes[0].published.map(|p| p.stamp()).as_deref(),
            Some("050825-1000")
        );
    }

    /// An enclosure is a URL the downloader will fetch and write to disk, so it
    /// is held to the same two schemes the subscription list is.
    #[test]
    fn ignores_enclosures_that_are_not_http() {
        let rss = r#"<rss version="2.0"><channel>
            <item><title>Local</title><enclosure url="file:///etc/passwd" type="audio/mpeg"/></item>
            <item><title>Data</title><enclosure url="data:audio/mpeg;base64,AAAA"/></item>
            <item><title>Real</title><enclosure url="https://cdn.test/ok.mp3" type="audio/mpeg"/></item>
          </channel></rss>"#;
        let feed = parse(rss).unwrap();
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(feed.episodes[0].url, "https://cdn.test/ok.mp3");
    }

    #[test]
    fn ignores_non_enclosure_links() {
        let atom = r#"<feed xmlns="http://www.w3.org/2005/Atom">
            <entry><title>T</title><link rel="alternate" href="https://x.test/page"/></entry>
          </feed>"#;
        assert!(parse(atom).unwrap().episodes.is_empty());
    }
}
