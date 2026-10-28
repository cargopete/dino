use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Identifier {
    Numeric(u64),
    Alpha(String),
}

#[derive(Debug, Clone)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<Identifier>,
    pub build: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError;

#[derive(Debug, Clone)]
pub struct Range {
    sets: Vec<Vec<Comparator>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

#[derive(Debug, Clone)]
enum Comparator {
    Any,
    Nothing,
    Cmp(Op, Version),
}

fn numeric(s: &str) -> Result<u64, ParseError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
        return Err(ParseError);
    }
    s.parse().map_err(|_| ParseError)
}

fn ident_chars(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn parse_pre(s: &str) -> Result<Vec<Identifier>, ParseError> {
    s.split('.')
        .map(|part| {
            if !ident_chars(part) {
                Err(ParseError)
            } else if part.bytes().all(|b| b.is_ascii_digit()) {
                numeric(part).map(Identifier::Numeric)
            } else {
                Ok(Identifier::Alpha(part.to_owned()))
            }
        })
        .collect()
}

fn parse_build(s: &str) -> Result<Vec<String>, ParseError> {
    s.split('.')
        .map(|part| if ident_chars(part) { Ok(part.to_owned()) } else { Err(ParseError) })
        .collect()
}

/// Splits `core[-pre][+build]`.
fn split_tail(s: &str) -> (&str, Option<&str>, Option<&str>) {
    let (rest, build) = match s.find('+') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (core, pre) = match rest.find('-') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    (core, pre, build)
}

impl Version {
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let (core, pre, build) = split_tail(s);
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(ParseError);
        }
        Ok(Version {
            major: numeric(parts[0])?,
            minor: numeric(parts[1])?,
            patch: numeric(parts[2])?,
            pre: pre.map(parse_pre).transpose()?.unwrap_or_default(),
            build: build.map(parse_build).transpose()?.unwrap_or_default(),
        })
    }

    fn new(major: u64, minor: u64, patch: u64, pre: Vec<Identifier>) -> Self {
        Version { major, minor, patch, pre, build: Vec::new() }
    }

    fn zero_pre(major: u64, minor: u64, patch: u64) -> Self {
        Self::new(major, minor, patch, vec![Identifier::Numeric(0)])
    }

    fn triple(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Identifier::Numeric(a), Identifier::Numeric(b)) => a.cmp(b),
            (Identifier::Numeric(_), Identifier::Alpha(_)) => Ordering::Less,
            (Identifier::Alpha(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Alpha(a), Identifier::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.triple().cmp(&other.triple()).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            }
        })
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Identifier::Numeric(n) => write!(f, "{n}"),
            Identifier::Alpha(s) => f.write_str(s),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            let pre: Vec<String> = self.pre.iter().map(ToString::to_string).collect();
            write!(f, "-{}", pre.join("."))?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build.join("."))?;
        }
        Ok(())
    }
}

/// A version with any of its three parts missing or a wildcard.
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<Identifier>,
}

fn is_wild(s: &str) -> bool {
    matches!(s, "x" | "X" | "*")
}

fn parse_partial(s: &str) -> Result<Partial, ParseError> {
    let s = s.strip_prefix('=').unwrap_or(s);
    let s = s.strip_prefix('v').unwrap_or(s);
    let (core, pre, build) = split_tail(s);
    if let Some(build) = build {
        parse_build(build)?;
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(ParseError);
    }
    let mut nums = [None, None, None];
    let mut wild = false;
    for (i, part) in parts.iter().enumerate() {
        if is_wild(part) {
            wild = true;
        } else if wild {
            return Err(ParseError);
        } else {
            nums[i] = Some(numeric(part)?);
        }
    }
    let pre = match pre {
        Some(pre) if nums[2].is_some() => parse_pre(pre)?,
        Some(_) => return Err(ParseError),
        None => Vec::new(),
    };
    Ok(Partial { major: nums[0], minor: nums[1], patch: nums[2], pre })
}

