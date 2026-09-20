//! Where a symbol is defined, guessed from the shapes a definition takes.
//!
//! **A heuristic, and it is honest about being one.** There is no type
//! information here and no language server: this builds a regular expression out
//! of the forms a definition is written in — `fn NAME`, `class NAME`,
//! `func (r *T) NAME` — and runs it through the same searcher the overlay uses.
//! What makes it usable rather than annoying is the rule the caller applies:
//! **jump only on exactly one hit**, and show the list otherwise. A guess that
//! moves you somewhere wrong is worse than no jump at all.
//!
//! **Why not a language server.** It would mean the daemon spawning and
//! supervising one process per language per workspace, a JSON-RPC client, minutes
//! of indexing and rust-analyzer's memory on every worktree — against the rule in
//! `CLAUDE.md` that a default may only depend on what the daemon already needs.
//! Worth revisiting if this proves annoying; not worth leading with.
//!
//! **The table is here rather than in the page** for the reason the rest of the
//! gates exist: a table in Rust has a test per language beside it, and a table in
//! JavaScript would have had none. It is also why the language is derived from the
//! *clicked file's* extension — a symbol has no language of its own.

use anyhow::Result;
use std::path::Path;

use crate::search::{search, Matches, Query};

/// The definition forms of one language, as regex templates. `{}` is where the
/// escaped symbol goes, and every template is anchored to the start of a line
/// because a definition begins one.
struct Shapes(&'static [&'static str]);

/// Rust. `macro_rules!` is spelled separately because the bang is part of the
/// keyword, and `impl` blocks are deliberately absent: `impl Foo` is where the
/// methods live, not where `Foo` is declared, and including it made every struct
/// answer with a list.
const RUST: Shapes = Shapes(&[
    r"^\s*(pub(\([^)]*\))?\s+)?(default\s+)?(async\s+)?(unsafe\s+)?(extern\s+\x22[^\x22]*\x22\s+)?fn\s+{}\b",
    r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum|trait|union|type|const|static|mod)\s+{}\b",
    r"^\s*macro_rules!\s*{}\b",
]);

/// JavaScript and TypeScript. The second template is the assignment form — a
/// `const` bound to an arrow or a function — which is most of a modern file.
const JS: Shapes = Shapes(&[
    r"^\s*(export\s+)?(default\s+)?(declare\s+)?(abstract\s+)?(async\s+)?(function\*?|class|interface|type|enum)\s+{}\b",
    r"^\s*(export\s+)?(declare\s+)?(const|let|var)\s+{}\s*[:=]",
    // A method or a property holding a function, at any indent: `foo(a) {` and
    // `foo: async (a) => {`. Requires the brace or arrow, so a *call* does not
    // match — which is the whole difficulty with this shape.
    r"^\s*(static\s+)?(async\s+)?(get\s+|set\s+)?{}\s*\([^;]*\)\s*\{",
    r"^\s*{}\s*[:=]\s*(async\s*)?(function\b|\([^;]*\)\s*=>)",
]);

const PYTHON: Shapes = Shapes(&[r"^\s*(async\s+)?(def|class)\s+{}\b"]);

const GO: Shapes = Shapes(&[
    r"^\s*func\s+(\([^)]*\)\s*)?{}\b",
    r"^\s*type\s+{}\b",
    r"^\s*(var|const)\s+{}\b",
]);

const JVM: Shapes = Shapes(&[
    r"^\s*[\w\s@<>\[\],.]*\b(class|interface|enum|record|trait|object)\s+{}\b",
    // A method: modifiers, a return type, the name, a parameter list, a brace.
    r"^\s*[\w\s@<>\[\],.]*\b{}\s*\([^;]*\)\s*(throws [\w\s,.]+)?\{",
]);

/// C and C++. A free function's definition and its declaration look the same
/// until the brace, so the brace is required — which means a definition split
/// across lines is missed, and that is the trade this shape makes.
const C: Shapes = Shapes(&[
    r"^\s*(typedef\s+)?(struct|class|enum|union|namespace)\s+{}\b",
    r"^\s*#\s*define\s+{}\b",
    r"^[\w\s\*&:<>,~]*\b{}\s*\([^;]*\)\s*(const\s*)?(noexcept\s*)?\{",
]);

const RUBY: Shapes = Shapes(&[r"^\s*(def|class|module)\s+(self\.)?{}\b"]);

const SHELL: Shapes = Shapes(&[r"^\s*(function\s+)?{}\s*\(\s*\)", r"^\s*function\s+{}\b"]);

/// Which shapes a file's extension implies. `None` means the caller should fall
/// back to an ordinary search, which is always a useful answer.
fn shapes_for(path: &Path) -> Option<Shapes> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => RUST,
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" => JS,
        "py" | "pyi" => PYTHON,
        "go" => GO,
        "java" | "kt" | "kts" | "scala" | "groovy" | "cs" => JVM,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" | "mm" => C,
        "rb" | "rake" => RUBY,
        "sh" | "bash" | "zsh" => SHELL,
        _ => return None,
    })
}

