//! A second, deliberately different implementation of the rules: whole-text
//! snapshots instead of recorded edits. The tests drive both and compare.
#![allow(dead_code)]

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Last {
    Typed(usize),
    DeletedOne(usize),
    Backspaced(usize),
    DeletedForward(usize),
}

#[derive(Default)]
pub struct Model {
    pub text: Vec<char>,
    before: Vec<Vec<char>>,
    after: Vec<Vec<char>>,
    last: Option<Last>,
    open: usize,
    group_has_step: bool,
}

impl Model {
    pub fn new(text: &str) -> Self {
        Model { text: text.chars().collect(), ..Self::default() }
    }

    pub fn string(&self) -> String {
        self.text.iter().collect()
    }

    pub fn can_undo(&self) -> bool {
        !self.before.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.after.is_empty()
    }

    /// Saves the text before an edit, unless the edit joins the current step.
    fn step(&mut self, joins: bool, snapshot: Vec<char>) {
        self.after.clear();
        if self.open > 0 {
            if !self.group_has_step {
                self.before.push(snapshot);
                self.group_has_step = true;
            }
        } else if !joins {
            self.before.push(snapshot);
        }
    }

    pub fn insert(&mut self, at: usize, s: &str) -> bool {
        if at > self.text.len() {
            return false;
        }
        let new: Vec<char> = s.chars().collect();
        if new.is_empty() {
            return true;
        }
        let snapshot = self.text.clone();
        let one = new.len() == 1 && new[0] != '\n';
        let joins = one && self.open == 0 && self.last == Some(Last::Typed(at));
        for (i, c) in new.into_iter().enumerate() {
            self.text.insert(at + i, c);
        }
        self.step(joins, snapshot);
        self.last = (one && self.open == 0).then_some(Last::Typed(at + 1));
        true
    }

    pub fn delete(&mut self, start: usize, end: usize) -> Option<String> {
        if start > end || end > self.text.len() {
            return None;
        }
        if start == end {
            return Some(String::new());
        }
        let snapshot = self.text.clone();
        let gone: String = self.text.drain(start..end).collect();
        let one = end - start == 1;
        let (joins, next) = if !one || self.open > 0 {
            (false, None)
        } else {
            match self.last {
                Some(Last::DeletedOne(s) | Last::Backspaced(s)) if end == s => (true, Some(Last::Backspaced(start))),
                Some(Last::DeletedOne(s) | Last::DeletedForward(s)) if start == s => (true, Some(Last::DeletedForward(start))),
                _ => (false, Some(Last::DeletedOne(start))),
            }
        };
        self.step(joins, snapshot);
        self.last = next;
        Some(gone)
    }

    pub fn begin(&mut self) {
        self.last = None;
        if self.open == 0 {
            self.group_has_step = false;
        }
        self.open += 1;
    }

    pub fn end(&mut self) {
        self.last = None;
        if self.open > 0 {
            self.open -= 1;
        }
    }

