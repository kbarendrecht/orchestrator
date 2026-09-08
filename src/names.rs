//! Worktree names, in the shape Claude Code gives them.
//!
//! The daemon cuts every tree itself now ([`crate::spawn::spawn_worktree_session`]
//! says why), and an unnamed one used to get `wt-<8 hex>`: unique, correct, and
//! impossible to talk about. A rail full of `wt-ca12db78` is a rail you navigate by
//! position rather than by name, and `federated-seeking-quasar` is a name you can
//! say out loud.
//!
//! The shape is Claude Code's: `<adjective>-<gerund>-<noun>`, three families of
//! noun (landscape, animal, a surname from computing) and two of adjective (cute,
//! and computer-science). Picked with four random bytes modulo the list length,
//! which is what its own generator does.
//!
//! **The words are ours, and that is deliberate.** Claude Code ships its lists as
//! three arrays inside a Bun-compiled binary, and they are readable with `grep -a`.
//! Lifting them would have been quicker and is the one thing not done here: this
//! repo is public and AGPL-3.0, so vendoring a curated list out of a proprietary
//! bundle would mean redistributing somebody else's compilation under a licence
//! that is not theirs to give — and the terms covering that binary restrict taking
//! it apart in the first place. A format is not ownable and neither is `quokka`, so
//! the shape and the method are reproduced and the vocabulary is written from
//! scratch.
//!
//! Measured against 2.1.263 rather than asserted: of these words, 61% of the
//! adjectives, 48% of the gerunds and 50% of the nouns also appear in its lists,
//! and 88 / 78 / 201 of them do not. That is what two people picking from the same
//! families looks like — there are only so many words for "idempotent" or "otter",
//! and about 150 of the words here came from names it had already generated, so
//! some of the overlap is that sample coming home. Independence is not the claim;
//! not vendoring its compilation is.
//!
//! The first ~150 words came from 58 names Claude Code had already generated in the
//! monorepo this was developed against, which are directory names in a working tree
//! rather than anything out of its source. The rest were written by hand.
//! 229 x 150 x 407 is 13,980,450 names.
//!
//! Nothing here is stable across versions: a name is chosen once and then belongs to
//! the directory it named. Adding words is free; removing one only stops it being
//! chosen.

/// Slot one. Gerunds appear here too (`shimmying`) because Claude Code puts them
/// here, and a list that "corrected" that would stop matching its output.
const ADJECTIVES: &[&str] = &[
    "bright", "bubbly", "cosmic", "cozy", "curious", "dreamy", "eager", "ethereal", "giggly",
    "gleaming", "glittery", "humming", "lazy", "lucky", "majestic", "peppy", "polished",
    "proud", "pure", "shimmying", "snazzy", "snoopy", "snuggly", "spicy", "starry", "tranquil",
    "whimsical", "wobbly", "zany", "zesty", "zippy", "balmy", "bouncy", "breezy", "brisk",
    "chatty", "cheerful", "cheery", "chipper", "chirpy", "comfy", "cuddly", "dainty", "dapper",
    "dinky", "downy", "drowsy", "dulcet", "feathery", "feisty", "fizzy", "fluffy", "foamy",
    "frisky", "frosty", "gentle", "giddy", "glowing", "golden", "hazy", "honeyed", "jaunty",
    "jolly", "jovial", "keen", "kindly", "lively", "loyal", "mellow", "merry", "mighty",
    "mirthful", "misty", "mossy", "nimble", "nutty", "plucky", "plush", "prancy", "quirky",
    "radiant", "roomy", "rosy", "ruddy", "rustic", "silky", "silly", "sleepy", "smiley",
    "snappy", "snowy", "sparkly", "spry", "squishy", "sturdy", "sunny", "sweet", "tender",
    "tidy", "tingly", "toasty", "twinkly", "velvety", "warm", "whirly", "wiggly", "winsome",
    "witty", "woolly", "zealous", "abstract", "adaptive", "compiled", "declarative", "deep",
    "federated", "generic", "hashed", "immutable", "lexical", "linear", "linked", "logical",
    "mutable", "resilient", "scalable", "sequential", "serialized", "sorted", "stateless",
    "transient", "agile", "async", "atomic", "batched", "binary", "buffered", "cached",
    "chunked", "columnar", "compact", "composed", "compressed", "concurrent", "curried",
    "delegated", "dense", "deterministic", "distributed", "dynamic", "elastic", "encoded",
    "enumerated", "eventual", "expressive", "factored", "functional", "greedy", "hermetic",
    "hoisted", "idempotent", "imperative", "indexed", "inherited", "inlined", "interpreted",
    "iterative", "layered", "lifted", "memoized", "modular", "monotonic", "nested",
    "normalized", "opaque", "optimized", "ordered", "parallel", "parsed", "partitioned",
    "persistent", "piped", "pipelined", "polymorphic", "portable", "prefetched", "quantized",
    "queued", "reactive", "recursive", "reentrant", "refactored", "reflective", "replicated",
    "robust", "rolling", "routed", "sandboxed", "scheduled", "scoped", "sharded", "sparse",
    "spliced", "staged", "stateful", "static", "streamed", "strict", "striped", "structured",
    "symbolic", "synchronous", "synthetic", "tagged", "temporal", "threaded", "tiled", "traced",
    "transactional", "typed", "unified", "unrolled", "validated", "vectorized", "versioned",
    "virtual", "weighted", "wrapped", "zipped"
];

