use crate::token::{Token, TokenKind};

/// How many tokens of each kind, in the order the kinds first appear.
pub fn count_kinds(tokens: &[Token<'_>]) -> Vec<(TokenKind, usize)> {
    let mut counts: Vec<(TokenKind, usize)> = Vec::new();
    for tok in tokens {
        match counts.iter_mut().find(|(kind, _)| *kind == tok.kind) {
            Some((_, n)) => *n += 1,
            None => counts.push((tok.kind, 1)),
        }
    }
    counts
}

/// The identifier with the most chars; the first one wins a tie.
pub fn longest_ident<'a>(tokens: &[Token<'a>]) -> Option<&'a str> {
    let mut best: Option<&Token<'a>> = None;
    for tok in tokens.iter().filter(|t| t.is(TokenKind::Ident)) {
        if best.is_none_or(|b| tok.text.chars().count() > b.text.chars().count()) {
            best = Some(tok);
        }
    }
    best.map(|t| t.as_str())
}
