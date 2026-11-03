//! What a source file is made of: its folds, its outline, and where a name is
//! declared, read with tree-sitter.
//!
//! **In-process, which is the whole argument for it.** `symbols` turned a language
//! server down because it means a process per language per worktree, indexing,
//! and a JSON-RPC client to supervise; a grammar is a C library linked into the
//! daemon and a parse of a large file is a few milliseconds. What it buys is
//! syntax and nothing more: no types, no cross-file resolution. That is enough for
//! the viewer's folds, its sticky scope lines and its outline, and for checking
//! that a regex hit in `symbols` really is a declaration.
//!
//! **A file this does not know answers with nothing**, never with an error. The
//! viewer then draws what it always drew, which is the only degradation a reader
//! should see.

use serde::Serialize;
use std::path::Path;
use std::sync::OnceLock;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

/// Past this a file is not parsed. The viewer stops colouring at the same size,
/// for the same reason: the string is the cost, not the grammar.
const HUGE: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Js,
    Ts,
    Tsx,
    Php,
    Python,
    Go,
}

/// Every language, once: the grammar cache is sized from it and the query test
/// walks it, so a language added to the enum and not here has neither.
const ALL: [Lang; 7] = [
    Lang::Rust,
    Lang::Js,
    Lang::Ts,
    Lang::Tsx,
    Lang::Php,
    Lang::Python,
    Lang::Go,
];
const LANGS: usize = ALL.len();

impl Lang {
    fn of(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "js" | "mjs" | "cjs" | "jsx" => Lang::Js,
            "ts" | "mts" | "cts" => Lang::Ts,
            "tsx" => Lang::Tsx,
            "php" => Lang::Php,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            _ => return None,
        })
    }

    fn language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Js => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            // The grammar with the HTML around `<?php`, because that is what a
            // `.php` file is; the PHP-only one fails on the opening tag.
            Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    /// The query every answer here is read from. Three captures on the node:
    /// `@def` is a declaration with a `@name`, `@scope` is a container worth a
    /// sticky line that declares nothing (`impl Foo`), and `@var` is a binding
    /// that counts as a definition for a jump but would bury the outline — every
    /// `const` in a function body.
    fn query(self) -> &'static str {
        match self {
            Lang::Rust => {
                r#"
                (function_item name: (identifier) @name) @def
                (function_signature_item name: (identifier) @name) @def
                (struct_item name: (type_identifier) @name) @def
                (enum_item name: (type_identifier) @name) @def
                (union_item name: (type_identifier) @name) @def
                (trait_item name: (type_identifier) @name) @def
                (type_item name: (type_identifier) @name) @def
                (associated_type name: (type_identifier) @name) @var
                (mod_item name: (identifier) @name) @def
                (macro_definition name: (identifier) @name) @def
                (const_item name: (identifier) @name) @var
                (static_item name: (identifier) @name) @var
                (impl_item) @scope
                "#
            }
            Lang::Js => {
                r#"
                (function_declaration name: (identifier) @name) @def
                (generator_function_declaration name: (identifier) @name) @def
                (class_declaration name: (identifier) @name) @def
                (method_definition name: (_) @name) @def
                (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def
                (field_definition property: (property_identifier) @name value: [(arrow_function) (function_expression)]) @def
                (pair key: (property_identifier) @name value: [(arrow_function) (function_expression)]) @def
                (variable_declarator name: (identifier) @name) @var
                "#
            }
            Lang::Ts | Lang::Tsx => {
                r#"
                (function_declaration name: (identifier) @name) @def
                (generator_function_declaration name: (identifier) @name) @def
                (class_declaration name: (type_identifier) @name) @def
                (abstract_class_declaration name: (type_identifier) @name) @def
                (interface_declaration name: (type_identifier) @name) @def
                (type_alias_declaration name: (type_identifier) @name) @def
                (enum_declaration name: (identifier) @name) @def
                (method_definition name: (_) @name) @def
                (function_signature name: (identifier) @name) @var
                (public_field_definition name: (_) @name value: [(arrow_function) (function_expression)]) @def
                (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def
                (pair key: (property_identifier) @name value: [(arrow_function) (function_expression)]) @def
                (variable_declarator name: (identifier) @name) @var
                "#
            }
            Lang::Php => {
                r#"
                (function_definition name: (name) @name) @def
                (class_declaration name: (name) @name) @def
                (interface_declaration name: (name) @name) @def
                (trait_declaration name: (name) @name) @def
                (enum_declaration name: (name) @name) @def
                (method_declaration name: (name) @name) @def
                "#
            }
            Lang::Python => {
                r#"
                (function_definition name: (identifier) @name) @def
                (class_definition name: (identifier) @name) @def
                "#
            }
            Lang::Go => {
                r#"
                (function_declaration name: (identifier) @name) @def
                (method_declaration name: (field_identifier) @name) @def
                (type_spec name: (type_identifier) @name) @def
                (type_alias name: (type_identifier) @name) @def
                (var_spec name: (identifier) @name) @var
                (const_spec name: (identifier) @name) @var
                "#
            }
        }
    }

    /// Top-level statements that bring a name in from elsewhere.
    fn imports(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["use_declaration", "extern_crate_declaration"],
            Lang::Js | Lang::Ts | Lang::Tsx => &["import_statement"],
            Lang::Php => &["namespace_use_declaration"],
            Lang::Python => &[
                "import_statement",
                "import_from_statement",
                "future_import_statement",
            ],
            Lang::Go => &["import_declaration"],
        }
    }

    fn is_comment(self, kind: &str) -> bool {
        match self {
            Lang::Rust => kind == "line_comment" || kind == "block_comment",
            _ => kind == "comment",
        }
    }
}