fn ge(v: Version) -> Comparator {
    Comparator::Cmp(Op::Ge, v)
}

fn lt(v: Version) -> Comparator {
    Comparator::Cmp(Op::Lt, v)
}

fn caret(p: &Partial) -> Vec<Comparator> {
    let Some(ma) = p.major else { return vec![Comparator::Any] };
    let Some(mi) = p.minor else {
        return vec![ge(Version::new(ma, 0, 0, vec![])), lt(Version::zero_pre(ma + 1, 0, 0))];
    };
    let Some(pa) = p.patch else {
        let upper = if ma == 0 { Version::zero_pre(ma, mi + 1, 0) } else { Version::zero_pre(ma + 1, 0, 0) };
        return vec![ge(Version::new(ma, mi, 0, vec![])), lt(upper)];
    };
    let upper = if ma > 0 {
        Version::zero_pre(ma + 1, 0, 0)
    } else if mi > 0 {
        Version::zero_pre(0, mi + 1, 0)
    } else {
        Version::zero_pre(0, 0, pa + 1)
    };
    vec![ge(Version::new(ma, mi, pa, p.pre.clone())), lt(upper)]
}

fn tilde(p: &Partial) -> Vec<Comparator> {
    let Some(ma) = p.major else { return vec![Comparator::Any] };
    let Some(mi) = p.minor else {
        return vec![ge(Version::new(ma, 0, 0, vec![])), lt(Version::zero_pre(ma + 1, 0, 0))];
    };
    let pa = p.patch.unwrap_or(0);
    let pre = if p.patch.is_some() { p.pre.clone() } else { vec![] };
    vec![ge(Version::new(ma, mi, pa, pre)), lt(Version::zero_pre(ma, mi + 1, 0))]
}

fn primitive(op: Option<Op>, p: &Partial) -> Vec<Comparator> {
    let Some(ma) = p.major else {
        return match op {
            Some(Op::Gt | Op::Lt) => vec![Comparator::Nothing],
            _ => vec![Comparator::Any],
        };
    };
    if let (Some(mi), Some(pa)) = (p.minor, p.patch) {
        let v = Version::new(ma, mi, pa, p.pre.clone());
        return vec![Comparator::Cmp(op.unwrap_or(Op::Eq), v)];
    }
    let mi = p.minor.unwrap_or(0);
    // The next version past the partial: 1 -> 2.0.0, 1.2 -> 1.3.0.
    let next = if p.minor.is_none() { (ma + 1, 0) } else { (ma, mi + 1) };
    match op {
        Some(Op::Gt) => vec![ge(Version::new(next.0, next.1, 0, vec![]))],
        Some(Op::Le) => vec![lt(Version::zero_pre(next.0, next.1, 0))],
        Some(Op::Lt) => vec![lt(Version::zero_pre(ma, mi, 0))],
        Some(Op::Ge) => vec![ge(Version::new(ma, mi, 0, vec![]))],
        None | Some(Op::Eq) => vec![
            ge(Version::new(ma, mi, 0, vec![])),
            lt(Version::zero_pre(next.0, next.1, 0)),
        ],
    }
}

fn hyphen(from: &Partial, to: &Partial) -> Vec<Comparator> {
    let mut out = Vec::new();
    if let Some(ma) = from.major {
        let v = match (from.minor, from.patch) {
            (Some(mi), Some(pa)) => Version::new(ma, mi, pa, from.pre.clone()),
            (Some(mi), None) => Version::new(ma, mi, 0, vec![]),
            _ => Version::new(ma, 0, 0, vec![]),
        };
        out.push(ge(v));
    }
    if let Some(ma) = to.major {
        out.push(match (to.minor, to.patch) {
            (Some(mi), Some(pa)) => Comparator::Cmp(Op::Le, Version::new(ma, mi, pa, to.pre.clone())),
            (Some(mi), None) => lt(Version::zero_pre(ma, mi + 1, 0)),
            _ => lt(Version::zero_pre(ma + 1, 0, 0)),
        });
    }
    if out.is_empty() {
        out.push(Comparator::Any);
    }
    out
}

