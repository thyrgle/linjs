//! Ownership inference: which unannotated declarations could be `@own`?
//!
//! The analysis walks each body (top level or function) as a flat
//! statement list and classifies every unannotated `let`/`var` whose
//! initializer is a fresh array or object literal — or a move from
//! another own-able binding. A declaration is **own-able** when every
//! later use of it is compatible with arena semantics:
//!
//! * **No returns** — an `@own` value cannot escape, so any appearance
//!   of the name inside a `return` disqualifies it.
//! * **No container stores** — it may not be the argument of a
//!   container method call (`push`); ordinary calls borrow, so they
//!   are fine.
//! * **No closure captures** — a nested function body referencing the
//!   name could outlive the activation.
//! * **No shared aliasing** — a plain `let b = a` takes the value as a
//!   *move* only when `a` has no uses afterwards; otherwise both stay
//!   garbage-collected, because JavaScript's shared-mutation aliasing
//!   must behave identically.
//!
//! The classification is deliberately conservative: when in doubt a
//! declaration stays garbage-collected, which is always sound because
//! garbage-collected is what it already was. Optimism is limited to one
//! known limit: ordinary calls are assumed to borrow only. A callee
//! that writes through its parameter turns that assumption into a
//! runtime error (never silent divergence), which the test suite and
//! the Node differential surface immediately.
//!
//! Apply the verdicts with [`infer`] and `apply = true`: qualifying
//! declarations become `@own`, and the ordinary move/borrow/escape
//! rules come with them.

use std::collections::HashMap;

use crate::ast::{Expr, Mem, Stmt, Target};
use crate::interp::Item;

/// Why a declaration cannot be `@own`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The name appears inside a `return`.
    EscapesViaReturn,
    /// The name is an argument of a container method call (`push`).
    StoredIntoContainer,
    /// The name is used where an `@own` value cannot go — arithmetic
    /// or string coercion, which JavaScript would perform silently.
    UnsupportedUse,
    /// A nested function body references the name.
    CapturedByClosure,
    /// A plain alias `let b = a` exists and `a` is used later — the
    /// value must keep garbage-collected sharing.
    SharedAliasing,
    /// The initializer reads a value that is not an own-able binding
    /// (a parameter, a global, a call result, a disqualified alias).
    NotAFreshValue,
}

impl Reason {
    pub fn description(&self) -> &'static str {
        match self {
            Reason::EscapesViaReturn => "escapes via return",
            Reason::StoredIntoContainer => "stored into a container",
            Reason::UnsupportedUse => "used in arithmetic or coercion",
            Reason::CapturedByClosure => "captured by a closure",
            Reason::SharedAliasing => "shared through aliasing",
            Reason::NotAFreshValue => "initializer is not a fresh value",
        }
    }
}

/// The verdict for one unannotated declaration, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Could be `@own` (and becomes one under [`infer`] with
    /// `apply = true`).
    OwnAble,
    /// Stays garbage-collected, for the stated reason.
    Gc(Reason),
}

/// The inference result: one verdict per unannotated candidate
/// declaration, in declaration order across the program (functions
/// first, then top level).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inference {
    /// `(declared name, verdict)` pairs.
    pub verdicts: Vec<(String, Verdict)>,
}

/// Classifies the program's unannotated fresh-value declarations. With
/// `apply`, qualifying declarations become `@own` in place.
pub fn infer(items: &mut [Item], apply: bool) -> Inference {
    let mut verdicts = Vec::new();

    for item in items.iter_mut() {
        if let Item::Fn(def) = item {
            let mut scope = Scope::analyze(&def.body);
            scope.classify();
            let mut report = scope.verdicts();
            verdicts.append(&mut report);
            if apply {
                let owned = scope.owned_names();
                apply_body(&mut def.body, &owned);
            }
        }
    }

    let top: Vec<Stmt> = items
        .iter()
        .filter_map(|item| match item {
            Item::Stmt(stmt) => Some(stmt.clone()),
            Item::Fn(_) => None,
        })
        .collect();
    let mut scope = Scope::analyze(&top);
    scope.classify();
    let mut report = scope.verdicts();
    verdicts.append(&mut report);
    if apply {
        let owned = scope.owned_names();
        for item in items.iter_mut() {
            if let Item::Stmt(stmt) = item {
                apply_body_stmt(stmt, &owned);
            }
        }
    }

    Inference { verdicts }
}