/// A language's grammar and its compiled query, built once. The capture indexes
/// are looked up here so a match is read by number rather than by name.
struct Grammar {
    language: Language,
    query: Query,
    name: Option<u32>,
    def: Option<u32>,
    scope: Option<u32>,
    var: Option<u32>,
}

/// `None` when the query does not compile against its grammar, which is a bug in
/// the table above and is caught by `every_query_compiles` before it ships. It is
/// logged rather than unwrapped because a panic here takes the daemon down.
fn grammar(lang: Lang) -> Option<&'static Grammar> {
    static CACHE: [OnceLock<Option<Grammar>>; LANGS] = [const { OnceLock::new() }; LANGS];
    CACHE
        .get(lang as usize)?
        .get_or_init(|| {
            let language = lang.language();
            let query = Query::new(&language, lang.query())
                .map_err(|e| tracing::warn!("the {lang:?} structure query is broken: {e}"))
                .ok()?;
            Some(Grammar {
                name: query.capture_index_for_name("name"),
                def: query.capture_index_for_name("def"),
                scope: query.capture_index_for_name("scope"),
                var: query.capture_index_for_name("var"),
                language,
                query,
            })
        })
        .as_ref()
}

/// Whether a file of this name has a grammar here, asked before its text is read.
pub fn knows(path: &Path) -> bool {
    Lang::of(path).is_some()
}

/// `text` parsed as the language `path` names, with that language's grammar. None
/// for a language without one, a file past [`HUGE`], or a parse that failed.
fn parsed(path: &Path, text: &str) -> Option<(Lang, &'static Grammar, Tree)> {
    let lang = Lang::of(path)?;
    let g = grammar(lang)?;
    if text.len() > HUGE {
        return None;
    }
    let mut parser = Parser::new();
    parser.set_language(&g.language).ok()?;
    Some((lang, g, parser.parse(text, None)?))
}

/// What a fold hides, which is what decides whether it starts closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
#[serde(rename_all = "lowercase")]
pub enum FoldKind {
    /// A run of imports. The one kind the viewer folds before anybody asks: it is
    /// the top of nearly every file and the part nobody came to read.
    Imports,
    Comment,
    Code,
}

/// One foldable range. `line` stays on screen with a marker; `line + 1 ..= last`
/// is what folding it hides. Both are 1-based.
///
/// **A closing bracket on its own line is not hidden**, so a folded block reads
/// `fn run() {` · `}` the way every editor draws it, rather than a header that
/// seems to run into the next declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Fold {
    pub line: u32,
    pub last: u32,
    pub kind: FoldKind,
}

/// One declaration, for the outline and for the scope lines that stay at the top
/// of the viewer. `line ..= end` is its whole extent, 1-based, and `depth` is how
/// many other symbols contain it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Symbol {
    pub name: String,
    /// A short word for what it is, `fn`, `class`, `impl`: what the outline draws
    /// beside the name.
    pub kind: String,
    pub line: u32,
    pub end: u32,
    pub depth: u32,
}