    pub fn undo(&mut self) -> bool {
        self.open = 0;
        self.last = None;
        match self.before.pop() {
            Some(text) => {
                self.after.push(std::mem::replace(&mut self.text, text));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        self.open = 0;
        self.last = None;
        match self.after.pop() {
            Some(text) => {
                self.before.push(std::mem::replace(&mut self.text, text));
                true
            }
            None => false,
        }
    }

    pub fn line_col(&self, at: usize) -> Option<(usize, usize)> {
        (at <= self.text.len()).then(|| {
            let mut line = 0;
            let mut col = 0;
            for &c in &self.text[..at] {
                if c == '\n' {
                    line += 1;
                    col = 0;
                } else {
                    col += 1;
                }
            }
            (line, col)
        })
    }
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

const ALPHABET: [char; 9] = ['a', 'b', ' ', 'é', '🦀', '\n', '中', 'z', '\u{301}'];

/// Weights per operation, in the order of the `match` in `drive`.
pub struct Mix(pub [usize; 12]);

pub const TYPING: Mix = Mix([30, 12, 6, 3, 3, 2, 2, 8, 4, 2, 2, 6]);
pub const EDITING: Mix = Mix([8, 4, 4, 12, 12, 3, 3, 10, 6, 3, 3, 4]);
pub const GROUPS: Mix = Mix([12, 6, 3, 6, 6, 3, 2, 8, 5, 8, 8, 4]);

/// Runs `ops` random operations against the buffer and the model, comparing
/// after every one.
pub fn drive(seed: u64, ops: usize, start: &str, mix: &Mix) {
    let mut rng = Rng::new(seed);
    let mut buffer = fixture::Buffer::from_text(start);
    let mut model = Model::new(start);
    let mut cursor = model.text.len();
    let total: usize = mix.0.iter().sum();
    for i in 0..ops {
        let mut pick = rng.below(total);
        let mut which = 0;
        while pick >= mix.0[which] {
            pick -= mix.0[which];
            which += 1;
        }
        let len = model.text.len();
        let what = match which {
            0 => {
                let c = ALPHABET[rng.below(ALPHABET.len())].to_string();
                let ok = model.insert(cursor, &c);
                assert_eq!(buffer.insert(cursor, &c).is_ok(), ok, "op {i}: type {c:?} at {cursor}");
                cursor += 1;
                format!("type {c:?}")
            }
            1 => {
                if cursor > 0 {
                    let want = model.delete(cursor - 1, cursor);
                    assert_eq!(buffer.delete(cursor - 1..cursor).ok(), want, "op {i}: backspace at {cursor}");
                    cursor -= 1;
                }
                "backspace".into()
            }
            2 => {
                if cursor < len {
                    let want = model.delete(cursor, cursor + 1);
                    assert_eq!(buffer.delete(cursor..cursor + 1).ok(), want, "op {i}: delete at {cursor}");
                }
                "forward delete".into()
            }
            3 => {
                let n = rng.below(4);
                let s: String = (0..n).map(|_| ALPHABET[rng.below(ALPHABET.len())]).collect();
                let at = rng.below(len + 2);
                let ok = model.insert(at, &s);
                assert_eq!(buffer.insert(at, &s).is_ok(), ok, "op {i}: insert {s:?} at {at}");
                format!("insert {s:?} at {at}")
            }
            4 => {
                let a = rng.below(len + 2);
                let b = rng.below(len + 2);
                let (a, b) = if rng.below(8) == 0 { (b.max(a), a.min(b)) } else { (a.min(b), a.max(b)) };
                let want = model.delete(a, b);
                assert_eq!(buffer.delete(a..b).ok(), want, "op {i}: delete {a}..{b}");
                format!("delete {a}..{b}")
            }
            5 => {
                assert!(buffer.insert(cursor, "").is_ok());
                "empty insert".into()
            }
            6 => {
                assert_eq!(buffer.delete(cursor..cursor).ok(), Some(String::new()));
                "empty delete".into()
            }
            7 => {
                assert_eq!(buffer.undo(), model.undo(), "op {i}: undo");
                "undo".into()
            }
            8 => {
                assert_eq!(buffer.redo(), model.redo(), "op {i}: redo");
                "redo".into()
            }
            9 => {
                buffer.begin_group();
                model.begin();
                "begin group".into()
            }
            10 => {
                buffer.end_group();
                model.end();
                "end group".into()
            }
            _ => {
                cursor = rng.below(len + 1);
                "move".into()
            }
        };
        cursor = cursor.min(model.text.len());
        assert_eq!(buffer.text(), model.string(), "op {i} ({what})");
        assert_eq!(buffer.len_chars(), model.text.len(), "op {i} ({what})");
        assert_eq!((buffer.can_undo(), buffer.can_redo()), (model.can_undo(), model.can_redo()), "op {i} ({what})");
        let at = rng.below(model.text.len() + 2);
        assert_eq!(buffer.line_col(at).ok(), model.line_col(at), "op {i}: line_col({at})");
        if let Some((line, col)) = model.line_col(at) {
            assert_eq!(buffer.offset(line, col), Ok(at), "op {i}: offset({line}, {col})");
        }
    }
}
