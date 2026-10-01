//! memjs on the engine: one pass segments the source into top-level
//! items and parses each item right where it stands.
//!
//! The parse results live in the tree as contexts — `Ctx::Fn(Arc<FnDef>)`
//! per function region, `Ctx::Stmt(Arc<Stmt>)` per top-level statement —
//! so the interpreter reads the tree without re-parsing. Untouched items
//! keep their exact `Arc` across edits via a content-keyed cache: same
//! bytes, same parse, same pointer. That is the reuse story the
//! incremental tests assert on.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use increparse::{Outcome, Pass, Span};

use crate::ast::{FnDef, Stmt};
use crate::interp::Item;
use crate::lexer::lex;
use crate::parser::{self, TopItem};

/// The per-node context of a memjs tree.
#[derive(Clone, Debug, PartialEq)]
pub enum Ctx {
    Root,
    /// A top-level function declaration; the parsed definition rides along.
    Fn(Arc<FnDef>),
    /// A top-level statement.
    Stmt(Arc<Stmt>),
    /// An item that failed to parse; the message is the diagnostic.
    Broken(Arc<str>),
}

/// The segmenting pass. Holds the parse cache.
pub struct ItemsPass {
    cache: Mutex<HashMap<(usize, usize, u64), Ctx>>,
}

impl ItemsPass {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for ItemsPass {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_slice(source: &str, span: Span) -> u64 {
    const FNV: u64 = 0xcbf2_9ce4_8422_2325;
    let mut hash = FNV;
    for b in &source.as_bytes()[span.start..span.end] {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl Pass for ItemsPass {
    type Ctx = Ctx;

    fn parse(&self, source: &str, span: Span, ctx: &Ctx) -> Outcome<Ctx> {
        if !matches!(ctx, Ctx::Root) {
            return Outcome::Done;
        }
        let lexed = match lex(source) {
            Ok(lexed) => lexed,
            Err(e) => {
                return Outcome::Expand(vec![(
                    Span::new(e.at.min(span.end), span.end.max(e.at), span.rev),
                    Ctx::Broken(Arc::from(e.message.as_str())),
                )]);
            }
        };
        let items = parser::top_level_items(source, &lexed.tokens, &lexed.comments);
        let mut children = Vec::new();
        for item in &items {
            let (start, end) = item.span();
            let child_span = Span::new(start.min(end), end.max(start), span.rev);
            let cached = {
                let mut cache = self.cache.lock().expect("parse cache");
                let key = (
                    child_span.start,
                    child_span.end,
                    hash_slice(source, child_span),
                );
                if let Some(ctx) = cache.get(&key) {
                    ctx.clone()
                } else {
                    let ctx = ctx_for_item(item);
                    cache.insert(key, ctx.clone());
                    ctx
                }
            };
            children.push((child_span, cached));
        }
        if children.is_empty() {
            Outcome::Done
        } else {
            Outcome::Expand(children)
        }
    }
}

fn ctx_for_item(item: &TopItem) -> Ctx {
    match item {
        TopItem::Fn { def: Ok(def), .. } => Ctx::Fn(Arc::new(def.clone())),
        TopItem::Fn { def: Err(e), .. } => Ctx::Broken(Arc::from(e.message.as_str())),
        TopItem::Stmt { stmt: Ok(stmt), .. } => Ctx::Stmt(Arc::new(stmt.clone())),
        TopItem::Stmt { stmt: Err(e), .. } => Ctx::Broken(Arc::from(e.message.as_str())),
    }
}

/// A no-op second pass: it gives the schedule a second round so the items
/// created by [`ItemsPass`] are processed, and every run reaches a clean
/// fixpoint.
pub struct Settle;

impl Pass for Settle {
    type Ctx = Ctx;

    fn parse(&self, _source: &str, _span: Span, _ctx: &Ctx) -> Outcome<Ctx> {
        Outcome::Done
    }
}

/// Extracts the parsed program from a settled tree, in source order.
pub fn program_from_tree(tree: &increparse::ParseTree<Ctx>) -> (Vec<Item>, Vec<(usize, String)>) {
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut children: Vec<_> = tree.children(tree.root()).to_vec();
    children.sort_by_key(|id| tree.span(*id).start);
    for id in children {
        match tree.ctx(id) {
            Ctx::Fn(def) => items.push(Item::Fn((**def).clone())),
            Ctx::Stmt(stmt) => items.push(Item::Stmt((**stmt).clone())),
            Ctx::Broken(msg) => errors.push((tree.span(id).start, msg.to_string())),
            Ctx::Root => {}
        }
    }
    (items, errors)
}

/// Parse diagnostics: byte offset plus message.
pub type Diagnostics = Vec<(usize, String)>;

/// Parses source into a program without the engine — the cold path used
/// for single-shot runs and tests.
pub fn parse_program(source: &str) -> Result<(Vec<Item>, Diagnostics), String> {
    let pass = ItemsPass::new();
    let mut session = increparse::Session::from_source(source, 0, Ctx::Root);
    let engine = increparse::Engine::with((pass, Settle));
    let report = session.run(
        &engine,
        source,
        &increparse::SerialExecutor,
        &increparse::CancelToken::new(),
    );
    if report.cancelled {
        return Err("run was cancelled".into());
    }
    Ok(program_from_tree(session.tree()))
}