/// A file's folds and its outline. Both empty for a language this does not know.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Structure {
    pub folds: Vec<Fold>,
    pub symbols: Vec<Symbol>,
}

/// The folds and the outline of `text`, read as the language `path` names.
pub fn structure(path: &Path, text: &str) -> Structure {
    let Some((lang, g, tree)) = parsed(path, text) else {
        return Structure::default();
    };
    let lines: Vec<&str> = text.lines().collect();
    let symbols = outline(g, &tree, text, &lines);
    let mut folds: Vec<Fold> = symbols
        .iter()
        .filter_map(|s| code_fold(s.line, s.end, &lines))
        .collect();
    folds.extend(imports_and_comments(lang, tree.root_node()));
    // One fold per header line, the widest: the page toggles a fold by the line
    // its marker is on, and two under one marker would fold as neither.
    folds.sort_by(|a, b| a.line.cmp(&b.line).then(b.last.cmp(&a.last)));
    folds.dedup_by(|later, kept| later.line == kept.line);
    Structure { folds, symbols }
}

/// The lines a name is declared on in `text`, or `None` when this does not know
/// the language — which the caller must read as "cannot say", not as "nowhere".
pub fn declared_on(path: &Path, text: &str, name: &str) -> Option<Vec<u32>> {
    let (_, g, tree) = parsed(path, text)?;
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&g.query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        let is_decl = m
            .captures()
            .iter()
            .any(|c| Some(c.index) == g.def || Some(c.index) == g.var);
        let named = m.captures().iter().find(|c| Some(c.index) == g.name);
        if let (true, Some(n)) = (is_decl, named) {
            if n.node.utf8_text(text.as_bytes()) == Ok(name) {
                out.push(n.node.start_position().row as u32 + 1);
            }
        }
    }
    out.dedup();
    Some(out)
}

fn outline(g: &Grammar, tree: &Tree, text: &str, lines: &[&str]) -> Vec<Symbol> {
    let mut found: Vec<Symbol> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&g.query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        let node = m
            .captures()
            .iter()
            .find(|c| Some(c.index) == g.def || Some(c.index) == g.scope)
            .map(|c| c.node);
        let Some(node) = node else { continue };
        let line = node.start_position().row as u32 + 1;
        let name = match m.captures().iter().find(|c| Some(c.index) == g.name) {
            Some(c) => c.node.utf8_text(text.as_bytes()).unwrap_or("").to_string(),
            // A scope names itself by its header: `impl Display for Order`, which
            // says more than the type alone would.
            None => header(lines, line),
        };
        found.push(Symbol {
            name,
            kind: kind_word(node.kind()).to_string(),
            line,
            end: end_line(node),
            depth: 0,
        });
    }
    // Outermost first at each line, so the depth below can be counted by what is
    // still open.
    found.sort_by(|a, b| a.line.cmp(&b.line).then(b.end.cmp(&a.end)));
    found.dedup_by(|later, kept| later.line == kept.line && later.end == kept.end);
    let mut open: Vec<u32> = Vec::new();
    for s in &mut found {
        while open.last().is_some_and(|&end| end < s.line) {
            open.pop();
        }
        s.depth = open.len() as u32;
        open.push(s.end);
    }
    found
}

/// The last line a node is on. A node that ends at column 0 has only reached the
/// newline before it, which is not a line it is on.
fn end_line(node: Node) -> u32 {
    let end = node.end_position();
    let row = if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    };
    row as u32 + 1
}

/// A header line as a label: trimmed, its opening brace dropped, and cut short.
fn header(lines: &[&str], line: u32) -> String {
    let text = lines
        .get(line as usize - 1)
        .map(|l| l.trim().trim_end_matches('{').trim_end())
        .unwrap_or("");
    text.chars().take(80).collect()
}

fn kind_word(kind: &str) -> &'static str {
    match kind {
        "function_item"
        | "function_signature_item"
        | "function_declaration"
        | "function_definition"
        | "generator_function_declaration"
        | "variable_declarator"
        | "pair"
        | "field_definition"
        | "public_field_definition" => "fn",
        "method_definition" | "method_declaration" => "method",
        "class_declaration" | "class_definition" | "abstract_class_declaration" => "class",
        "struct_item" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "union_item" => "union",
        "trait_item" | "trait_declaration" => "trait",
        "interface_declaration" => "interface",
        "type_item" | "type_alias_declaration" | "type_spec" | "type_alias" => "type",
        "mod_item" => "mod",
        "impl_item" => "impl",
        "macro_definition" => "macro",
        _ => "symbol",
    }
}

