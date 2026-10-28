mod common;

use common::*;

macro_rules! random {
    ($name:ident, $seed:expr, $ops:expr, $start:expr, $mix:expr) => {
        #[test]
        fn $name() {
            drive($seed, $ops, $start, &$mix);
        }
    };
}

random!(typing_from_empty, 1, 600, "", TYPING);
random!(typing_into_text, 2, 600, "hello\nwörld\n", TYPING);
random!(typing_long_run, 3, 2_000, "", TYPING);
random!(editing_from_empty, 4, 600, "", EDITING);
random!(editing_unicode, 5, 600, "🦀 é\n中文\n\n", EDITING);
random!(editing_long_run, 6, 2_000, "abc\ndef", EDITING);
random!(groups_from_empty, 7, 600, "", GROUPS);
random!(groups_into_text, 8, 600, "one\ntwo\nthree", GROUPS);
random!(groups_long_run, 9, 2_000, "x", GROUPS);
random!(mixed_seed_10, 10, 1_000, "", EDITING);
random!(mixed_seed_11, 11, 1_000, "", TYPING);
random!(mixed_seed_12, 12, 1_000, "", GROUPS);
