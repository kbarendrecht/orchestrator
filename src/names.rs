//! Worktree names, in the shape Claude Code gives them.
//!
//! The daemon cuts every tree itself now ([`crate::spawn::spawn_worktree_session`]
//! says why), and an unnamed one used to get `wt-<8 hex>`: unique, correct, and
//! impossible to talk about. A rail full of `wt-ca12db78` is a rail you navigate
//! by position rather than by name, and the thing that made the old arm pleasant
//! to use was that `federated-seeking-quasar` is a name you can say out loud.
//!
//! **The three lists are mined, not invented.** They come from 58 names Claude
//! Code had already generated in the monorepo this was developed against, split on
//! the hyphens, with the words that came from hand-named trees (`fcp-initial-bundle`,
//! `flake-article-field`) dropped. So the shape is its shape rather than a guess at
//! it: `<adjective>-<gerund>-<noun>`, where the adjectives run from cute to
//! computer-science (`bubbly`, `immutable`) and about a third of the nouns are
//! surnames from computing (`cerf`, `wadler`, `nygaard`). 51 x 43 x 56 is 122,808
//! names, which is four orders of magnitude past the worktree count anyone reaches.
//!
//! Nothing here is stable across versions: a name is chosen once and then belongs
//! to the directory it named. Adding words is free, and removing one only means it
//! stops being chosen.

/// Words for the first slot. Gerunds appear here too (`shimmying`) because Claude
/// Code puts them here, and a list that "corrected" that would stop matching.
const ADJECTIVES: [&str; 51] = [
    "abstract", "adaptive", "bright", "bubbly", "compiled", "cosmic", "cozy",
    "curious", "declarative", "deep", "dreamy", "eager", "ethereal", "federated",
    "generic", "giggly", "gleaming", "glittery", "hashed", "humming", "immutable",
    "lazy", "lexical", "linear", "linked", "logical", "lucky", "majestic",
    "mutable", "peppy", "polished", "proud", "pure", "resilient", "scalable",
    "sequential", "serialized", "shimmying", "snazzy", "snoopy", "snuggly",
    "sorted", "spicy", "stateless", "tranquil", "transient", "whimsical", "wobbly",
    "zany", "zesty", "zippy",
];

const VERBS: [&str; 43] = [
    "bouncing", "chasing", "conjuring", "cooking", "cuddling", "dazzling",
    "discovering", "doodling", "dreaming", "forging", "gathering", "giggling",
    "greeting", "hugging", "imagining", "inventing", "juggling", "knitting",
    "mapping", "napping", "nibbling", "painting", "plotting", "prancing",
    "puzzling", "roaming", "seeking", "singing", "sleeping", "soaring",
    "sprouting", "squishing", "stirring", "strolling", "swinging", "tickling",
    "tinkering", "toasting", "tumbling", "wandering", "wibbling", "wiggling",
    "wishing",
];

const NOUNS: [&str; 56] = [
    "badger", "bee", "bird", "catmull", "cerf", "cherny", "church", "crane",
    "crayon", "crown", "curry", "dewdrop", "diffie", "dragon", "eagle", "fox",
    "globe", "goblet", "goose", "gray", "grove", "hare", "hennessy", "journal",
    "lake", "locket", "metcalfe", "mochi", "moler", "neumann", "newell",
    "nygaard", "orbit", "perlis", "pony", "quasar", "ripple", "sparrow", "spring",
    "star", "steele", "stream", "sun", "sundae", "swan", "swing", "teacup",
    "tide", "trinket", "tulip", "valley", "wadler", "wilkinson", "wombat",
    "wozniak", "zephyr",
];

/// How many names to offer before the caller gives up.
///
/// Bounded because uniqueness is the *caller's* question and it may answer no to
/// every one of these — a spinning generator would be a hung spawn. Twenty draws
/// from 122,808 names miss only if something is very wrong, and the caller has a
/// fallback for that case rather than an error.
const TRIES: usize = 20;

/// Names, one per item, until you find one nothing has taken.
///
/// An iterator rather than a single name, because what counts as taken is not
/// knowable here: a live workspace holds a name, and so does a directory left on
/// disk by a torn-down one. Only the spawner can ask both.
pub fn candidates() -> impl Iterator<Item = String> {
    (0..TRIES).map(|_| one())
}

/// One name, from one v4 uuid.
///
/// `uuid` is already a dependency and a v4 carries 122 random bits, so this needs
/// no `rand`. Bytes 0 to 5 only: a v4 fixes bits in bytes 6 and 8 for its version
/// and variant, and a pick that read those would be less random than it looks.
fn one() -> String {
    let b = uuid::Uuid::new_v4().into_bytes();
    let pick = |lo: usize, list: &[&str]| -> String {
        let n = u16::from_le_bytes([b[lo], b[lo + 1]]) as usize;
        list[n % list.len()].to_string()
    };
    format!(
        "{}-{}-{}",
        pick(0, &ADJECTIVES),
        pick(2, &VERBS),
        pick(4, &NOUNS)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every word has to survive `validate_worktree_name`, which is the only thing
    /// standing between a generated name and a path on disk. A word with a capital,
    /// a space or a dot would be refused by the spawn that generated it — a failure
    /// nobody would think to look for here.
    #[test]
    fn every_word_is_a_legal_name_part() {
        for list in [&ADJECTIVES[..], &VERBS[..], &NOUNS[..]] {
            for w in list {
                assert!(!w.is_empty(), "an empty word");
                assert!(
                    w.chars().all(|c| c.is_ascii_lowercase()),
                    "{w} is not plain lowercase ascii"
                );
            }
        }
    }

    /// No duplicates, because a repeated word is a name that comes up twice as
    /// often for no reason, and the lists were assembled by hand from a mined set.
    #[test]
    fn no_word_is_listed_twice() {
        for (what, list) in [
            ("adjectives", &ADJECTIVES[..]),
            ("verbs", &VERBS[..]),
            ("nouns", &NOUNS[..]),
        ] {
            let mut seen = std::collections::HashSet::new();
            for w in list {
                assert!(seen.insert(w), "{what} lists {w} twice");
            }
        }
    }

    #[test]
    fn a_name_is_three_words_and_passes_the_spawn_guard() {
        for name in candidates() {
            assert_eq!(name.split('-').count(), 3, "{name}");
            crate::spawn::validate_worktree_name(&name).expect(&name);
        }
    }

    /// The whole point of the iterator: the caller filters, and a name it refuses
    /// costs one item rather than the spawn.
    #[test]
    fn the_caller_can_skip_what_it_has_already_taken() {
        let first = candidates().next().expect("at least one name");
        let free = candidates().find(|c| *c != first);
        assert!(free.is_some(), "twenty draws that all collide");
    }

    /// Two draws in a row differ. Not a randomness test — it is here because the
    /// pick used to read a fixed uuid byte in an earlier draft, which is the shape
    /// of mistake that makes every worktree in a session share a name.
    #[test]
    fn draws_are_not_all_the_same() {
        let names: std::collections::HashSet<String> = candidates().collect();
        assert!(names.len() > 1, "twenty draws produced {:?}", names);
    }
}

#[cfg(test)]
mod sample {
    /// Not an assertion — a way to look at what the lists actually produce, since
    /// the only real test of a name is whether a person would say it.
    #[test]
    #[ignore = "prints names; run with --ignored"]
    fn print_a_dozen() {
        for n in super::candidates().take(12) {
            println!("{n}");
        }
    }
}