/// A declaration's fold, or none when it fits on two lines: hiding one line
/// behind a marker saves nothing.
fn code_fold(line: u32, end: u32, lines: &[&str]) -> Option<Fold> {
    let closer = lines.get(end as usize - 1).is_some_and(|l| {
        matches!(
            l.trim(),
            "}" | "};" | "}," | ")" | ");" | "]" | "];" | "})" | "});"
        )
    });
    let last = if closer { end - 1 } else { end };
    (last > line).then_some(Fold {
        line,
        last,
        kind: FoldKind::Code,
    })
}

/// The import runs at the top level, and every run of comments three lines long
/// or more anywhere in the file.
fn imports_and_comments(lang: Lang, root: Node) -> Vec<Fold> {
    let mut out = Vec::new();
    // Imports only at the top: an import inside a function is the code itself.
    let mut run: Option<(u32, u32)> = None;
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if lang.imports().contains(&child.kind()) {
            let (s, e) = (child.start_position().row as u32 + 1, end_line(child));
            run = Some(run.map_or((s, e), |(first, _)| (first, e)));
        } else if !lang.is_comment(child.kind()) || run.is_none() {
            // A comment between two imports belongs to the run; anything else ends it.
            push(&mut out, run.take(), FoldKind::Imports, 1);
        }
    }
    push(&mut out, run, FoldKind::Imports, 1);

    // Comments at every level, walked without recursion: a deep file is a deep
    // stack, and the daemon's threads do not get a large one.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        let mut run: Option<(u32, u32)> = None;
        for child in node.children(&mut cursor) {
            if lang.is_comment(child.kind()) {
                let (s, e) = (child.start_position().row as u32 + 1, end_line(child));
                run = match run {
                    Some((first, last)) if s <= last + 1 => Some((first, e)),
                    other => {
                        push(&mut out, other, FoldKind::Comment, 2);
                        Some((s, e))
                    }
                };
            } else {
                push(&mut out, run.take(), FoldKind::Comment, 2);
                if child.child_count() > 0 {
                    stack.push(child);
                }
            }
        }
        push(&mut out, run, FoldKind::Comment, 2);
    }
    out
}

