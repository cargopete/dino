//! A page read from its HTML alone, without a browser, into the same
//! [`Reading`] the browser gives. Most sites need nothing more, and this is
//! several times faster than drawing the page.

use scraper::ElementRef;
use scraper::Html;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use crate::Link;
use crate::Reading;

/// Less text than this, and the page is probably a shell a script fills in.
const SHELL_TEXT: usize = 500;
const TEXT_WIDTH: usize = 120;

fn select(selector: &str) -> Selector {
    Selector::parse(selector).expect("a fixed selector parses")
}

fn text_of(element: ElementRef<'_>) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Class or id words that mark a menu, a banner or other furniture.
const FURNITURE: &[&str] = &[
    "nav",
    "menu",
    "sidebar",
    "breadcrumb",
    "cookie",
    "consent",
    "gdpr",
    "banner",
    "share",
    "social",
    "related",
    "newsletter",
    "modal",
    "popup",
    "footer",
    "header",
];

fn is_furniture(element: ElementRef<'_>, whole_body: bool) -> bool {
    let value = element.value();
    let tag = value.name();
    if matches!(
        tag,
        "nav" | "aside" | "script" | "style" | "noscript" | "template" | "svg" | "form" | "iframe"
    ) {
        return true;
    }
    // A header or footer inside an article is the article's own; on the whole body it is the site's.
    if whole_body && matches!(tag, "header" | "footer") {
        return true;
    }
    let role = value.attr("role").unwrap_or_default();
    if matches!(role, "navigation" | "banner" | "contentinfo" | "dialog") {
        return true;
    }
    let names = format!(
        "{} {}",
        value.id().unwrap_or_default(),
        value.attr("class").unwrap_or_default()
    )
    .to_ascii_lowercase();
    let marked = names
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| FURNITURE.contains(&word));
    marked && text_of(element).len() < 3_000
}

/// `root`'s HTML with menus, banners and the like cut out.
fn without_furniture(root: ElementRef<'_>, whole_body: bool) -> String {
    let mut out = root.html();
    for node in root.descendants().skip(1) {
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };
        if is_furniture(element, whole_body) {
            // An ancestor already cut takes this with it, and the replace finds nothing.
            out = out.replacen(&element.html(), "", 1);
        }
    }
    out
}