/// Is this a symbol at all?
///
/// The page sends whatever word was under the pointer, and a click lands on
/// punctuation and on prose as easily as on an identifier. A pattern built from
/// `"` or from a whole sentence is a query that either fails to compile or walks
/// the tree for nothing.
fn is_symbol(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Where `symbol` is defined, as far as a regular expression can tell.
///
/// `in_file` is the file it was clicked in — the language comes from its
/// extension, because a symbol has none of its own. An unknown extension, or a
/// word that is not an identifier, answers with nothing rather than guessing: the
/// page falls back to an ordinary search, which is what was wanted anyway.
///
/// Blocking, for the reason [`crate::search::search`] is.
pub fn definitions(
    root: &Path,
    exclude: Option<&str>,
    in_file: &Path,
    symbol: &str,
) -> Result<Matches> {
    if !is_symbol(symbol) {
        return Ok(Matches {
            hits: Vec::new(),
            truncated: false,
        });
    }
    let Some(shapes) = shapes_for(in_file) else {
        return Ok(Matches {
            hits: Vec::new(),
            truncated: false,
        });
    };
    let quoted = regex::escape(symbol);
    // One alternation rather than one search per shape: the walk is the cost, and
    // doing it four times over would be four times the cost for one answer.
    let pattern = shapes
        .0
        .iter()
        .map(|t| format!("(?:{})", t.replace("{}", &quoted)))
        .collect::<Vec<_>>()
        .join("|");
    search(
        root,
        exclude,
        &Query {
            pattern,
            regex: true,
            exact_case: true,
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// One file per language, each holding a definition and a *use* of the same
    /// name. The use is the point: a shape that matches a call site would send
    /// every jump to the first place the symbol is mentioned.
    fn tree() -> std::path::PathBuf {
        let dir = orchd_base::testutil::scratch_repo("symbols");
        fs::write(
            dir.join("a.rs"),
            "fn other() {\n    run_blocking(\"x\");\n}\npub async fn run_blocking(w: &str) {}\nstruct Thing;\n",
        )
        .unwrap();
        fs::write(
            dir.join("a.js"),
            "doThing()\nexport function doThing() {}\nconst helper = (x) => x\nhelper(1)\n",
        )
        .unwrap();
        fs::write(dir.join("a.py"), "widget()\ndef widget():\n    pass\n").unwrap();
        fs::write(dir.join("a.go"), "gadget()\nfunc gadget() {}\n").unwrap();
        fs::write(dir.join("a.rb"), "sprocket\ndef sprocket\nend\n").unwrap();
        fs::write(dir.join("a.sh"), "cog\ncog() {\n  :\n}\n").unwrap();
        dir
    }

    fn at(dir: &std::path::Path, file: &str, symbol: &str) -> Vec<(String, u32)> {
        definitions(dir, None, std::path::Path::new(file), symbol)
            .unwrap()
            .hits
            .into_iter()
            .map(|h| (h.path, h.line))
            .collect()
    }

    /// **The call site is what each shape has to refuse**, and every file above
    /// puts one before the definition so a shape that matched it would be caught
    /// by the line number rather than by the count.
    #[test]
    fn a_definition_is_found_and_a_call_is_not() {
        let dir = tree();
        assert_eq!(at(&dir, "a.rs", "run_blocking"), vec![("a.rs".into(), 4)]);
        assert_eq!(at(&dir, "a.rs", "Thing"), vec![("a.rs".into(), 5)]);
        assert_eq!(at(&dir, "a.js", "doThing"), vec![("a.js".into(), 2)]);
        assert_eq!(at(&dir, "a.js", "helper"), vec![("a.js".into(), 3)]);
        assert_eq!(at(&dir, "a.py", "widget"), vec![("a.py".into(), 2)]);
        assert_eq!(at(&dir, "a.go", "gadget"), vec![("a.go".into(), 2)]);
        assert_eq!(at(&dir, "a.rb", "sprocket"), vec![("a.rb".into(), 2)]);
        assert_eq!(at(&dir, "a.sh", "cog"), vec![("a.sh".into(), 2)]);
    }

    /// The language is the clicked file's, because a symbol has none. The same
    /// word in a file the shapes do not describe answers with nothing, and the
    /// caller falls back to an ordinary search.
    #[test]
    fn the_language_comes_from_the_file_it_was_clicked_in() {
        let dir = tree();
        // Rust shapes do not describe `def widget():`, so asking as Rust misses it.
        assert!(at(&dir, "a.rs", "widget").is_empty());
        assert_eq!(at(&dir, "a.py", "widget"), vec![("a.py".into(), 2)]);
        // An extension with no table at all.
        assert!(at(&dir, "notes.txt", "widget").is_empty());
    }

    /// Case is exact here, unlike a typed query: a jump moves on the strength of
    /// one hit, and folding two names together is a way to land somewhere nobody
    /// asked for.
    #[test]
    fn case_is_not_smart_for_a_definition() {
        let dir = orchd_base::testutil::scratch_repo("symbols-case");
        fs::write(dir.join("a.rs"), "fn thing() {}\nfn THING() {}\n").unwrap();
        assert_eq!(at(&dir, "a.rs", "thing"), vec![("a.rs".into(), 1)]);
        assert_eq!(at(&dir, "a.rs", "THING"), vec![("a.rs".into(), 2)]);
    }

    /// A word that is not an identifier is refused before it becomes a pattern.
    /// The page sends whatever was under the pointer, and that is punctuation and
    /// prose as often as it is a name.
    #[test]
    fn a_click_on_punctuation_is_not_a_query() {
        let dir = tree();
        for junk in ["", "  ", "fn(", "a b", "\"", "1st", &"x".repeat(200)] {
            assert!(
                at(&dir, "a.rs", junk).is_empty(),
                "{junk:?} must not be asked about"
            );
        }
    }

    /// Several definitions of one name is the common case in a trait or an
    /// interface, and it is the case the caller must not jump on. It is reported
    /// as a list, in the searcher's own order.
    #[test]
    fn several_definitions_come_back_as_several() {
        let dir = orchd_base::testutil::scratch_repo("symbols-many");
        fs::write(dir.join("one.rs"), "fn shared() {}\n").unwrap();
        fs::write(dir.join("two.rs"), "pub fn shared() {}\n").unwrap();
        assert_eq!(
            at(&dir, "one.rs", "shared"),
            vec![("one.rs".into(), 1), ("two.rs".into(), 1)]
        );
    }
}
