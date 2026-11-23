//! URL normalisation, so one page is one frontier row.

use url::Url;

const TRACKING: [&str; 9] = [
    "fbclid", "gclid", "dclid", "msclkid", "mc_cid", "mc_eid", "yclid", "_ga", "igshid",
];

/// Drops the fragment, tracking parameters and a default port, keeps the rest.
/// Anything that is not http or https is `None`.
#[must_use]
pub fn normalise(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(None);
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !key.starts_with("utm_") && !TRACKING.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    Some(url.to_string())
}

#[must_use]
pub fn host(raw: &str) -> Option<String> {
    Url::parse(raw)
        .ok()?
        .host_str()
        .map(|host| host.trim_start_matches("www.").to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_and_fragments_go_the_rest_stays() {
        assert_eq!(
            normalise("HTTPS://Example.com:443/a/b?id=3&utm_source=x&fbclid=y#top").as_deref(),
            Some("https://example.com/a/b?id=3")
        );
        assert_eq!(
            normalise("https://example.com/?utm_medium=a").as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn only_the_web_is_crawled() {
        assert_eq!(normalise("mailto:a@b.c"), None);
        assert_eq!(normalise("javascript:void(0)"), None);
        assert_eq!(normalise("not a url"), None);
    }

    #[test]
    fn www_is_the_same_host() {
        assert_eq!(host("https://www.BNB.bg/x"), Some("bnb.bg".into()));
    }
}
