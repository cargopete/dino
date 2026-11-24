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

/// Less text than this on a page with scripts, and it is probably a shell they fill in.
const SHELL_TEXT: usize = 500;
const TEXT_WIDTH: usize = 120;

fn select(selector: &str) -> Selector {
    Selector::parse(selector).expect("a fixed selector parses")
}

/// Text a reader would see: `text()` alone also yields the source of every script and style.
fn visible_text(element: ElementRef<'_>) -> usize {
    element
        .children()
        .map(|child| match ElementRef::wrap(child) {
            Some(child)
                if matches!(
                    child.value().name(),
                    "script" | "style" | "noscript" | "template"
                ) =>
            {
                0
            }
            Some(child) => visible_text(child),
            None => child
                .value()
                .as_text()
                .map_or(0, |text| text.split_whitespace().map(str::len).sum()),
        })
        .sum()
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
    );
    if words(&names).any(|word| FURNITURE.contains(&word.as_str()))
        && text_of(element).len() < 3_000
    {
        return true;
    }
    // Unlabelled menus: many links, and little text that is not a link.
    if matches!(tag, "ul" | "ol" | "div" | "section" | "table") {
        let links = element.select(&select("a")).count();
        if links >= 15 {
            let text = text_of(element).len().max(1);
            let linked: usize = element.select(&select("a")).map(|a| text_of(a).len()).sum();
            return linked * 10 >= text * 7;
        }
    }
    false
}

/// The words of an id or class, split at punctuation and at capitals: `statsMenu` is `stats` and `menu`.
fn words(names: &str) -> impl Iterator<Item = String> + '_ {
    let mut spaced = String::with_capacity(names.len() + 8);
    let mut previous_lower = false;
    for c in names.chars() {
        if c.is_ascii_uppercase() && previous_lower {
            spaced.push(' ');
        }
        previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        spaced.push(if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            ' '
        });
    }
    spaced
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
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
    // Footnotes would list every link a second time; the links are kept apart anyway.
    let convert = |source: &str| {
        html2text::config::plain()
            .link_footnotes(false)
            .string_from_read(source.as_bytes(), TEXT_WIDTH)
            .unwrap_or_default()
    };
    let markdown = match main.or_else(|| first("body")) {
        Some(root) => {
            let cut = convert(&without_furniture(root, main.is_none()));
            // A page that is nothing but a list of links keeps its list.
            if cut.trim().len() < 200 && text_of(root).len() > 1_000 {
                convert(&root.html())
            } else {
                cut
            }
        }
        None => convert(html),
    };

    // Counted before the furniture is cut, or an index page of links would look like a shell.
    let visible = first("body").map_or(0, visible_text);
    // Without a script nothing would fill the page in, so a browser draws what is already here.
    let scripted = document
        .select(&select(r#"script:not([type="application/ld+json"])"#))
        .any(|script| {
            script.value().attr("src").is_some() || !script.text().all(|t| t.trim().is_empty())
        });
    let says_script = html.contains("enable JavaScript") || html.contains("requires JavaScript");
    let shell = scripted && (visible < SHELL_TEXT || (says_script && visible < 2_000));

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
    fn camel_case_and_unlabelled_menus_are_furniture() {
        assert_eq!(
            words("tree1 statsMenu").collect::<Vec<_>>(),
            ["tree1", "stats", "menu"]
        );
        let items = (0..20).fold(String::new(), |mut list, n| {
            use std::fmt::Write as _;
            let _ = write!(list, r#"<li><a href="/x/{n}">Dataset number {n}</a></li>"#);
            list
        });
        let body = "<p>Employment rose by 1.4% in the third quarter. </p>".repeat(20);
        let html = format!(
            r#"<html><body><ul id="tree1">{items}</ul><div class="row">{body}</div></body></html>"#
        );
        let (reading, shell) = read(&url(), 200, &html);
        assert!(!reading.markdown.contains("Dataset number 3"));
        assert!(reading.markdown.contains("Employment rose"));
        assert!(!shell);
        let index = format!(
            "<html><body><h1>Datasets</h1><ul>{}</ul></body></html>",
            items.repeat(3)
        );
        let (_, shell) = read(&url(), 200, &index);
        assert!(!shell, "an index of links is a page, not a shell");
    }

    #[test]
    fn a_page_a_script_fills_from_inline_data_asks_for_a_browser() {
        let quotes =
            r#"{"text": "The world as we have created it is a process of our thinking."}, "#
                .repeat(60);
        let html = format!(
            r#"<html><head><title>Quotes to Scrape</title></head><body><div class="container">
            <h1><a href="/">Quotes to Scrape</a></h1><a href="/login">Login</a>
            <script src="jquery.js"></script><script>var data = [{quotes}]; render(data);</script>
            <ul class="pager"><li><a href="/js/page/2/">Next</a></li></ul></div>
            <footer>Quotes by: <a href="https://goodreads.com">GoodReads.com</a></footer></body></html>"#
        );
        let (_, shell) = read(&url(), 200, &html);
        assert!(shell, "the script's data is not text a reader sees");
    }

    #[test]
    fn a_shop_shell_with_a_tag_manager_asks_for_a_browser() {
        let html = format!(
            r#"<html><head><style>{}</style><script>{}</script></head><body>
            <noscript><iframe src="https://www.googletagmanager.com/ns.html?id=GTM-X"></iframe></noscript>
            <div id="app"></div><script src="/app.js"></script></body></html>"#,
            ".banner { color: red; } ".repeat(100),
            "window.dataLayer = window.dataLayer || []; ".repeat(100),
        );
        let (_, shell) = read(&url(), 200, &html);
        assert!(shell);
    }

    #[test]
    fn a_short_page_without_scripts_is_not_drawn() {
        let html = r#"<html><head><title>Example Domain</title><style>body { margin: 0 }</style></head>
            <body><div><h1>Example Domain</h1><p>This domain is for use in documentation examples
            without needing permission. Avoid use in operations.</p>
            <p><a href="https://iana.org/domains/example">Learn more</a></p></div></body></html>"#;
        let (reading, shell) = read(&url(), 200, html);
        assert!(!shell, "a browser would draw the same text");
        assert!(reading.markdown.contains("documentation examples"));
    }

    #[test]
    fn a_script_shell_asks_for_a_browser() {
        let html = r#"<html><head><title>Shop</title></head><body><div id="root"></div>
            <noscript>You need to enable JavaScript to run this app.</noscript><script src="/app.js"></script></body></html>"#;
        let (_, shell) = read(&url(), 200, html);
        assert!(shell);
    }
}
