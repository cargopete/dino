//! Sitemaps: the `<loc>` entries of a urlset, or the child sitemaps of an index.

use std::io::Read as _;

use regex::Regex;

pub enum Sitemap {
    Pages(Vec<String>),
    Index(Vec<String>),
}

/// The bytes as text, gunzipped first when they are gzip.
#[must_use]
pub fn text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = String::new();
        if flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut out)
            .is_ok()
        {
            return out;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[must_use]
pub fn parse(xml: &str) -> Sitemap {
    let loc = Regex::new(r"(?s)<loc>\s*(.*?)\s*</loc>").expect("a fixed regex compiles");
    let found = loc
        .captures_iter(xml)
        .map(|c| {
            c[1].replace("&amp;", "&")
                .replace("<![CDATA[", "")
                .replace("]]>", "")
                .trim()
                .to_owned()
        })
        .filter(|url| !url.is_empty())
        .collect();
    if xml.contains("<sitemapindex") {
        Sitemap::Index(found)
    } else {
        Sitemap::Pages(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_urlset_gives_its_pages() {
        let xml = "<urlset><url><loc> https://a.test/x?a=1&amp;b=2 </loc></url><url><loc><![CDATA[https://a.test/y]]></loc></url></urlset>";
        let Sitemap::Pages(pages) = parse(xml) else {
            panic!("not pages")
        };
        assert_eq!(pages, ["https://a.test/x?a=1&b=2", "https://a.test/y"]);
    }

    #[test]
    fn an_index_gives_its_sitemaps() {
        let xml = r#"<?xml version="1.0"?><sitemapindex><sitemap><loc>https://a.test/s1.xml.gz</loc></sitemap></sitemapindex>"#;
        assert!(matches!(parse(xml), Sitemap::Index(maps) if maps == ["https://a.test/s1.xml.gz"]));
    }

    #[test]
    fn gzip_is_opened() {
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"<urlset><loc>https://a.test/</loc></urlset>")
            .unwrap();
        assert!(text(&gz.finish().unwrap()).contains("https://a.test/"));
    }
}
