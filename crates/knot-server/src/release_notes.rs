//! Release notes from CHANGELOG.md, compiled into the binary so the notes
//! always match the running version.
//!
//! The changelog follows Keep a Changelog: a `## [x.y.z] - YYYY-MM-DD`
//! heading per release, newest first, with `### Added`, `### Fixed`, …
//! sections under it. The preamble and `## [Unreleased]` are left out. Text
//! between a release heading and its first section is kept as a section
//! with an empty title.

use std::sync::LazyLock;

use semver::Version;
use serde::Serialize;

const CHANGELOG: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.md"));

/// Every release in CHANGELOG.md, newest first. Parsed on first use.
pub static RELEASES: LazyLock<Vec<Release>> = LazyLock::new(|| parse(CHANGELOG));

#[derive(Debug, Serialize)]
pub struct Release {
    pub version: Version,
    /// As written in the heading (`2026-10-21`).
    pub date: Option<String>,
    pub sections: Vec<Section>,
}

#[derive(Debug, Serialize)]
pub struct Section {
    /// `Added`, `Fixed`, …; empty for the text above the first section.
    pub title: String,
    /// The section's markdown, rendered and sanitised.
    pub html: String,
}

/// The releases in a Keep a Changelog file, in file order.
pub fn parse(changelog: &str) -> Vec<Release> {
    let mut releases = Vec::new();
    // `None` in the preamble and under a heading that is not a release
    // (`[Unreleased]`): those lines are dropped.
    let mut release: Option<Release> = None;
    let mut title = "";
    let mut body = String::new();
    let mut in_code = false;
    for line in changelog.lines() {
        // A `## ` line inside a fenced code block is not a heading.
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        let heading = match (line.strip_prefix("## "), line.strip_prefix("### ")) {
            _ if in_code => None,
            (Some(h2), _) => Some((2, h2)),
            (_, Some(h3)) => Some((3, h3)),
            _ => None,
        };
        let Some((level, text)) = heading else {
            body.push_str(line);
            body.push('\n');
            continue;
        };
        if let Some(r) = release.as_mut() {
            push_section(r, title, &body);
        }
        body.clear();
        if level == 2 {
            releases.extend(release.take());
            release = release_heading(text);
            title = "";
        } else {
            title = text.trim();
        }
    }
    if let Some(mut r) = release {
        push_section(&mut r, title, &body);
        releases.push(r);
    }
    releases
}

/// `[0.8.1] - 2026-10-21` → that release, with no sections yet. `None` for
/// `[Unreleased]` and anything else that is not a version.
fn release_heading(heading: &str) -> Option<Release> {
    let (label, rest) = heading.strip_prefix('[')?.split_once(']')?;
    let date = rest.trim().strip_prefix('-').map(str::trim);
    Some(Release {
        version: Version::parse(label).ok()?,
        date: date.filter(|d| !d.is_empty()).map(str::to_owned),
        sections: Vec::new(),
    })
}

fn push_section(release: &mut Release, title: &str, body: &str) {
    if title.is_empty() && body.trim().is_empty() {
        return;
    }
    release.sections.push(Section {
        title: title.to_owned(),
        html: render(body),
    });
}

/// The changelog is ours, but the web client injects this HTML, so it is
/// sanitised like any other rendered markdown (see `routes::public`). The
/// link definitions at the end of the file land in the last section, where
/// pulldown-cmark consumes them without output.
fn render(markdown: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let mut raw = String::new();
    html::push_html(&mut raw, Parser::new_ext(markdown, opts));
    ammonia::clean(&raw)
}

/// Releases the user has not seen: newer than `last_seen` and not newer
/// than the running version, newest first. Empty after a downgrade.
pub fn unseen<'a>(
    releases: &'a [Release],
    last_seen: &Version,
    running: &Version,
) -> Vec<&'a Version> {
    let mut out: Vec<_> = releases
        .iter()
        .map(|r| &r.version)
        .filter(|v| *v > last_seen && *v <= running)
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Changelog

All notable changes.

## [Unreleased]

### Added
- Not released yet.

## [0.10.0] - 2026-11-02

The intro of 0.10.0.

### Added
- **Bold** news.

```text
## not a heading
```

### Fixed
- [click](javascript:alert(1)) <img src=x onerror=alert(1)>

<script>alert(1)</script>

## [0.9.0] - 2026-10-30

### Changed
- The last section.

