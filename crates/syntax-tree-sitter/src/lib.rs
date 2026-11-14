//! Syntax trees from tree-sitter behind `ghostrealm_core::syntax`: a
//! registry of grammars by file extension, each with a highlight query whose
//! capture names map to [`Code`] colours. Nothing outside this crate names a
//! tree-sitter type.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use ghostrealm_core::fs_tree::Waker;
use ghostrealm_core::highlight::Code;
use ghostrealm_core::syntax::SyntaxDoc;
use tree_sitter::{Language, Query};

mod doc;

pub use doc::WORKER_BYTES;

/// A language with a grammar.
struct Grammar {
    /// Its name, as [`SyntaxDoc::name`] gives it.
    name: &'static str,
    /// Lowercase file extensions (no dot) it claims.
    extensions: &'static [&'static str],
    language: fn() -> Language,
    /// The highlight query, in parts joined in order: patterns of ours put
    /// before a grammar's own take its nodes first.
    query: &'static [&'static str],
    /// The compiled query; `None` if it doesn't compile (the language then
    /// opens plain).
    loaded: OnceLock<Option<Arc<Loaded>>>,
}

/// A grammar's language and its compiled highlight query.
pub(crate) struct Loaded {
    pub name: &'static str,
    pub language: Language,
    pub query: Query,
    /// Each capture's colour, by capture index; `None` for plain.
    pub codes: Vec<Option<Code>>,
}

static GRAMMARS: [Grammar; 3] = [
    Grammar {
        name: "kotlin",
        extensions: &["kt", "kts"],
        language: || tree_sitter_kotlin::LANGUAGE.into(),
        query: &[include_str!("../queries/kotlin.scm")],
        loaded: OnceLock::new(),
    },
    Grammar {
        name: "java",
        extensions: &["java"],
        language: || tree_sitter_java::LANGUAGE.into(),
        query: &[include_str!("../queries/java.scm"), tree_sitter_java::HIGHLIGHTS_QUERY],
        loaded: OnceLock::new(),
    },
    Grammar {
        name: "rust",
        extensions: &["rs"],
        language: || tree_sitter_rust::LANGUAGE.into(),
        query: &[include_str!("../queries/rust.scm"), tree_sitter_rust::HIGHLIGHTS_QUERY],
        loaded: OnceLock::new(),
    },
];

impl Grammar {
    /// The compiled query, compiled the first time it is asked for.
    fn loaded(&self) -> Option<Arc<Loaded>> {
        self.loaded.get_or_init(|| self.load().ok().map(Arc::new)).clone()
    }

    fn load(&self) -> Result<Loaded, String> {
        let language = (self.language)();
        let query = Query::new(&language, &self.query.concat()).map_err(|e| format!("{}: {e}", self.name))?;
        let codes = query.capture_names().iter().map(|n| code_of(n)).collect();
        Ok(Loaded { name: self.name, language, query, codes })
    }
}

/// The grammar claiming `path`'s extension (case-insensitive).
fn grammar_for(path: &Path) -> Option<&'static Grammar> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    GRAMMARS.iter().find(|g| g.extensions.contains(&ext.as_str()))
}

/// The name of the language `path` opens in, if it has a grammar whose
/// query compiles: a view holding a document of another name (or none)
/// opens a new one.
pub fn language(path: &Path) -> Option<&'static str> {
    let g = grammar_for(path)?;
    g.loaded().map(|_| g.name)
}

/// A syntax document for a file at `path`, if its language has a grammar.
/// It has no tree until its first [`SyntaxDoc::update`]; a parse on the
/// worker calls `waker` when it lands.
pub fn open(path: &Path, waker: Option<Waker>) -> Option<Box<dyn SyntaxDoc>> {
    let loaded = grammar_for(path)?.loaded()?;
    Some(Box::new(doc::TreeDoc::new(loaded, waker)))
}

