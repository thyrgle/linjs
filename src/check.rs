//! The static type checker: TS-style annotations, checked and erased.
//!
//! Types never change runtime behavior — the interpreter and VM run
//! typed programs exactly as they run untyped ones. This pass exists to
//! catch mistakes before they run (`linjs::check_program`), and to feed
//! concrete types to the WASM backend, where they decide valtypes.
//!
//! The type language is deliberately small: `number`, `string`,
//! `boolean`, `any`, and `T[]`. Compatibility is gradual: two types
//! match when they are equal, or when either side is `any`. Unannotated
//! declarations are inferred from their initializers, so untyped code
//! is checked too — inference just never rejects what it cannot prove.

use std::collections::HashMap;

use crate::ast::{Declarator, Expr, Param, Stmt, TypeAnn};
use crate::interp::Item;

/// A checker-level type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Num,
    Str,
    Bool,
    Any,
    Arr(Box<Type>),
}

impl Type {
    fn name(&self) -> String {
        match self {
            Type::Num => "number".into(),
            Type::Str => "string".into(),
            Type::Bool => "boolean".into(),
            Type::Any => "any".into(),
            Type::Arr(t) => format!("{}[]", t.name()),
        }
    }

    fn from_ann(ann: &TypeAnn) -> Type {
        match ann {
            TypeAnn::Num => Type::Num,
            TypeAnn::Str => Type::Str,
            TypeAnn::Bool => Type::Bool,
            TypeAnn::Any => Type::Any,
            TypeAnn::Array(inner) => Type::Arr(Box::new(Type::from_ann(inner))),
        }
    }
}

/// Gradual compatibility: equal, or either side is `any`.
fn compatible(a: &Type, b: &Type) -> bool {
    *a == Type::Any || *b == Type::Any || a == b
}

/// One type error: the offending variable or call plus the explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeError {
    pub subject: String,
    pub message: String,
}

impl TypeError {
    /// The full diagnostic line.
    pub fn render(&self) -> String {
        format!("type error in `{}`: {}", self.subject, self.message)
    }
}

/// Checks a program, returning every type error found (empty = clean).
/// Type annotations are erased at runtime, so a clean program behaves
/// exactly as its untyped twin.
pub fn check_program(source: &str) -> Result<Vec<TypeError>, String> {
    let (items, errors) = crate::passes::parse_program(source)?;
    if !errors.is_empty() {
        return Err(format!("program has parse errors: {}", errors[0].1));
    }

    // Function signatures first: annotated params + return types.
    let mut signatures: HashMap<String, (Vec<Type>, Type)> = HashMap::new();
    for item in &items {
        if let Item::Fn(def) = item {
            let params = def
                .params
                .iter()
                .map(|p: &Param| match &p.ann {
                    Some(ann) => Type::from_ann(ann),
                    None => Type::Any,
                })
                .collect();
            let ret = match &def.ret {
                Some(ann) => Type::from_ann(ann),
                None => Type::Any,
            };
            signatures.insert(def.name.clone(), (params, ret));
        }
    }

    let mut errors = Vec::new();
    for item in &items {
        match item {
            Item::Fn(def) => {
                let mut scope = Scope::new(&signatures);
                for p in &def.params {
                    let ty = match &p.ann {
                        Some(ann) => Type::from_ann(ann),
                        None => Type::Any,
                    };
                    scope.declare(&p.name, ty);
                }
                let body: Vec<&Stmt> = def.body.iter().collect();
                let ret = match &def.ret {
                    Some(ann) => Type::from_ann(ann),
                    None => Type::Any,
                };
                scope.check_stmts(&body, Some(&ret), &mut errors);
            }
            Item::Stmt(_) => {}
        }
    }
    let top: Vec<&Stmt> = items
        .iter()
        .filter_map(|item| match item {
            Item::Stmt(stmt) => Some(stmt),
            Item::Fn(_) => None,
        })
        .collect();
    let mut scope = Scope::new(&signatures);
    scope.check_stmts(&top, None, &mut errors);
    Ok(errors)
}

/// A lexical scope chain.
struct Scope<'s> {
    names: Vec<HashMap<String, Type>>,
    signatures: &'s HashMap<String, (Vec<Type>, Type)>,
}

impl<'s> Scope<'s> {
    fn new(signatures: &'s HashMap<String, (Vec<Type>, Type)>) -> Self {
        Scope {
            names: vec![HashMap::new()],
            signatures,
        }
    }

    fn push(&mut self) {
        self.names.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.names.pop();
    }