/// The reading, and whether the page looks as though it needs a browser to draw it.
#[must_use]
pub fn read(url: &Url, status: u16, html: &str) -> (Reading, bool) {
    let document = Html::parse_document(html);
    let first = |selector: &str| document.select(&select(selector)).next();

    let title = first("title").map(text_of).unwrap_or_default();
    let lang = first("html")
        .and_then(|el| el.value().attr("lang"))
        .map(str::to_owned);
    let canonical = first("link[rel=canonical]")
        .and_then(|el| el.value().attr("href"))
        .and_then(|href| url.join(href).ok())
        .map(String::from);

    let mut meta = serde_json::Map::new();
    for element in document.select(&select("meta[content]")) {
        let attrs = element.value();
        let Some(key) = attrs.attr("name").or_else(|| attrs.attr("property")) else {
            continue;
        };
        let lower = key.to_ascii_lowercase();
        let wanted = ["description", "keywords", "author"].contains(&lower.as_str())
            || ["og:", "twitter:", "article:", "product:"]
                .iter()
                .any(|prefix| lower.starts_with(prefix));
        if wanted {
            meta.insert(
                key.to_owned(),
                Value::String(attrs.attr("content").unwrap_or_default().to_owned()),
            );
        }
    }

    let jsonld = document
        .select(&select(r#"script[type="application/ld+json"]"#))
        .filter_map(|script| serde_json::from_str(&script.text().collect::<String>()).ok())
        .collect();

    let mut links: Vec<Link> = Vec::new();
    for anchor in document.select(&select("a[href]")) {
        let Some(href) = anchor
            .value()
            .attr("href")
            .and_then(|href| url.join(href).ok())
        else {
            continue;
        };
        if !matches!(href.scheme(), "http" | "https") || links.len() >= 3_000 {
            continue;
        }
        let href = String::from(href);
        if links.iter().all(|link| link.href != href) {
            let text: String = text_of(anchor).chars().take(200).collect();
            links.push(Link { href, text });
        }
    }

    let main = ["article", "main", "[role=main]", "#content", "#main"]
        .iter()
        .filter_map(|selector| first(selector))
        .find(|element| text_of(*element).len() > 400);
    let source = match main.or_else(|| first("body")) {
        Some(root) => without_furniture(root, main.is_none()),
        None => html.to_owned(),
    };
    // Footnotes would list every link a second time; the links are kept apart anyway.
    let markdown = html2text::config::plain()
        .link_footnotes(false)
        .string_from_read(source.as_bytes(), TEXT_WIDTH)
        .unwrap_or_default();

    let visible = markdown.split_whitespace().map(str::len).sum::<usize>();
    let says_script = html.contains("enable JavaScript") || html.contains("requires JavaScript");
    let shell = visible < SHELL_TEXT
        || (links.len() < 3 && visible < 2_000)
        || (says_script && visible < 2_000);

    let reading = Reading {
        url: url.to_string(),
        canonical,
        status: Some(status),
        title,
        lang,
        markdown: markdown.trim().to_owned(),
        links,
        jsonld,
        meta,
        actions: Vec::new(),
    };
    (reading, shell)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://shop.test/a/").unwrap()
    }

    #[test]
    fn a_static_page_is_read_whole() {
        let body = "<p>A kettle that boils quickly. </p>".repeat(30);
        let html = format!(
            r#"<html lang="bg"><head><title> Kettles </title><link rel="canonical" href="/kettles">
            <meta property="og:image" content="https://shop.test/k.jpg">
            <script type="application/ld+json">{{"@type":"Product"}}</script></head>
            <body><nav><a href="/">Home</a><a href="b">B</a><a href="mailto:x@y.z">mail</a><a href="https://other.test/c#x">C</a></nav>
            <article><h1>Kettles</h1>{body}</article></body></html>"#
        );
        let (reading, shell) = read(&url(), 200, &html);
        assert!(!shell);
        assert_eq!(reading.title, "Kettles");
        assert_eq!(reading.lang.as_deref(), Some("bg"));
        assert_eq!(
            reading.canonical.as_deref(),
            Some("https://shop.test/kettles")
        );
        assert_eq!(reading.meta["og:image"], "https://shop.test/k.jpg");
        assert_eq!(reading.jsonld[0]["@type"], "Product");
        let hrefs: Vec<&str> = reading.links.iter().map(|l| l.href.as_str()).collect();
        assert_eq!(
            hrefs,
            [
                "https://shop.test/",
                "https://shop.test/a/b",
                "https://other.test/c#x"
            ]
        );
        assert!(reading.markdown.contains("boils quickly"));
        assert!(
            !reading.markdown.contains("Home"),
            "the article is chosen over the nav"
        );
    }

    #[test]
    fn a_page_without_an_article_loses_its_menus_not_its_text() {
        let body = "<p>Inflation was 3.2% in October. </p>".repeat(20);
        let menu = (0..200).fold(String::new(), |mut menu, n| {
            use std::fmt::Write as _;
            let _ = write!(menu, r#"<li><a href="/s/{n}">Section {n}</a></li>"#);
            menu
        });
        let html = format!(
            r#"<html><body><header><a href="/">NSI</a></header><div class="main-menu"><ul>{menu}</ul></div>
            <div class="page"><h1>Inflation</h1>{body}</div><footer>Address, phone</footer></body></html>"#
        );
        let (reading, _) = read(&url(), 200, &html);
        assert!(reading.markdown.contains("Inflation was 3.2%"));
        assert!(
            !reading.markdown.contains("Section 7"),
            "{}",
            &reading.markdown[..300]
        );
        assert!(!reading.markdown.contains("Address, phone"));
        assert!(!reading.markdown.contains("[1]:"), "no link footnotes");
        assert_eq!(reading.links.len(), 201, "the links are still all kept");
    }

    #[test]
    fn a_script_shell_asks_for_a_browser() {
        let html = r#"<html><head><title>Shop</title></head><body><div id="root"></div>
            <noscript>You need to enable JavaScript to run this app.</noscript><script src="/app.js"></script></body></html>"#;
        let (_, shell) = read(&url(), 200, html);
        assert!(shell);
    }
}