/// A run as a fold, when it hides at least `hides` lines.
fn push(out: &mut Vec<Fold>, run: Option<(u32, u32)>, kind: FoldKind, hides: u32) {
    if let Some((line, last)) = run {
        if last >= line + hides {
            out.push(Fold { line, last, kind });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A query that does not compile is a language that silently has no
    /// structure**, because `grammar` logs and answers `None` rather than panic.
    /// So every one is compiled here, against the grammar it is written for.
    #[test]
    fn every_query_compiles() {
        for lang in ALL {
            let language = lang.language();
            if let Err(e) = Query::new(&language, lang.query()) {
                panic!("{lang:?}: {e}");
            }
        }
    }

    fn of(file: &str, text: &str) -> Structure {
        structure(Path::new(file), text)
    }

    fn folds(s: &Structure) -> Vec<(u32, u32, FoldKind)> {
        s.folds.iter().map(|f| (f.line, f.last, f.kind)).collect()
    }

    fn names(s: &Structure) -> Vec<(&str, &str, u32, u32)> {
        s.symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str(), s.line, s.depth))
            .collect()
    }

    /// The imports fold from the first to the last, a comment between them
    /// included; a function folds to above its closing brace; an `impl` is a scope
    /// named by its header, and its methods sit one level inside it.
    #[test]
    fn rust_folds_its_imports_and_nests_an_impl() {
        let s = of(
            "a.rs",
            "use std::fs;\n// why\nuse std::io;\n\nstruct Order;\n\nimpl Display for Order {\n    fn fmt(&self) {\n        a();\n        b();\n    }\n}\n",
        );
        assert_eq!(
            folds(&s),
            vec![
                (1, 3, FoldKind::Imports),
                (7, 11, FoldKind::Code),
                (8, 10, FoldKind::Code),
            ]
        );
        assert_eq!(
            names(&s),
            vec![
                ("Order", "struct", 5, 0),
                ("impl Display for Order", "impl", 7, 0),
                ("fmt", "fn", 8, 1),
            ]
        );
    }

    /// One import is not a run worth a marker, and a short comment is not either.
    #[test]
    fn a_single_import_and_a_short_comment_do_not_fold() {
        let s = of(
            "a.ts",
            "import { a } from './a';\n// one\n// two\nexport const x = 1;\n",
        );
        assert!(folds(&s).is_empty(), "{:?}", folds(&s));
    }

    /// PHP is the language the monorepo here is written in, and its braces sit on
    /// their own line: the method folds from its signature, and the `use` lines
    /// after the namespace are one run.
    #[test]
    fn php_folds_from_the_signature_and_runs_the_uses() {
        let s = of(
            "a.php",
            "<?php\nnamespace App;\n\nuse A\\B;\nuse C\\D;\n\nclass Order\n{\n    public function total(): int\n    {\n        return 1;\n    }\n}\n",
        );
        assert_eq!(
            folds(&s),
            vec![
                (4, 5, FoldKind::Imports),
                (7, 12, FoldKind::Code),
                (9, 11, FoldKind::Code),
            ]
        );
        assert_eq!(
            names(&s),
            vec![("Order", "class", 7, 0), ("total", "method", 9, 1)]
        );
    }

    /// A doc comment three lines long folds; the module's own `//!` header is a
    /// comment run like any other, and does not start closed.
    #[test]
    fn a_long_comment_run_folds_as_a_comment() {
        let s = of("a.rs", "//! one\n//! two\n//! three\nfn a() {}\n");
        assert_eq!(folds(&s), vec![(1, 3, FoldKind::Comment)]);
    }

    /// TypeScript's members: a method, and an arrow function held in a field,
    /// which is how an Angular component writes half its handlers.
    #[test]
    fn typescript_outlines_methods_and_arrow_fields() {
        let s = of(
            "a.ts",
            "export class Cart {\n  total(): number {\n    return 1;\n  }\n  clear = () => {\n    this.x = 0;\n  };\n}\n",
        );
        assert_eq!(
            names(&s),
            vec![
                ("Cart", "class", 1, 0),
                ("total", "method", 2, 1),
                ("clear", "fn", 5, 1)
            ]
        );
    }

    /// A language without a grammar answers with nothing, which the viewer draws
    /// as the file it always drew.
    #[test]
    fn an_unknown_language_has_no_structure() {
        let s = of("a.txt", "use std::fs;\nuse std::io;\n");
        assert!(s.folds.is_empty() && s.symbols.is_empty());
        assert_eq!(declared_on(Path::new("a.txt"), "x", "x"), None);
    }

    /// The check `symbols` makes on a regex hit: a declaration is one, a type
    /// member that only looks like one is not, and a call is not.
    #[test]
    fn a_declaration_is_told_from_a_lookalike() {
        let ts = "interface P {\n  onChange: (v: string) => void;\n}\nconst onChange = (v: string) => {};\nonChange('x');\n";
        assert_eq!(
            declared_on(Path::new("a.ts"), ts, "onChange"),
            Some(vec![4])
        );
        let rs = "const LIMIT: u32 = 4;\nfn run() { run(); }\n";
        assert_eq!(declared_on(Path::new("a.rs"), rs, "LIMIT"), Some(vec![1]));
        assert_eq!(declared_on(Path::new("a.rs"), rs, "run"), Some(vec![2]));
    }

    /// Forms the regex shapes in `symbols` match, so the parse must agree they
    /// are declarations or `confirmed` drops them beside any other hit.
    #[test]
    fn every_shape_symbols_matches_is_a_declaration_here() {
        let cases = [
            ("a.js", "class A {\n  clear = () => {};\n}\n", "clear"),
            ("a.go", "package a\n\ntype ID = string\n", "ID"),
            ("a.rs", "trait T {\n    type Item;\n}\n", "Item"),
            (
                "a.ts",
                "export declare function load(x: string): void;\n",
                "load",
            ),
        ];
        for (file, text, name) in cases {
            let line = text.lines().position(|l| l.contains(name)).unwrap() as u32 + 1;
            assert_eq!(
                declared_on(Path::new(file), text, name),
                Some(vec![line]),
                "{file}"
            );
        }
    }
}