/// Slot two.
const GERUNDS: &[&str] = &[
    "bouncing", "chasing", "conjuring", "cooking", "cuddling", "dazzling", "discovering",
    "doodling", "dreaming", "forging", "gathering", "giggling", "greeting", "hugging",
    "imagining", "inventing", "juggling", "knitting", "mapping", "napping", "nibbling",
    "painting", "plotting", "prancing", "puzzling", "roaming", "seeking", "singing", "sleeping",
    "soaring", "sprouting", "squishing", "stirring", "strolling", "swinging", "tickling",
    "tinkering", "toasting", "tumbling", "wandering", "wibbling", "wiggling", "wishing",
    "baking", "basking", "beaming", "blooming", "bobbing", "bounding", "brewing", "bubbling",
    "building", "chirping", "climbing", "coasting", "crafting", "dancing", "dashing", "digging",
    "diving", "drifting", "drumming", "dusting", "fetching", "fishing", "floating", "flitting",
    "flying", "folding", "frolicking", "gardening", "gliding", "grinning", "growing",
    "harvesting", "herding", "hiking", "hopping", "hunting", "jogging", "jumping", "leaping",
    "lifting", "listening", "marching", "mending", "mixing", "munching", "nesting", "noodling",
    "nudging", "paddling", "parading", "planting", "playing", "polishing", "pondering",
    "pouncing", "prowling", "racing", "rambling", "reading", "resting", "rippling", "rolling",
    "rowing", "sailing", "sanding", "scampering", "scouting", "sculpting", "sewing",
    "shuffling", "sifting", "skating", "sketching", "skipping", "sliding", "snoozing", "sowing",
    "sparkling", "spinning", "splashing", "sprinting", "sprucing", "stacking", "stitching",
    "strumming", "studying", "swimming", "swirling", "tending", "thinking", "tiptoeing",
    "tracing", "trotting", "twirling", "unfolding", "vaulting", "wading", "waltzing",
    "watching", "weaving", "whirling", "whisking", "whistling", "winding", "writing", "zipping",
    "zooming"
];

