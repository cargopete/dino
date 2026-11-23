//! `robots.txt`, the group for `dino` or else `*`, longest match wins (RFC 9309).

#[derive(Debug, Default, Clone)]
pub struct Robots {
    rules: Vec<(bool, String)>,
    pub crawl_delay: Option<f64>,
}

impl Robots {
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut groups: Vec<(Vec<String>, Self)> = Vec::new();
        let mut in_agents = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            match key.as_str() {
                "user-agent" => {
                    if !in_agents {
                        groups.push((Vec::new(), Self::default()));
                    }
                    in_agents = true;
                    if let Some((agents, _)) = groups.last_mut() {
                        agents.push(value.to_ascii_lowercase());
                    }
                }
                "allow" | "disallow" | "crawl-delay" => {
                    in_agents = false;
                    let Some((_, group)) = groups.last_mut() else {
                        continue;
                    };
                    if key == "crawl-delay" {
                        group.crawl_delay = value.parse().ok();
                    } else if !value.is_empty() {
                        group.rules.push((key == "allow", value.to_owned()));
                    }
                }
                _ => {}
            }
        }
        let pick = |name: &str| {
            groups
                .iter()
                .find(|(agents, _)| agents.iter().any(|agent| agent == name))
        };
        pick("dino")
            .or_else(|| pick("*"))
            .map(|(_, group)| group.clone())
            .unwrap_or_default()
    }

    /// Whether `path` (with its query) may be fetched.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if matches(pattern, path) {
                let length = pattern.len();
                // On a tie the allow rule wins, as RFC 9309 says.
                if best.is_none_or(|(len, was_allow)| {
                    length > len || (length == len && *allow && !was_allow)
                }) {
                    best = Some((length, *allow));
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = pattern
        .strip_suffix('$')
        .map_or((pattern, false), |p| (p, true));
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut at = 0;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !path.starts_with(part) {
                return false;
            }
            at = part.len();
        } else if let Some(found) = path[at..].find(part) {
            at += found + part.len();
        } else {
            return false;
        }
    }
    if anchored {
        let last = parts.last().copied().unwrap_or_default();
        return path.ends_with(last) && (parts.len() > 1 || at == path.len());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "User-agent: *\nDisallow: /private\nAllow: /private/open\nDisallow: /*.pdf$\nCrawl-delay: 2\n\nUser-agent: badbot\nDisallow: /\n";

    #[test]
    fn longest_rule_wins() {
        let robots = Robots::parse(SAMPLE);
        assert!(robots.allows("/"));
        assert!(!robots.allows("/private/x"));
        assert!(robots.allows("/private/open/x"));
        assert_eq!(robots.crawl_delay, Some(2.0));
    }

    #[test]
    fn wildcards_and_anchors() {
        let robots = Robots::parse(SAMPLE);
        assert!(!robots.allows("/files/report.pdf"));
        assert!(robots.allows("/files/report.pdf?x=1"));
    }

    #[test]
    fn our_own_group_beats_the_star() {
        let robots = Robots::parse("User-agent: dino\nDisallow: /\n\nUser-agent: *\nAllow: /\n");
        assert!(!robots.allows("/anything"));
    }

    #[test]
    fn nothing_said_means_allowed() {
        assert!(Robots::parse("").allows("/x"));
        assert!(Robots::parse("User-agent: *\nDisallow:\n").allows("/x"));
    }
}
