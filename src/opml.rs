//! OPML subscription-list parsing.
//!
//! Every podcast app exports a slightly different OPML dialect, so we stay
//! permissive: walk the whole tree and treat any `outline` carrying a feed URL
//! attribute as a subscription, regardless of nesting or `type`.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub title: String,
    pub url: String,
}

#[derive(Debug)]
pub enum OpmlError {
    Io(std::io::Error),
    Xml(roxmltree::Error),
    Empty,
}

impl std::fmt::Display for OpmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpmlError::Io(e) => write!(f, "could not read file: {e}"),
            OpmlError::Xml(e) => write!(f, "not valid XML: {e}"),
            OpmlError::Empty => write!(f, "no feeds found in this OPML file"),
        }
    }
}

impl std::error::Error for OpmlError {}

pub fn parse_file(path: &Path) -> Result<Vec<Subscription>, OpmlError> {
    let text = std::fs::read_to_string(path).map_err(OpmlError::Io)?;
    parse_str(&text)
}

pub fn parse_str(text: &str) -> Result<Vec<Subscription>, OpmlError> {
    // Some exports carry a UTF-8 BOM, which roxmltree rejects as junk before the
    // declaration.
    let text = text.trim_start_matches('\u{feff}');
    let doc = roxmltree::Document::parse(text).map_err(OpmlError::Xml)?;

    let mut subs: Vec<Subscription> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for node in doc.descendants().filter(|n| n.has_tag_name("outline")) {
        let Some(url) = attr_ci(&node, "xmlUrl").or_else(|| attr_ci(&node, "url")) else {
            // A container outline (a folder/category), not a feed.
            continue;
        };
        let url = url.trim();
        if url.is_empty() || !crate::util::looks_like_http(url) {
            continue;
        }

        let title = attr_ci(&node, "text")
            .or_else(|| attr_ci(&node, "title"))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("Untitled Podcast")
            .to_string();

        // The same feed can appear in several categories; keep the first.
        let key = url.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        subs.push(Subscription {
            title,
            url: url.to_string(),
        });
    }

    if subs.is_empty() {
        return Err(OpmlError::Empty);
    }
    Ok(subs)
}

/// OPML attribute casing is inconsistent in the wild (`xmlUrl`, `xmlurl`,
/// `XMLURL`), so match case-insensitively.
fn attr_ci<'a>(node: &roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name().eq_ignore_ascii_case(name))
        .map(|a| a.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="1.0">
  <head><title>Subscriptions</title></head>
  <body>
    <outline text="Tech">
      <outline type="rss" text="Show One" xmlUrl="https://one.test/feed.xml"/>
      <outline type="rss" title="Show Two" xmlurl="https://two.test/feed.xml"/>
    </outline>
    <outline type="rss" text="Show One Again" xmlUrl="https://ONE.test/feed.xml"/>
    <outline type="rss" text="Bad" xmlUrl="itpc://nope.test/feed"/>
    <outline text="Just A Folder"/>
  </body>
</opml>"#;

    #[test]
    fn extracts_nested_feeds_and_dedupes() {
        let subs = parse_str(SAMPLE).unwrap();
        assert_eq!(
            subs,
            vec![
                Subscription { title: "Show One".into(), url: "https://one.test/feed.xml".into() },
                Subscription { title: "Show Two".into(), url: "https://two.test/feed.xml".into() },
            ]
        );
    }

    #[test]
    fn reports_empty_files() {
        let doc = r#"<opml><body><outline text="Folder"/></body></opml>"#;
        assert!(matches!(parse_str(doc), Err(OpmlError::Empty)));
    }

    #[test]
    fn tolerates_a_bom() {
        let subs = parse_str(&format!("\u{feff}{SAMPLE}")).unwrap();
        assert_eq!(subs.len(), 2);
    }
}