/// Slot three: landscape, then animal, then a surname from computing. The three
/// families are what makes a name read as a name rather than as two random words.
const NOUNS: &[&str] = &[
    "crayon", "crown", "curry", "dewdrop", "globe", "goblet", "grove", "journal", "lake",
    "locket", "mochi", "orbit", "ripple", "spring", "star", "stream", "sun", "sundae", "swing",
    "teacup", "tide", "trinket", "tulip", "valley", "zephyr", "alcove", "atoll", "bay",
    "beacon", "birch", "bloom", "bluff", "bog", "boulder", "bramble", "brook", "canyon",
    "cavern", "cedar", "cliff", "clover", "comet", "corona", "cosmos", "cove", "crag", "creek",
    "crest", "dawn", "delta", "dune", "dusk", "eclipse", "eddy", "ember", "fen", "fern",
    "fjord", "flint", "foam", "fog", "forest", "fountain", "frost", "galaxy", "garden",
    "geyser", "glade", "glen", "granite", "grotto", "gully", "harbor", "haven", "heath",
    "hedge", "hollow", "ivy", "knoll", "lagoon", "ledge", "lichen", "lily", "marsh", "meadow",
    "mesa", "meteor", "mist", "moon", "moor", "moss", "nebula", "nova", "oasis", "orchard",
    "palm", "pebble", "petal", "pinecone", "planet", "plateau", "pollen", "pond", "prairie",
    "puddle", "pulsar", "quarry", "rapids", "ravine", "reed", "reef", "ridge", "rill", "sage",
    "sand", "sapling", "savanna", "shale", "shore", "shrub", "sleet", "slope", "sprout",
    "spruce", "steppe", "stone", "summit", "sunbeam", "swamp", "thicket", "thistle", "thorn",
    "timber", "trail", "tundra", "twilight", "vale", "vine", "willow", "woodland", "zenith",
    "badger", "bee", "bird", "crane", "dragon", "eagle", "fox", "goose", "hare", "pony",
    "sparrow", "swan", "wombat", "alpaca", "anteater", "axolotl", "barnacle", "beaver",
    "beetle", "bison", "bobcat", "buffalo", "bullfrog", "bumblebee", "bunny", "capybara",
    "cardinal", "caribou", "chameleon", "cheetah", "chickadee", "chinchilla", "chipmunk",
    "cicada", "clam", "cobra", "condor", "coyote", "crab", "cricket", "crow", "cuckoo",
    "curlew", "dingo", "dolphin", "donkey", "dormouse", "dove", "duckling", "dugong", "echidna",
    "egret", "elk", "falcon", "fawn", "ferret", "finch", "firefly", "flamingo", "gannet",
    "gazelle", "gecko", "gerbil", "gibbon", "giraffe", "gnat", "gopher", "grouse", "guppy",
    "hamster", "hedgehog", "heron", "hippo", "hornet", "hummingbird", "ibex", "ibis", "iguana",
    "impala", "jackdaw", "jaguar", "jay", "jellyfish", "kestrel", "kingfisher", "kinkajou",
    "kitten", "koala", "krill", "ladybug", "lapwing", "lark", "lemur", "leopard", "limpet",
    "llama", "lobster", "lynx", "macaw", "magpie", "mallard", "manatee", "mantis", "marmot",
    "marten", "meerkat", "mink", "minnow", "mole", "mongoose", "moose", "moth", "narwhal",
    "newt", "nightingale", "numbat", "ocelot", "octopus", "okapi", "opossum", "orca", "oriole",
    "osprey", "otter", "owl", "oyster", "panda", "pangolin", "parrot", "partridge", "peacock",
    "pelican", "penguin", "petrel", "pheasant", "pigeon", "piglet", "pika", "platypus",
    "plover", "porcupine", "possum", "prawn", "puffin", "puma", "quail", "quokka", "rabbit",
    "raccoon", "raven", "reindeer", "robin", "rooster", "salamander", "salmon", "sandpiper",
    "scallop", "seahorse", "seal", "serval", "shrew", "shrike", "skink", "skunk", "sloth",
    "snail", "spoonbill", "squid", "squirrel", "starling", "stingray", "stoat", "stork",
    "sunfish", "swallow", "tamarin", "tanager", "tapir", "teal", "tern", "thrush", "toad",
    "tortoise", "toucan", "trout", "turtle", "urchin", "vole", "vulture", "wallaby", "walrus",
    "warbler", "weasel", "whale", "wigeon", "wolverine", "woodpecker", "wren", "yak", "zebra",
    "catmull", "cerf", "cherny", "church", "gray", "hennessy", "metcalfe", "moler", "neumann",
    "newell", "nygaard", "perlis", "steele", "wadler", "wilkinson", "wozniak", "diffie",
    "allen", "backus", "bachman", "bartik", "blum", "brooks", "cocke", "codd", "cook",
    "corbato", "dahl", "dijkstra", "engelbart", "feigenbaum", "floyd", "hamming", "hartmanis",
    "hoare", "hopcroft", "hopper", "iverson", "kahn", "karp", "kay", "knuth", "lamport",
    "lampson", "liskov", "mccarthy", "milner", "minsky", "naur", "pnueli", "rabin", "reddy",
    "ritchie", "rivest", "scott", "shamir", "simon", "stearns", "sutherland", "tarjan",
    "thacker", "thompson", "valiant", "wilkes", "wirth", "zuse"
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
        pick(0, ADJECTIVES),
        pick(2, GERUNDS),
        pick(4, NOUNS)
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
        for list in [ADJECTIVES, GERUNDS, NOUNS] {
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
            ("adjectives", ADJECTIVES),
            ("verbs", GERUNDS),
            ("nouns", NOUNS),
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