/// Capture names and the colour each gives. A name not listed falls back to
/// its prefix with its last dotted segment dropped (`function.method` is a
/// `function`).
const CODES: &[(&str, Code)] = &[
    ("comment", Code::Comment),
    ("keyword", Code::Keyword),
    ("include", Code::Keyword),
    ("conditional", Code::Keyword),
    ("repeat", Code::Keyword),
    ("exception", Code::Keyword),
    ("variable.builtin", Code::Keyword),
    ("string", Code::String),
    ("character", Code::String),
    ("escape", Code::String),
    ("number", Code::Number),
    ("float", Code::Number),
    ("constant", Code::Constant),
    ("boolean", Code::Constant),
    ("type", Code::Type),
    ("constructor", Code::Type),
    ("namespace", Code::Type),
    ("module", Code::Type),
    ("function", Code::Function),
    ("method", Code::Function),
    ("property", Code::Property),
    ("field", Code::Property),
    ("variable.member", Code::Property),
    ("attribute", Code::Attribute),
    ("annotation", Code::Attribute),
    ("decorator", Code::Attribute),
    ("operator", Code::Operator),
    ("punctuation", Code::Punctuation),
];

/// Capture names drawn plain on purpose: a plain capture inside a coloured
/// node draws it plain (an interpolation inside a string).
const PLAIN: &[&str] = &["variable", "variable.parameter", "label", "none"];

/// The colour capture `name` gives, `None` for plain: listed names by their
/// longest listed prefix; names starting with `_` (a query's own helpers)
/// and unknown names plain.
pub(crate) fn code_of(name: &str) -> Option<Code> {
    lookup(name).flatten()
}

/// `Some(colour)` for a name in [`CODES`] or [`PLAIN`] (by prefix), `None`
/// for one in neither.
fn lookup(name: &str) -> Option<Option<Code>> {
    if name.starts_with('_') {
        return Some(None);
    }
    let mut n = name;
    loop {
        if let Some((_, c)) = CODES.iter().find(|(k, _)| *k == n) {
            return Some(Some(*c));
        }
        if PLAIN.contains(&n) {
            return Some(None);
        }
        n = &n[..n.rfind('.')?];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_loads_and_its_query_compiles() {
        for g in &GRAMMARS {
            let loaded = g.load().unwrap_or_else(|e| panic!("{e}"));
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&loaded.language).expect("the grammar's ABI is accepted");
            assert!(loaded.query.pattern_count() > 10, "{}", g.name);
        }
    }

    #[test]
    fn every_capture_name_a_query_uses_is_mapped_or_plain() {
        for g in &GRAMMARS {
            let loaded = g.loaded().expect("compiles");
            for name in loaded.query.capture_names() {
                assert!(lookup(name).is_some(), "{}: capture @{name} is neither mapped nor plain", g.name);
            }
        }
    }

    #[test]
    fn capture_names_map_by_their_longest_listed_prefix() {
        assert_eq!(code_of("keyword"), Some(Code::Keyword));
        assert_eq!(code_of("keyword.function"), Some(Code::Keyword));
        assert_eq!(code_of("function.method"), Some(Code::Function));
        assert_eq!(code_of("function.builtin.static"), Some(Code::Function));
        assert_eq!(code_of("variable.builtin"), Some(Code::Keyword));
        assert_eq!(code_of("variable.member"), Some(Code::Property));
        assert_eq!(code_of("variable"), None);
        assert_eq!(code_of("variable.parameter"), None);
        assert_eq!(code_of("string.escape"), Some(Code::String));
        assert_eq!(code_of("constant.builtin"), Some(Code::Constant));
        assert_eq!(code_of("punctuation.bracket"), Some(Code::Punctuation));
        assert_eq!(code_of("_function"), None);
        assert_eq!(lookup("_function"), Some(None));
        assert_eq!(lookup("weird.name"), None, "unknown: neither mapped nor plain");
    }

    #[test]
    fn languages_are_found_by_extension() {
        assert_eq!(language(Path::new("a/Main.kt")), Some("kotlin"));
        assert_eq!(language(Path::new("build.gradle.kts")), Some("kotlin"));
        assert_eq!(language(Path::new("A.JAVA")), Some("java"));
        assert_eq!(language(Path::new("lib.rs")), Some("rust"));
        assert_eq!(language(Path::new("notes.md")), None);
        assert_eq!(language(Path::new("Makefile")), None);
        assert!(open(Path::new("x.txt"), None).is_none());
        assert_eq!(open(Path::new("x.kt"), None).map(|d| d.name()), Some("kotlin"));
    }
}