fn split_op(token: &str) -> (Option<Op>, &str) {
    for (text, op) in [(">=", Op::Ge), ("<=", Op::Le), (">", Op::Gt), ("<", Op::Lt), ("=", Op::Eq)] {
        if let Some(rest) = token.strip_prefix(text) {
            return (Some(op), rest);
        }
    }
    (None, token)
}

/// Joins an operator to the version after it: `>= 1.2` becomes `>=1.2`.
fn tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    for word in s.split_whitespace() {
        let is_op = matches!(word, ">=" | "<=" | ">" | "<" | "=" | "~" | "~>" | "^");
        match pending.take() {
            Some(op) => out.push(format!("{op}{word}")),
            None if is_op => pending = Some(word.to_owned()),
            None => out.push(word.to_owned()),
        }
    }
    if let Some(op) = pending {
        out.push(op);
    }
    out
}

fn parse_set(s: &str) -> Result<Vec<Comparator>, ParseError> {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() == 3 && words[1] == "-" {
        return Ok(hyphen(&parse_partial(words[0])?, &parse_partial(words[2])?));
    }
    let toks = tokens(s);
    if toks.is_empty() {
        return Ok(vec![Comparator::Any]);
    }
    let mut out = Vec::new();
    for tok in toks {
        if let Some(rest) = tok.strip_prefix('^') {
            out.extend(caret(&parse_partial(rest)?));
        } else if let Some(rest) = tok.strip_prefix("~>").or_else(|| tok.strip_prefix('~')) {
            out.extend(tilde(&parse_partial(rest)?));
        } else {
            let (op, rest) = split_op(&tok);
            if rest.is_empty() {
                return Err(ParseError);
            }
            out.extend(primitive(op, &parse_partial(rest)?));
        }
    }
    Ok(out)
}

fn holds(c: &Comparator, v: &Version) -> bool {
    match c {
        Comparator::Any => true,
        Comparator::Nothing => false,
        Comparator::Cmp(op, bound) => {
            let ord = v.cmp(bound);
            match op {
                Op::Lt => ord == Ordering::Less,
                Op::Le => ord != Ordering::Greater,
                Op::Gt => ord == Ordering::Greater,
                Op::Ge => ord != Ordering::Less,
                Op::Eq => ord == Ordering::Equal,
            }
        }
    }
}

fn set_matches(set: &[Comparator], v: &Version) -> bool {
    if !set.iter().all(|c| holds(c, v)) {
        return false;
    }
    if v.pre.is_empty() {
        return true;
    }
    set.iter().any(|c| match c {
        Comparator::Cmp(_, bound) => !bound.pre.is_empty() && bound.triple() == v.triple(),
        _ => false,
    })
}

impl Range {
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let sets = s.split("||").map(parse_set).collect::<Result<_, _>>()?;
        Ok(Range { sets })
    }

    pub fn matches(&self, version: &Version) -> bool {
        self.sets.iter().any(|set| set_matches(set, version))
    }

    pub fn max_satisfying<'a>(&self, versions: &'a [Version]) -> Option<&'a Version> {
        let mut best: Option<&Version> = None;
        for v in versions.iter().filter(|v| self.matches(v)) {
            if best.is_none_or(|b| v > b) {
                best = Some(v);
            }
        }
        best
    }

    pub fn min_satisfying<'a>(&self, versions: &'a [Version]) -> Option<&'a Version> {
        let mut best: Option<&Version> = None;
        for v in versions.iter().filter(|v| self.matches(v)) {
            if best.is_none_or(|b| v < b) {
                best = Some(v);
            }
        }
        best
    }
}