    fn declare(&mut self, name: &str, ty: Type) {
        self.names.last_mut().unwrap().insert(name.to_string(), ty);
    }

    fn lookup(&self, name: &str) -> Option<Type> {
        for scope in self.names.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty.clone());
            }
        }
        None
    }

    /// Assigns to an existing binding, checking compatibility. Returns
    /// None if the name is undeclared (a global — unchecked in v1).
    fn assign(&mut self, name: &str, ty: &Type) -> Option<TypeError> {
        for scope in self.names.iter_mut().rev() {
            if let Some(existing) = scope.get(name) {
                if !compatible(existing, ty) {
                    return Some(TypeError {
                        subject: name.to_string(),
                        message: format!("cannot assign {} to `{}`", ty.name(), existing.name()),
                    });
                }
                return None;
            }
        }
        None
    }

    fn check_stmts(&mut self, stmts: &[&Stmt], ret: Option<&Type>, errors: &mut Vec<TypeError>) {
        for stmt in stmts {
            self.check_stmt(stmt, ret, errors);
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt, ret: Option<&Type>, errors: &mut Vec<TypeError>) {
        match stmt {
            Stmt::Empty | Stmt::FnDecl(..) => {}
            Stmt::Let {
                decls,
                is_const: _,
                mem: _,
            }
            | Stmt::Var { decls, mem: _ } => {
                for d in decls {
                    self.check_declarator(d, errors);
                }
            }
            Stmt::Expr(expr) => {
                self.infer(expr, errors);
            }
            Stmt::If(cond, then, els) => {
                self.infer(cond, errors);
                self.push();
                self.check_stmt(then, ret, errors);
                if let Some(els) = els {
                    self.check_stmt(els, ret, errors);
                }
                self.pop();
            }
            Stmt::While(cond, body) => {
                self.infer(cond, errors);
                self.push();
                self.check_stmt(body, ret, errors);
                self.pop();
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                self.push();
                if let Some(init) = init {
                    self.check_stmt(init, ret, errors);
                }
                if let Some(cond) = cond {
                    self.infer(cond, errors);
                }
                if let Some(step) = step {
                    self.infer(step, errors);
                }
                self.check_stmt(body, ret, errors);
                self.pop();
            }
            Stmt::ForOf {
                name,
                iterable,
                body,
                ..
            } => {
                let elem = match self.infer(iterable, errors) {
                    Type::Arr(elem) => (*elem).clone(),
                    _ => Type::Any,
                };
                self.push();
                self.declare(name, elem);
                self.check_stmt(body, ret, errors);
                self.pop();
            }
            Stmt::ForIn {
                name,
                iterable,
                body,
                ..
            } => {
                self.infer(iterable, errors);
                self.push();
                self.declare(name, Type::Str);
                self.check_stmt(body, ret, errors);
                self.pop();
            }
            Stmt::Block(stmts) => {
                self.push();
                let refs: Vec<&Stmt> = stmts.iter().collect();
                self.check_stmts(&refs, ret, errors);
                self.pop();
            }
            Stmt::Return(Some(expr)) => {
                let ty = self.infer(expr, errors);
                if let Some(ret) = ret {
                    if !compatible(&ty, ret) {
                        errors.push(TypeError {
                            subject: "return".into(),
                            message: format!(
                                "function declares {}, returns {}",
                                ret.name(),
                                ty.name()
                            ),
                        });
                    }
                }
            }
            Stmt::Return(None) | Stmt::Break | Stmt::Continue => {}
        }
    }

    fn check_declarator(&mut self, d: &Declarator, errors: &mut Vec<TypeError>) {
        match (&d.ann, &d.init) {
            (Some(ann), Some(init)) => {
                let declared = Type::from_ann(ann);
                let actual = self.infer(init, errors);
                if !compatible(&declared, &actual) {
                    errors.push(TypeError {
                        subject: d.name.clone(),
                        message: format!(
                            "initializer is {}, declared {}",
                            actual.name(),
                            declared.name()
                        ),
                    });
                }
                self.declare(&d.name, declared);
            }
            (Some(ann), None) => {
                self.declare(&d.name, Type::from_ann(ann));
            }
            (None, Some(init)) => {
                // Unannotated: no contract, so future reassignments are
                // unchecked. The variable reads as `any` — gradual.
                let _ = self.infer(init, errors);
                self.declare(&d.name, Type::Any);
            }
            (None, None) => {
                self.declare(&d.name, Type::Any);
            }
        }
    }

    /// Infers an expression's type, checking along the way.
    fn infer(&mut self, expr: &Expr, errors: &mut Vec<TypeError>) -> Type {
        match expr {
            Expr::Num(_) => Type::Num,
            Expr::Str(_) => Type::Str,
            Expr::Bool(_) => Type::Bool,
            Expr::Null | Expr::Undefined => Type::Any,
            Expr::Ident(name) => self.lookup(name).unwrap_or(Type::Any),
            Expr::Array(items) => {
                // The element type: the first inferable element, `any`
                // when the array is empty or mixed.
                let mut elem: Option<Type> = None;
                for item in items {
                    let ty = self.infer(item, errors);
                    match &elem {
                        None => elem = Some(ty),
                        Some(prev) if *prev == ty => {}
                        Some(_) => elem = Some(Type::Any),
                    }
                }
                Type::Arr(Box::new(elem.unwrap_or(Type::Any)))
            }
            Expr::Obj(_) => Type::Any,
            Expr::Unary(op, e) => match op {
                crate::ast::UnaryOp::Not => Type::Bool,
                crate::ast::UnaryOp::Neg => self.infer(e, errors),
                crate::ast::UnaryOp::Typeof => Type::Str,
            },
            Expr::Binary(crate::ast::BinOp::Add, l, r) => {
                let lt = self.infer(l, errors);
                let rt = self.infer(r, errors);
                if lt == Type::Str || rt == Type::Str {
                    Type::Str
                } else {
                    Type::Num
                }
            }
            Expr::Binary(_, l, r) => {
                self.infer(l, errors);
                self.infer(r, errors);
                Type::Num
            }
            Expr::Eq(..) => Type::Bool,
            Expr::Logical(_, l, r) => {
                let lt = self.infer(l, errors);
                let rt = self.infer(r, errors);
                if lt == rt {
                    lt
                } else {
                    Type::Any
                }
            }
            Expr::Ternary(c, t, e) => {
                self.infer(c, errors);
                let tt = self.infer(t, errors);
                let et = self.infer(e, errors);
                if tt == et {
                    tt
                } else {
                    Type::Any
                }
            }
            Expr::Assign(target, value) => {
                let vt = self.infer(value, errors);
                match target {
                    crate::ast::Target::Ident(name) => {
                        if let Some(err) = self.assign(name, &vt) {
                            errors.push(err);
                        }
                        vt
                    }
                    crate::ast::Target::Index(obj, _) => {
                        if let Type::Arr(elem) = self.infer(obj, errors) {
                            if !compatible(&elem, &vt) {
                                errors.push(TypeError {
                                    subject: "index assignment".into(),
                                    message: format!(
                                        "cannot store {} in a {} array",
                                        vt.name(),
                                        elem.name()
                                    ),
                                });
                            }
                        }
                        vt
                    }
                    crate::ast::Target::Member(..) => vt,
                }
            }
            Expr::Index(obj, idx) => {
                let ot = self.infer(obj, errors);
                self.infer(idx, errors);
                match ot {
                    Type::Arr(elem) => (*elem).clone(),
                    _ => Type::Any,
                }
            }
            Expr::Member(obj, prop) => {
                let ot = self.infer(obj, errors);
                if prop == "length" {
                    Type::Num
                } else {
                    let _ = ot;
                    Type::Any
                }
            }
            Expr::Call(callee, args) => {
                // Infer argument types first (checking their innards),
                // remember them, then check against the signature.
                let mut arg_types = Vec::new();
                for a in args {
                    arg_types.push(self.infer(a, errors));
                }
                match &**callee {
                    Expr::Ident(name) => match self.signatures.get(name) {
                        Some((params, ret)) => {
                            if args.len() != params.len() {
                                errors.push(TypeError {
                                    subject: name.clone(),
                                    message: format!(
                                        "called with {} arguments, expects {}",
                                        args.len(),
                                        params.len()
                                    ),
                                });
                            } else {
                                for (i, (pt, at)) in params.iter().zip(arg_types.iter()).enumerate()
                                {
                                    if !compatible(pt, at) {
                                        errors.push(TypeError {
                                            subject: name.clone(),
                                            message: format!(
                                                "argument {} is {}, expects {}",
                                                i + 1,
                                                at.name(),
                                                pt.name()
                                            ),
                                        });
                                    }
                                }
                            }
                            ret.clone()
                        }
                        None => Type::Any,
                    },
                    _ => Type::Any,
                }
            }
            Expr::Update(_, _, target) => match target {
                crate::ast::Target::Ident(name) => self.lookup(name).unwrap_or(Type::Any),
                _ => Type::Num,
            },
            Expr::Arrow(..) | Expr::Fn(..) => Type::Any,
        }
    }
}