/// One candidate declaration.
#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    /// Verdict so far; starts own-able and can only get worse.
    verdict: Verdict,
}

/// One analyzable body.
struct Scope {
    candidates: Vec<Candidate>,
    /// The candidate initializers: name -> source name for ident moves.
    moves_from: HashMap<String, String>,
    stmts: Vec<Stmt>,
}

impl Scope {
    /// Candidates: unannotated single-path declarations whose
    /// initializer is a fresh literal or an identifier.
    fn analyze(stmts: &[Stmt]) -> Scope {
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut moves_from = HashMap::new();
        for stmt in stmts {
            let (mem, decls): (Mem, &Vec<(String, Option<Expr>)>) = match stmt {
                Stmt::Let { mem, decls, .. } => (*mem, decls),
                Stmt::Var { mem, decls } => (*mem, decls),
                _ => continue,
            };
            if mem != Mem::Gc {
                // Explicitly annotated declarations are not candidates.
                for (name, _) in decls {
                    moves_from.remove(name);
                }
                continue;
            }
            for (name, init) in decls {
                match init.as_ref() {
                    Some(Expr::Array(_) | Expr::Obj(_)) => {
                        candidates.push(Candidate {
                            name: name.clone(),
                            verdict: Verdict::OwnAble,
                        });
                    }
                    Some(Expr::Ident(source)) if source != name => {
                        // A potential move; own-ability depends on the
                        // source, resolved by `resolve_moves`.
                        candidates.push(Candidate {
                            name: name.clone(),
                            verdict: Verdict::OwnAble,
                        });
                        moves_from.insert(name.clone(), source.clone());
                    }
                    _ => {}
                }
            }
        }
        Scope {
            candidates,
            moves_from,
            stmts: stmts.to_vec(),
        }
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.candidates.iter().position(|c| c.name == name)
    }

    fn disqualify(&mut self, name: &str, reason: Reason) {
        if let Some(i) = self.find(name) {
            if matches!(self.candidates[i].verdict, Verdict::OwnAble) {
                self.candidates[i].verdict = Verdict::Gc(reason);
            }
        }
    }

    fn is_own_able(&self, name: &str) -> bool {
        self.find(name)
            .is_some_and(|i| matches!(self.candidates[i].verdict, Verdict::OwnAble))
    }