[Unreleased]: https://example.test/compare/v0.10.0...HEAD
[0.10.0]: https://example.test/compare/v0.9.0...v0.10.0
";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn titles(r: &Release) -> Vec<&str> {
        r.sections.iter().map(|s| s.title.as_str()).collect()
    }

    #[test]
    fn parses_every_release_in_the_changelog() {
        let versions: Vec<String> = RELEASES.iter().map(|r| r.version.to_string()).collect();
        // Releases cut later go on top; these must stay parseable.
        let known = [
            "0.8.1", "0.8.0", "0.7.0", "0.6.0", "0.5.0", "0.4.0", "0.3.0", "0.2.1", "0.2.0",
            "0.1.0",
        ]
        .map(String::from);
        assert!(versions.ends_with(&known), "got {versions:?}");
        // No release heading is skipped, so a format slip cannot hide one.
        let headings = CHANGELOG
            .lines()
            .filter(|l| l.starts_with("## ") && l != &"## [Unreleased]")
            .count();
        assert_eq!(versions.len(), headings, "got {versions:?}");
        assert!(
            RELEASES.windows(2).all(|w| w[0].version > w[1].version),
            "releases must be newest first: {versions:?}"
        );
        assert!(RELEASES.iter().all(|r| r.date.is_some()));
        assert!(RELEASES.iter().all(|r| !r.sections.is_empty()));
    }

    #[test]
    fn changelog_sections_are_kept_as_written() {
        let find = |s: &str| RELEASES.iter().find(|r| r.version == v(s)).unwrap();
        let r081 = find("0.8.1");
        assert_eq!(r081.date.as_deref(), Some("2026-10-21"));
        assert_eq!(titles(r081), ["Upgrading", "Security"]);
        assert!(r081.sections[0].html.starts_with("<ul>"));
        // 0.2.1 opens with paragraphs before its first `###`.
        let r021 = find("0.2.1");
        assert_eq!(r021.sections[0].title, "");
        assert!(
            r021.sections[0]
                .html
                .contains("<strong>The Helm chart could never be installed</strong>"),
            "got {}",
            r021.sections[0].html
        );
        // The link definitions at the end of the file render to nothing.
        let r010 = find("0.1.0");
        assert!(r010.sections.iter().all(|s| !s.html.contains("compare/")));
    }

    #[test]
    fn skips_preamble_and_unreleased() {
        let releases = parse(SAMPLE);
        let versions: Vec<_> = releases.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, ["0.10.0", "0.9.0"]);
        let all_html: String = releases
            .iter()
            .flat_map(|r| &r.sections)
            .map(|s| s.html.as_str())
            .collect();
        assert!(!all_html.contains("All notable"), "{all_html}");
        assert!(!all_html.contains("Not released yet"), "{all_html}");
        assert!(!all_html.contains("example.test"), "{all_html}");
    }

    #[test]
    fn keeps_text_above_the_first_section() {
        let releases = parse(SAMPLE);
        assert_eq!(releases[0].date.as_deref(), Some("2026-11-02"));
        assert_eq!(titles(&releases[0]), ["", "Added", "Fixed"]);
        assert_eq!(
            releases[0].sections[0].html,
            "<p>The intro of 0.10.0.</p>\n"
        );
        assert_eq!(titles(&releases[1]), ["Changed"]);
    }

    #[test]
    fn hash_lines_in_code_blocks_are_not_headings() {
        let added = &parse(SAMPLE)[0].sections[1];
        assert!(added.html.contains("<strong>Bold</strong>"));
        assert!(added.html.contains("## not a heading"), "{}", added.html);
    }

    #[test]
    fn sanitises_the_html() {
        let fixed = &parse(SAMPLE)[0].sections[2];
        assert!(!fixed.html.contains("<script"), "{}", fixed.html);
        assert!(!fixed.html.contains("javascript:"), "{}", fixed.html);
        assert!(!fixed.html.contains("onerror"), "{}", fixed.html);
        assert!(fixed.html.contains("click"), "{}", fixed.html);
    }

    fn releases(versions: &[&str]) -> Vec<Release> {
        versions
            .iter()
            .map(|s| Release {
                version: v(s),
                date: None,
                sections: Vec::new(),
            })
            .collect()
    }

    fn unseen_str(releases: &[Release], last_seen: &str, running: &str) -> Vec<String> {
        unseen(releases, &v(last_seen), &v(running))
            .into_iter()
            .map(Version::to_string)
            .collect()
    }

    #[test]
    fn unseen_is_newer_than_last_seen_up_to_running_newest_first() {
        let rs = releases(&["0.10.0", "0.9.0", "0.8.1", "0.8.0"]);
        // Semver, not string order: "0.10.0" < "0.9.0" as strings.
        assert_eq!(
            unseen_str(&rs, "0.8.0", "0.10.0"),
            ["0.10.0", "0.9.0", "0.8.1"]
        );
        assert_eq!(unseen_str(&rs, "0.8.1", "0.9.0"), ["0.9.0"]);
        // A changelog that is out of order still yields newest first.
        let shuffled = releases(&["0.8.1", "0.10.0", "0.9.0"]);
        assert_eq!(
            unseen_str(&shuffled, "0.8.0", "0.10.0"),
            ["0.10.0", "0.9.0", "0.8.1"]
        );
    }

    #[test]
    fn nothing_is_unseen_when_caught_up_or_downgraded() {
        let rs = releases(&["0.10.0", "0.9.0", "0.8.1"]);
        assert!(unseen_str(&rs, "0.10.0", "0.10.0").is_empty());
        assert!(unseen_str(&rs, "0.10.0", "0.9.0").is_empty());
    }
}
