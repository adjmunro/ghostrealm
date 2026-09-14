//! The command registry: one catalog of named actions, three consumers
//! (palette, keybindings, agent channel).
//!
//! Generic over a context `C` (the app's state) so the core stays
//! platform-agnostic and testable: the app instantiates `Registry<AppState>`
//! and registers handlers; tests use a trivial context.

use std::collections::HashMap;
use std::fmt;

use crate::command::{ArgError, Args, CommandMeta};
use crate::fuzzy;

/// Result of running a command. Extensible; carries an optional human/agent
/// message (e.g. what changed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CmdOutcome {
    pub message: Option<String>,
}

impl CmdOutcome {
    pub fn ok() -> Self {
        CmdOutcome { message: None }
    }
    pub fn msg(text: impl Into<String>) -> Self {
        CmdOutcome {
            message: Some(text.into()),
        }
    }
}

/// Why a command failed to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CmdError {
    Unknown(String),
    Args(ArgError),
    /// The handler ran and failed.
    Failed(String),
}

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CmdError::Unknown(id) => write!(f, "unknown command `{id}`"),
            CmdError::Args(e) => write!(f, "{e}"),
            CmdError::Failed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for CmdError {}

impl From<ArgError> for CmdError {
    fn from(e: ArgError) -> Self {
        CmdError::Args(e)
    }
}

/// A handler mutates the context in response to a validated argument bag.
pub type Handler<C> = Box<dyn FnMut(&mut C, &Args) -> Result<CmdOutcome, CmdError> + 'static>;

struct Entry<C> {
    meta: CommandMeta,
    handler: Handler<C>,
}

/// A search result: the matched command and its score (higher is better).
pub struct SearchHit<'a> {
    pub meta: &'a CommandMeta,
    pub score: i32,
}

/// The registry itself.
pub struct Registry<C> {
    entries: Vec<Entry<C>>,
    index: HashMap<&'static str, usize>,
}

impl<C> Default for Registry<C> {
    fn default() -> Self {
        Registry {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl<C> Registry<C> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a command. Panics on a duplicate id — that is a programming
    /// error (two commands claiming the same id), not a runtime condition.
    pub fn register(&mut self, meta: CommandMeta, handler: Handler<C>) -> &mut Self {
        assert!(
            !self.index.contains_key(meta.id),
            "duplicate command id `{}`",
            meta.id
        );
        self.index.insert(meta.id, self.entries.len());
        self.entries.push(Entry { meta, handler });
        self
    }

    pub fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    pub fn meta(&self, id: &str) -> Option<&CommandMeta> {
        self.index.get(id).map(|&i| &self.entries[i].meta)
    }

    pub fn metas(&self) -> impl Iterator<Item = &CommandMeta> {
        self.entries.iter().map(|e| &e.meta)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fuzzy-search command titles and ids. Returns up to `limit` hits, best
    /// first, with a stable title tie-break. An empty query returns all commands
    /// in registration order.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit<'_>> {
        let mut hits: Vec<SearchHit<'_>> = self
            .entries
            .iter()
            .filter_map(|e| {
                let by_title = fuzzy::score(query, e.meta.title);
                let by_id = fuzzy::score(query, e.meta.id);
                let score = match (by_title, by_id) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                }?;
                Some(SearchHit {
                    meta: &e.meta,
                    score,
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.meta.title.cmp(b.meta.title))
        });
        hits.truncate(limit);
        hits
    }

    /// Validate `args` against the command's schema, then run it.
    pub fn execute(&mut self, id: &str, args: &Args, ctx: &mut C) -> Result<CmdOutcome, CmdError> {
        let idx = *self
            .index
            .get(id)
            .ok_or_else(|| CmdError::Unknown(id.to_string()))?;
        let entry = &mut self.entries[idx];
        args.validate(&entry.meta.args)?;
        (entry.handler)(ctx, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{ArgKind, ArgSpec, CommandMeta, Value};

    #[derive(Default)]
    struct Ctx {
        counter: i32,
        last_label: String,
    }

    fn registry() -> Registry<Ctx> {
        let mut r = Registry::new();
        r.register(
            CommandMeta::new("tab.new", "New Tab", "Open a new vertical tab"),
            Box::new(|ctx: &mut Ctx, _args| {
                ctx.counter += 1;
                Ok(CmdOutcome::msg("opened tab"))
            }),
        );
        r.register(
            CommandMeta::new("tab.rename", "Rename Tab", "Set the current tab's label")
                .arg(ArgSpec::required("label", ArgKind::Str, "new label")),
            Box::new(|ctx: &mut Ctx, args| {
                ctx.last_label = args.get_str("label")?.to_string();
                Ok(CmdOutcome::ok())
            }),
        );
        r.register(
            CommandMeta::new("print.thing", "Print Thing", "unrelated"),
            Box::new(|_ctx, _args| Ok(CmdOutcome::ok())),
        );
        r
    }

    #[test]
    fn execute_runs_handler_and_mutates_context() {
        let mut r = registry();
        let mut ctx = Ctx::default();
        r.execute("tab.new", &Args::new(), &mut ctx).unwrap();
        r.execute("tab.new", &Args::new(), &mut ctx).unwrap();
        assert_eq!(ctx.counter, 2);
    }

    #[test]
    fn execute_validates_required_args() {
        let mut r = registry();
        let mut ctx = Ctx::default();
        let err = r.execute("tab.rename", &Args::new(), &mut ctx).unwrap_err();
        assert!(
            matches!(err, CmdError::Args(ArgError::Missing(ref n)) if n == "label"),
            "expected Missing(label), got {err:?}. Next steps: check Args::validate against schema."
        );
        r.execute(
            "tab.rename",
            &Args::new().with("label", Value::Str("build".into())),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(ctx.last_label, "build");
    }

    #[test]
    fn execute_unknown_id() {
        let mut r = registry();
        let mut ctx = Ctx::default();
        assert!(matches!(
            r.execute("nope", &Args::new(), &mut ctx),
            Err(CmdError::Unknown(_))
        ));
    }

    #[test]
    fn search_ranks_word_boundary_first() {
        let r = registry();
        let hits = r.search("nt", 10);
        assert_eq!(
            hits.first().map(|h| h.meta.id),
            Some("tab.new"),
            "query 'nt' should surface 'New Tab' first; got {:?}",
            hits.iter().map(|h| h.meta.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn search_empty_returns_all() {
        let r = registry();
        assert_eq!(r.search("", 10).len(), 3);
    }

    #[test]
    #[should_panic(expected = "duplicate command id")]
    fn duplicate_id_panics() {
        let mut r: Registry<Ctx> = Registry::new();
        r.register(
            CommandMeta::new("dup", "A", ""),
            Box::new(|_, _| Ok(CmdOutcome::ok())),
        );
        r.register(
            CommandMeta::new("dup", "B", ""),
            Box::new(|_, _| Ok(CmdOutcome::ok())),
        );
    }
}