    /// Runs the disqualification rules over every candidate.
    fn classify(&mut self) {
        // Snapshot the candidate names; the set never changes.
        let names: Vec<String> = self.candidates.iter().map(|c| c.name.clone()).collect();

        for name in &names {
            // Which statement declares it, and where its initializer
            // reads another candidate (a move).
            let mut declared_at: Option<usize> = None;
            for (i, stmt) in self.stmts.iter().enumerate() {
                if let Some((declared, _init)) = single_decl(stmt) {
                    if declared == name {
                        declared_at = Some(i);
                    }
                }
            }
            let declared_at = declared_at;

            // Closure captures anywhere in the body.
            let snapshot = self.stmts.clone();
            for stmt in &snapshot {
                if stmt_captures(stmt, name) {
                    self.disqualify(name, Reason::CapturedByClosure);
                }
            }

            // Uses of the name, statement by statement.
            for (i, stmt) in snapshot.iter().enumerate() {
                if declared_at == Some(i) {
                    // The declaration's own initializer mentions the
                    // name only if it is a move source — not a use.
                    continue;
                }
                let mentions = {
                    let mut found = false;
                    collect_uses(stmt, &mut |n| {
                        if n == name {
                            found = true;
                        }
                    });
                    found
                };
                if !mentions {
                    continue;
                }
                if let Stmt::Return(Some(expr)) = stmt {
                    if return_escapes(expr, name) {
                        self.disqualify(name, Reason::EscapesViaReturn);
                        continue;
                    }
                } else if matches!(stmt, Stmt::Return(None)) {
                    continue;
                }
                if stmt_stores_into_container(stmt, name) {
                    self.disqualify(name, Reason::StoredIntoContainer);
                    continue;
                }
                // A move `let other = name`?  Sound only if `name` has
                // no uses after this statement.
                if let Some((other, source)) = single_ident_decl(stmt) {
                    if source == name && other != name {
                        let later_used = snapshot[i + 1..].iter().any(|later| {
                            let mut found = false;
                            collect_uses(later, &mut |n| {
                                if n == name {
                                    found = true;
                                }
                            });
                            found
                        });
                        if later_used {
                            self.disqualify(name, Reason::SharedAliasing);
                        }
                        continue;
                    }
                    let _ = other;
                }
                if stmt_uses_in_arithmetic(stmt, name) {
                    self.disqualify(name, Reason::UnsupportedUse);
                    continue;
                }
                // Every other mention is a plain use: reads, mutation
                // targets, call arguments (which borrow).
            }

            // A move from a name that is not a candidate at all (a
            // parameter, a call result, a scalar) is not a fresh value.
            if let Some(source) = self.moves_from.get(name).cloned() {
                if !names.contains(&source) {
                    self.disqualify(name, Reason::NotAFreshValue);
                }
            }
        }

        // Fixpoint: a Gc source poisons its move targets.
        loop {
            let mut changed = false;
            for (target, source) in self.moves_from.clone() {
                if !self.is_own_able(&source) && self.is_own_able(&target) {
                    self.disqualify(&target, Reason::NotAFreshValue);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn verdicts(&self) -> Vec<(String, Verdict)> {
        self.candidates
            .iter()
            .map(|c| (c.name.clone(), c.verdict.clone()))
            .collect()
    }

    fn owned_names(&self) -> Vec<String> {
        self.candidates
            .iter()
            .filter(|c| matches!(c.verdict, Verdict::OwnAble))
            .map(|c| c.name.clone())
            .collect()
    }
}

/// The single declarator of a declaration statement, if it has exactly
/// one: `(name, init)`.
fn single_decl(stmt: &Stmt) -> Option<(&String, &Option<Expr>)> {
    let decls = match stmt {
        Stmt::Let { decls, mem, .. } if *mem == Mem::Gc => decls,
        Stmt::Var { decls, mem } if *mem == Mem::Gc => decls,
        _ => return None,
    };
    if decls.len() == 1 {
        let (name, init) = &decls[0];
        Some((name, init))
    } else {
        None
    }
}

/// `(other, source)` when the statement is exactly `let other = source;`.
fn single_ident_decl(stmt: &Stmt) -> Option<(&String, &str)> {
    let (name, init) = single_decl(stmt)?;
    match init.as_ref()? {
        Expr::Ident(source) => Some((name, source)),
        _ => None,
    }
}

/// Whether any nested function body inside the statement references
/// `name` (a capture that could outlive the activation).
fn stmt_captures(stmt: &Stmt, name: &str) -> bool {
    let mut found = false;
    walk_stmt(stmt, &mut |expr| {
        match expr {
            Expr::Arrow(_, body) => {
                if body_mentions(body, name) {
                    found = true;
                }
            }
            Expr::Fn(_, _, stmts) => {
                found |= stmts.iter().any(|s| body_or_nested_mentions(s, name));
            }
            _ => {}
        };
    });
    found
}

fn body_or_nested_mentions(stmt: &Stmt, name: &str) -> bool {
    let mut found = false;
    collect_uses(stmt, &mut |n| {
        if n == name {
            found = true;
        }
    });
    found
}

fn body_mentions(body: &crate::ast::FnBody, name: &str) -> bool {
    match body {
        crate::ast::FnBody::Expr(expr) => expr_mentions(expr, name),
        crate::ast::FnBody::Block(stmts) => stmts.iter().any(|s| body_or_nested_mentions(s, name)),
    }
}

/// Whether returning `expr` would move `name` out: the name appears
/// as a value, not merely as a projection receiver (`local.length`,
/// `local[0]`) or inside a call argument (`f(local)` — a borrow).
fn return_escapes(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Ident(id) => id == name,
        Expr::Array(items) => items.iter().any(|e| return_escapes(e, name)),
        Expr::Obj(entries) => entries.iter().any(|e| return_escapes(&e.value, name)),
        Expr::Ternary(c, t, e) => {
            expr_mentions(c, name) || return_escapes(t, name) || return_escapes(e, name)
        }
        // Projections produce fresh scalars; call arguments borrow.
        Expr::Index(..) | Expr::Member(..) | Expr::Call(..) => false,
        // `local || x` can yield the value itself; arithmetic coerces.
        Expr::Logical(_, l, r) => return_escapes(l, name) || return_escapes(r, name),
        _ => expr_mentions(expr, name),
    }
}

/// Whether `name` appears inside arithmetic or coercion positions,
/// where JavaScript would silently coerce but the interpreter rejects
/// `@own` operands.
fn stmt_uses_in_arithmetic(stmt: &Stmt, name: &str) -> bool {
    let mut found = false;
    walk_stmt(stmt, &mut |expr| match expr {
        Expr::Binary(..) | Expr::Unary(crate::ast::UnaryOp::Neg, _) => {
            found |= expr_mentions(expr, name);
        }
        _ => {}
    });
    found
}

/// Whether the statement uses `name` as the argument of a container
/// method call (`x.push(name)`): a store into an unknown-typed
/// receiver.
fn stmt_stores_into_container(stmt: &Stmt, name: &str) -> bool {
    let mut found = false;
    walk_stmt(stmt, &mut |expr| {
        if let Expr::Call(callee, args) = expr {
            if let Expr::Member(_, prop) = &**callee {
                if prop == "push" && args.iter().any(|a| expr_mentions(a, name)) {
                    found = true;
                }
            }
        }
    });
    found
}

fn walk_stmt(stmt: &Stmt, f: &mut impl FnMut(&Expr)) {
    collect_walk_stmt(stmt, f);
}

fn collect_walk_stmt(stmt: &Stmt, f: &mut impl FnMut(&Expr)) {
    match stmt {
        Stmt::Let { decls, .. } | Stmt::Var { decls, .. } => {
            for (_, init) in decls {
                if let Some(init) = init {
                    collect_walk_expr(init, f);
                }
            }
        }
        Stmt::Expr(expr) => collect_walk_expr(expr, f),
        Stmt::If(cond, then, els) => {
            collect_walk_expr(cond, f);
            collect_walk_stmt(then, f);
            if let Some(els) = els {
                collect_walk_stmt(els, f);
            }
        }
        Stmt::While(cond, body) => {
            collect_walk_expr(cond, f);
            collect_walk_stmt(body, f);
        }
        Stmt::For {
            init,
            cond,
            step,
            body,
        } => {
            if let Some(init) = init {
                collect_walk_stmt(init, f);
            }
            if let Some(cond) = cond {
                collect_walk_expr(cond, f);
            }
            if let Some(step) = step {
                collect_walk_expr(step, f);
            }
            collect_walk_stmt(body, f);
        }
        Stmt::ForOf { iterable, body, .. } | Stmt::ForIn { iterable, body, .. } => {
            collect_walk_expr(iterable, f);
            collect_walk_stmt(body, f);
        }
        Stmt::Block(stmts) => stmts.iter().for_each(|s| collect_walk_stmt(s, f)),
        Stmt::Return(Some(expr)) => collect_walk_expr(expr, f),
        _ => {}
    }
}

fn collect_walk_expr(expr: &Expr, f: &mut impl FnMut(&Expr)) {
    f(expr);
    match expr {
        Expr::Array(items) => items.iter().for_each(|e| collect_walk_expr(e, f)),
        Expr::Obj(entries) => entries.iter().for_each(|e| collect_walk_expr(&e.value, f)),
        Expr::Unary(_, e) => collect_walk_expr(e, f),
        Expr::Binary(_, l, r) | Expr::Logical(_, l, r) | Expr::Eq(_, l, r) => {
            collect_walk_expr(l, f);
            collect_walk_expr(r, f);
        }
        Expr::Assign(target, value) => {
            match target {
                Target::Ident(name) => f(&Expr::Ident(name.clone())),
                Target::Index(obj, idx) => {
                    collect_walk_expr(obj, f);
                    collect_walk_expr(idx, f);
                }
                Target::Member(obj, _) => collect_walk_expr(obj, f),
            }
            collect_walk_expr(value, f);
        }
        Expr::Index(obj, idx) => {
            collect_walk_expr(obj, f);
            collect_walk_expr(idx, f);
        }
        Expr::Member(obj, _) => collect_walk_expr(obj, f),
        Expr::Call(callee, args) => {
            collect_walk_expr(callee, f);
            args.iter().for_each(|a| collect_walk_expr(a, f));
        }
        Expr::Ternary(c, t, e) => {
            collect_walk_expr(c, f);
            collect_walk_expr(t, f);
            collect_walk_expr(e, f);
        }
        Expr::Arrow(..) | Expr::Fn(..) | Expr::Update(_, _, _) => {}
        _ => {}
    }
}

fn expr_mentions(expr: &Expr, name: &str) -> bool {
    let mut found = false;
    collect_uses_expr(expr, &mut |n| {
        if n == name {
            found = true;
        }
    });
    found
}

/// Collects identifier uses in a statement, skipping nested function
/// bodies (captures are handled separately by `stmt_captures`).
pub fn collect_uses(stmt: &Stmt, out: &mut impl FnMut(&str)) {
    match stmt {
        Stmt::Let { decls, .. } | Stmt::Var { decls, .. } => {
            for (_, init) in decls {
                if let Some(init) = init {
                    collect_uses_expr(init, out);
                }
            }
        }
        Stmt::Expr(expr) => collect_uses_expr(expr, out),
        Stmt::If(cond, then, els) => {
            collect_uses_expr(cond, out);
            collect_uses(then, out);
            if let Some(els) = els {
                collect_uses(els, out);
            }
        }
        Stmt::While(cond, body) => {
            collect_uses_expr(cond, out);
            collect_uses(body, out);
        }
        Stmt::For {
            init,
            cond,
            step,
            body,
        } => {
            if let Some(init) = init {
                collect_uses(init, out);
            }
            if let Some(cond) = cond {
                collect_uses_expr(cond, out);
            }
            if let Some(step) = step {
                collect_uses_expr(step, out);
            }
            collect_uses(body, out);
        }
        Stmt::ForOf { iterable, body, .. } | Stmt::ForIn { iterable, body, .. } => {
            collect_uses_expr(iterable, out);
            collect_uses(body, out);
        }
        Stmt::Block(stmts) => stmts.iter().for_each(|s| collect_uses(s, out)),
        Stmt::Return(Some(expr)) => collect_uses_expr(expr, out),
        _ => {}
    }
}

fn collect_uses_expr(expr: &Expr, out: &mut impl FnMut(&str)) {
    match expr {
        Expr::Ident(name) => out(name),
        Expr::Array(items) => items.iter().for_each(|e| collect_uses_expr(e, out)),
        Expr::Obj(entries) => entries
            .iter()
            .for_each(|e| collect_uses_expr(&e.value, out)),
        Expr::Unary(_, e) => collect_uses_expr(e, out),
        Expr::Binary(_, l, r) | Expr::Logical(_, l, r) | Expr::Eq(_, l, r) => {
            collect_uses_expr(l, out);
            collect_uses_expr(r, out);
        }
        Expr::Assign(target, value) => {
            collect_target_uses(target, out);
            collect_uses_expr(value, out);
        }
        Expr::Index(obj, idx) => {
            collect_uses_expr(obj, out);
            collect_uses_expr(idx, out);
        }
        Expr::Member(obj, _) => collect_uses_expr(obj, out),
        Expr::Call(callee, args) => {
            collect_uses_expr(callee, out);
            args.iter().for_each(|a| collect_uses_expr(a, out));
        }
        Expr::Ternary(c, t, e) => {
            collect_uses_expr(c, out);
            collect_uses_expr(t, out);
            collect_uses_expr(e, out);
        }
        Expr::Update(_, _, target) => collect_target_uses(target, out),
        Expr::Arrow(..) | Expr::Fn(..) => {}
        _ => {}
    }
}

fn collect_target_uses(target: &Target, out: &mut impl FnMut(&str)) {
    match target {
        Target::Ident(name) => out(name),
        Target::Index(obj, idx) => {
            collect_uses_expr(obj, out);
            collect_uses_expr(idx, out);
        }
        Target::Member(obj, _) => collect_uses_expr(obj, out),
    }
}

/// Sets `Mem::Own` on the qualifying declarations of a body.
fn apply_body(body: &mut [Stmt], owned: &[String]) {
    body.iter_mut()
        .for_each(|stmt| apply_body_stmt(stmt, owned));
}

fn apply_body_stmt(stmt: &mut Stmt, owned: &[String]) {
    match stmt {
        Stmt::Let { decls, mem, .. } | Stmt::Var { decls, mem } => {
            let qualifies = *mem == Mem::Gc && decls.len() == 1 && owned.contains(&decls[0].0);
            if qualifies {
                *mem = Mem::Own;
            }
        }
        _ => {}
    }
}
