//! The tree-walking interpreter.
//!
//! Runs the items a [`crate::passes`] regionization produced: functions
//! register first (declaration hoisting), then top-level statements run
//! in source order. Closures work through the environment chain on
//! [`value::Env`]; array methods dispatch with their receiver.
//!
//! Documented divergences from JavaScript:
//! * No exceptions (`try`/`catch`): runtime errors abort the program.
//! * `t op= v` evaluates the target twice.
//! * Object literals support `key: value` and shorthand `key`, but not
//!   method shorthand, computed keys, or getters; there is no `delete`.
//! * `in` exists only in `for..in`, not as an operator.
//! * `console.log` inspects containers like Node, but prints very deep
//!   structures fully instead of truncating to `[Array]` / `[Object]`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::*;
use crate::value::{Env, Func, SetError, Value};

/// A runtime failure.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpError {
    pub message: String,
}

impl From<String> for InterpError {
    fn from(message: String) -> Self {
        InterpError { message }
    }
}

fn bail(msg: impl Into<String>) -> InterpError {
    InterpError {
        message: msg.into(),
    }
}

/// Control flow through statement evaluation.
enum Ctl {
    /// Produced a value that statement context discards.
    Val,
    Return(Value),
    Break,
    Continue,
}

/// The interpreter. `console.log` writes to `out` so tests capture it
/// and the Node differential compares it byte for byte.
pub struct Interp<'o> {
    out: &'o mut dyn std::io::Write,
    /// Arena storage for `@own` values. Arena 0 is the global
    /// activation; each call pushes and pops one.
    arenas: crate::mem::Arenas,
}

/// A fully parsed program: top-level items in source order.
pub enum Item {
    Fn(FnDef),
    Stmt(Stmt),
}

impl<'o> Interp<'o> {
    pub fn new(out: &'o mut dyn std::io::Write) -> Self {
        Self {
            out,
            arenas: crate::mem::new_store(),
        }
    }

    /// How many activation arenas currently exist: 1 means every call's
    /// arena has been dropped (the global arena remains).
    pub fn arena_count(&self) -> usize {
        self.arenas.borrow().len()
    }

    /// Runs a whole program.
    pub fn run(&mut self, items: &[Item]) -> Result<(), InterpError> {
        let global = Env::function_scope(None);
        // The non-writable numeric globals JavaScript always provides.
        global.declare("NaN", Value::Num(f64::NAN), true);
        global.declare("Infinity", Value::Num(f64::INFINITY), true);
        for item in items {
            if let Item::Fn(def) = item {
                global.declare(
                    &def.name,
                    self.closure(
                        Some(def.name.clone()),
                        def.params.clone(),
                        &def.body,
                        &global,
                    ),
                    false,
                );
            }
        }
        for item in items {
            if let Item::Stmt(stmt) = item {
                hoist_vars(&global, stmt);
            }
        }
        for item in items {
            if let Item::Stmt(stmt) = item {
                self.exec_stmt(&global, stmt)?;
            }
        }
        Ok(())
    }

    fn closure(
        &self,
        name: Option<String>,
        params: Vec<String>,
        body: &[Stmt],
        env: &Rc<Env>,
    ) -> Value {
        Value::Func(Rc::new(Func::Closure {
            name,
            params,
            body: FnBody::Block(body.to_vec()),
            env: env.clone(),
        }))
    }

    // ---- statements ----

    fn exec_block(&mut self, env: &Rc<Env>, stmts: &[Stmt]) -> Result<Ctl, InterpError> {
        let block_env = Env::new(Some(env.clone()));
        self.hoist_fns(&block_env, stmts);
        for stmt in stmts {
            match self.exec_stmt(&block_env, stmt)? {
                Ctl::Val => {}
                ctl => return Ok(ctl),
            }
        }
        Ok(Ctl::Val)
    }

    /// Registers `function` declarations in this block before the
    /// statements run (block-level hoisting).
    fn hoist_fns(&self, env: &Rc<Env>, stmts: &[Stmt]) {
        for stmt in stmts {
            if let Stmt::FnDecl(name, params, body) = stmt {
                env.declare(
                    name,
                    self.closure(Some(name.clone()), params.clone(), body, env),
                    false,
                );
            }
        }
    }

    fn exec_stmt(&mut self, env: &Rc<Env>, stmt: &Stmt) -> Result<Ctl, InterpError> {
        match stmt {
            Stmt::Empty | Stmt::FnDecl(..) => Ok(Ctl::Val),
            Stmt::Let {
                is_const,
                decls,
                mem,
            } => {
                for (name, init) in decls {
                    let value = self.eval_decl(env, *mem, init, name)?;
                    env.declare(name, value, *is_const);
                }
                Ok(Ctl::Val)
            }
            Stmt::Var { decls, mem } => {
                for (name, init) in decls {
                    let value = self.eval_decl(env, *mem, init, name)?;
                    set_var(env, name, value);
                }
                Ok(Ctl::Val)
            }
            Stmt::Expr(expr) => {
                self.eval(env, expr)?;
                Ok(Ctl::Val)
            }
            Stmt::If(cond, then, els) => {
                if self.eval(env, cond)?.is_truthy() {
                    self.exec_stmt(env, then)
                } else if let Some(els) = els {
                    self.exec_stmt(env, els)
                } else {
                    Ok(Ctl::Val)
                }
            }
            Stmt::While(cond, body) => {
                while self.eval(env, cond)?.is_truthy() {
                    match self.exec_stmt(env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                }
                Ok(Ctl::Val)
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                let loop_env = Env::new(Some(env.clone()));
                if let Some(init) = init {
                    self.exec_stmt(&loop_env, init)?;
                }
                // `let` loop variables get a fresh binding per iteration
                // — closures in the body capture this iteration's value,
                // as JavaScript specifies. `var` bindings live in the
                // function frame and are shared.
                let let_names: Vec<String> = match init.as_deref() {
                    Some(Stmt::Let { decls, .. }) => {
                        decls.iter().map(|(name, _)| name.clone()).collect()
                    }
                    _ => Vec::new(),
                };
                loop {
                    if let Some(cond) = cond {
                        if !self.eval(&loop_env, cond)?.is_truthy() {
                            break;
                        }
                    }
                    let iter_env = Env::new(Some(
                        loop_env.parent.clone().unwrap_or_else(|| loop_env.clone()),
                    ));
                    for name in &let_names {
                        if let Some(v) = loop_env.get(name) {
                            iter_env.declare(name, v, false);
                        }
                    }
                    match self.exec_stmt(&iter_env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                    // The body may have mutated the iteration's binding;
                    // sync it back before the step runs.
                    for name in &let_names {
                        if let Some(v) = iter_env.get(name) {
                            let _ = loop_env.set(name, v);
                        }
                    }
                    if let Some(step) = step {
                        self.eval(&loop_env, step)?;
                    }
                }
                Ok(Ctl::Val)
            }
            Stmt::ForOf {
                name,
                is_const,
                iterable,
                body,
            } => {
                let iter_value = self.eval(env, iterable)?;
                let items: Vec<Value> = match iter_value {
                    Value::Arr(arr) => arr.borrow().clone(),
                    Value::Own(handle) => crate::mem::read_arr(&self.arenas, &handle)?,
                    Value::Str(s) => s
                        .chars()
                        .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                        .collect(),
                    other => return Err(bail(format!("{} is not iterable", other.to_display()))),
                };
                for item in items {
                    let iter_env = Env::new(Some(env.clone()));
                    iter_env.declare(name, item, *is_const);
                    match self.exec_stmt(&iter_env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                }
                Ok(Ctl::Val)
            }
            Stmt::ForIn {
                name,
                is_const,
                iterable,
                body,
            } => {
                let target = self.eval(env, iterable)?;
                let keys: Vec<Value> = match &target {
                    Value::Own(handle) => crate::mem::read_obj(&self.arenas, handle)?
                        .into_iter()
                        .map(|(k, _)| Value::Str(Rc::from(k.as_str())))
                        .collect(),
                    Value::Obj(obj) => obj
                        .borrow()
                        .iter()
                        .map(|(k, _)| Value::Str(Rc::from(k.as_str())))
                        .collect(),
                    Value::Arr(arr) => (0..arr.borrow().len())
                        .map(|i| Value::Str(Rc::from(crate::value::fmt_number(i as f64).as_str())))
                        .collect(),
                    Value::Str(s) => (0..s.chars().count())
                        .map(|i| Value::Str(Rc::from(crate::value::fmt_number(i as f64).as_str())))
                        .collect(),
                    // JavaScript: for-in over other types iterates
                    // nothing, without error.
                    _ => Vec::new(),
                };
                for key in keys {
                    let iter_env = Env::new(Some(env.clone()));
                    iter_env.declare(name, key, *is_const);
                    match self.exec_stmt(&iter_env, body)? {
                        Ctl::Break => break,
                        Ctl::Return(v) => return Ok(Ctl::Return(v)),
                        _ => {}
                    }
                }
                Ok(Ctl::Val)
            }
            Stmt::Block(stmts) => self.exec_block(env, stmts),
            Stmt::Return(expr) => {
                let value = match expr {
                    Some(e) => self.eval(env, e)?,
                    None => Value::Undefined,
                };
                if matches!(value, Value::Own(_)) {
                    return Err(bail(
                        "an @own value cannot escape its activation — return a copy or restructure",
                    ));
                }
                Ok(Ctl::Return(value))
            }
            Stmt::Break => Ok(Ctl::Break),
            Stmt::Continue => Ok(Ctl::Continue),
        }
    }

    // ---- expressions ----

    fn eval(&mut self, env: &Rc<Env>, expr: &Expr) -> Result<Value, InterpError> {
        match expr {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Str(s) => Ok(Value::Str(Rc::from(s.as_str()))),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Ident(name) => {
                let value = env
                    .get(name)
                    .ok_or_else(|| bail(format!("{name} is not defined")))?;
                if matches!(value, Value::Moved) {
                    return Err(bail(format!("use after move of `{name}`")));
                }
                Ok(value)
            }
            Expr::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval(env, item)?);
                }
                Ok(Value::Arr(Rc::new(RefCell::new(out))))
            }
            Expr::Obj(entries) => {
                let mut map: Vec<(String, Value)> = Vec::with_capacity(entries.len());
                for entry in entries {
                    let value = self.eval(env, &entry.value)?;
                    match map.iter_mut().find(|(k, _)| *k == entry.key) {
                        Some(slot) => slot.1 = value,
                        None => map.push((entry.key.clone(), value)),
                    }
                }
                Ok(Value::Obj(Rc::new(RefCell::new(map))))
            }
            Expr::Unary(op, e) => {
                let v = self.eval(env, e)?;
                Ok(match op {
                    UnaryOp::Not => Value::Bool(!v.is_truthy()),
                    UnaryOp::Neg => Value::Num(-to_num(&v)),
                    UnaryOp::Typeof => match v {
                        Value::Num(_) => Value::Str("number".into()),
                        Value::Str(_) => Value::Str("string".into()),
                        Value::Bool(_) => Value::Str("boolean".into()),
                        Value::Undefined => Value::Str("undefined".into()),
                        Value::Null | Value::Arr(_) | Value::Obj(_) | Value::Own(_) => {
                            Value::Str("object".into())
                        }
                        Value::Moved => Value::Str("undefined".into()),
                        Value::Func(_) => Value::Str("function".into()),
                    },
                })
            }
            Expr::Binary(op, l, r) => {
                let lv = self.eval(env, l)?;
                let rv = self.eval(env, r)?;
                if matches!(lv, Value::Own(_)) || matches!(rv, Value::Own(_)) {
                    return Err(bail(
                        "@own values cannot take part in arithmetic — read their parts instead",
                    ));
                }
                Ok(binary(*op, lv, rv))
            }
            Expr::Logical(op, l, r) => {
                let lv = self.eval(env, l)?;
                let truthy = lv.is_truthy();
                match op {
                    LogicalOp::And if truthy => self.eval(env, r),
                    LogicalOp::And => Ok(lv),
                    LogicalOp::Or if truthy => Ok(lv),
                    LogicalOp::Or => self.eval(env, r),
                }
            }
            Expr::Eq(op, l, r) => {
                let lv = self.eval(env, l)?;
                let rv = self.eval(env, r)?;
                Ok(Value::Bool(match op {
                    EqOp::Strict => lv.strict_eq(&rv),
                    EqOp::Loose => lv.loose_eq(&rv),
                }))
            }
            Expr::Ternary(cond, then, els) => {
                if self.eval(env, cond)?.is_truthy() {
                    self.eval(env, then)
                } else {
                    self.eval(env, els)
                }
            }
            Expr::Assign(target, value) => {
                let v = self.eval(env, value)?;
                self.assign(env, target, v.clone(), Some(value))?;
                Ok(v)
            }
            Expr::Index(obj, index) => {
                let o = self.eval(env, obj)?;
                let i = self.eval(env, index)?;
                Ok(index_get(&self.arenas, &o, &i)?)
            }
            Expr::Member(obj, prop) => {
                let o = self.eval(env, obj)?;
                Ok(member_get(&self.arenas, &o, prop)?)
            }
            Expr::Call(callee, args) => {
                // console.log(...) — the interpreter's output channel.
                if let Expr::Member(obj_expr, prop) = &**callee {
                    if matches!(**obj_expr, Expr::Ident(ref n) if n == "console") && prop == "log" {
                        let mut line = String::new();
                        for (i, a) in args.iter().enumerate() {
                            if i > 0 {
                                line.push(' ');
                            }
                            let v = self.eval(env, a)?;
                            // Containers print Node-style; top-level
                            // scalars print raw.
                            line.push_str(&match &v {
                                Value::Own(handle) => self.inspect_own(handle)?,
                                Value::Arr(_) | Value::Obj(_) | Value::Func(_) => v.inspect(),
                                other => other.to_display(),
                            });
                        }
                        writeln!(self.out, "{line}")
                            .map_err(|e| bail(format!("write failed: {e}")))?;
                        return Ok(Value::Undefined);
                    }
                    // Method call with a receiver: arr.push(x), arr.map(f)...
                    let recv = self.eval(env, obj_expr)?;
                    let mut argv = Vec::with_capacity(args.len());
                    for a in args {
                        argv.push(self.eval(env, a)?);
                    }
                    return self.call_method(&recv, prop, argv);
                }
                let f = self.eval(env, callee)?;
                let mut argv = Vec::with_capacity(args.len());
                for a in args {
                    argv.push(self.eval(env, a)?);
                }
                self.call_value(&f, argv)
            }
            Expr::Arrow(params, body) => Ok(Value::Func(Rc::new(Func::Closure {
                name: None,
                params: params.clone(),
                body: (**body).clone(),
                env: env.clone(),
            }))),
            Expr::Fn(name, params, body) => Ok(Value::Func(Rc::new(Func::Closure {
                name: name.clone(),
                params: params.clone(),
                body: FnBody::Block(body.clone()),
                env: env.clone(),
            }))),
            Expr::Update(op, prefix, target) => {
                let old = self.read_target(env, target)?;
                let new = Value::Num(match op {
                    UpdateOp::Inc => to_num(&old) + 1.0,
                    UpdateOp::Dec => to_num(&old) - 1.0,
                });
                self.assign(env, target, new.clone(), None)?;
                Ok(if *prefix { new } else { old })
            }
        }
    }

    fn read_target(&mut self, env: &Rc<Env>, target: &Target) -> Result<Value, InterpError> {
        match target {
            Target::Ident(name) => env
                .get(name)
                .ok_or_else(|| bail(format!("{name} is not defined"))),
            Target::Index(obj, idx) => {
                let o = self.eval(env, obj)?;
                let i = self.eval(env, idx)?;
                Ok(index_get(&self.arenas, &o, &i)?)
            }
            Target::Member(obj, prop) => {
                let o = self.eval(env, obj)?;
                Ok(member_get(&self.arenas, &o, prop)?)
            }
        }
    }

    /// Evaluates a declaration's initializer under a memory mode.
    fn eval_decl(
        &mut self,
        env: &Rc<Env>,
        mem: Mem,
        init: &Option<Expr>,
        name: &str,
    ) -> Result<Value, InterpError> {
        let raw = match init {
            Some(e) => self.eval(env, e)?,
            None => Value::Undefined,
        };
        match mem {
            Mem::Gc => match raw {
                Value::Own(_) => Err(bail(format!(
                    "cannot assign an @own value to unannotated `{name}` — declare it with // @own or // @ref"
                ))),
                other => Ok(other),
            },
            Mem::Own => match raw {
                Value::Own(handle) => {
                    if handle.readonly {
                        return Err(bail("cannot move through @ref"));
                    }
                    // Move: the source binding becomes unusable.
                    if let Some(Expr::Ident(source)) = init {
                        let _ = env.set(source, Value::Moved);
                    }
                    Ok(Value::Own(handle))
                }
                fresh @ (Value::Arr(_) | Value::Obj(_)) => {
                    Ok(Value::Own(crate::mem::take_gc(&self.arenas, &fresh)?))
                }
                other => Err(bail(format!(
                    "@own applies to arrays and objects, not {}",
                    other.to_display()
                ))),
            },
            Mem::Ref => match raw {
                Value::Own(handle) => {
                    Ok(Value::Own(crate::mem::OwnHandle { readonly: true, ..handle }))
                }
                other => Err(bail(format!(
                    "@ref requires an @own value, not {}",
                    other.to_display()
                ))),
            },
        }
    }

    fn assign(
        &mut self,
        env: &Rc<Env>,
        target: &Target,
        value: Value,
        init: Option<&Expr>,
    ) -> Result<(), InterpError> {
        match target {
            Target::Ident(name) => {
                // Moving an @own value between bindings.
                if matches!(value, Value::Own(_)) {
                    let current = env.get(name);
                    if !matches!(current, Some(Value::Own(_)) | None) {
                        return Err(bail(format!(
                            "cannot store an @own value in unannotated `{name}`"
                        )));
                    }
                    if let Some(Expr::Ident(source)) = init {
                        let _ = env.set(source, Value::Moved);
                    }
                }
                match env.set(name, value) {
                    Ok(()) => Ok(()),
                    Err(SetError::Const) => {
                        Err(bail(format!("assignment to constant variable `{name}`")))
                    }
                    Err(SetError::NotFound) => Err(bail(format!("{name} is not defined"))),
                }
            }
            Target::Index(obj, idx) => {
                let o = self.eval(env, obj)?;
                if matches!(value, Value::Own(_)) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                let i = self.eval(env, idx)?;
                index_set(&self.arenas, &o, &i, value)?;
                Ok(())
            }
            Target::Member(obj, prop) => {
                let o = self.eval(env, obj)?;
                if matches!(value, Value::Own(_)) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                member_set(&self.arenas, &o, prop, value)?;
                Ok(())
            }
        }
    }

    fn call_method(
        &mut self,
        recv: &Value,
        prop: &str,
        argv: Vec<Value>,
    ) -> Result<Value, InterpError> {
        match (recv, prop) {
            // @own receivers mutate arena storage in place; borrows are
            // rejected by the mem helpers.
            (Value::Own(handle), "push") => {
                // Depth-one ownership: no @own values inside @own values.
                if argv.iter().any(|v| matches!(v, Value::Own(_))) {
                    return Err(bail("@own values cannot be nested (depth-one ownership)"));
                }
                crate::mem::with_arr_mut(&self.arenas, handle, |items| items.extend(argv))?;
                let len = crate::mem::read_arr(&self.arenas, handle)?.len();
                Ok(Value::Num(len as f64))
            }
            (Value::Own(handle), "pop") => {
                let mut popped = Value::Undefined;
                crate::mem::with_arr_mut(&self.arenas, handle, |items| {
                    popped = items.pop().unwrap_or(Value::Undefined);
                })?;
                Ok(popped)
            }
            (Value::Own(handle), "map" | "filter") => {
                let snapshot = crate::mem::read_arr(&self.arenas, handle)?;
                self.map_or_filter(&snapshot, prop, argv)
            }
            (Value::Arr(arr), "push") => {
                if argv.iter().any(|v| matches!(v, Value::Own(_))) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                let mut arr = arr.borrow_mut();
                arr.extend(argv);
                Ok(Value::Num(arr.len() as f64))
            }
            (Value::Arr(arr), "pop") => {
                let mut arr = arr.borrow_mut();
                Ok(arr.pop().unwrap_or(Value::Undefined))
            }
            (Value::Arr(arr), "map") => {
                let f = argv
                    .first()
                    .cloned()
                    .ok_or_else(|| bail("map expects a callback"))?;
                let snapshot = arr.borrow().clone();
                self.map_or_filter(&snapshot, "map", vec![f])
            }
            (Value::Arr(arr), "filter") => {
                let f = argv
                    .first()
                    .cloned()
                    .ok_or_else(|| bail("filter expects a callback"))?;
                let snapshot = arr.borrow().clone();
                self.map_or_filter(&snapshot, "filter", vec![f])
            }
            (recv, _) => Err(bail(format!(
                "{}.{} is not a function",
                recv.to_display(),
                prop
            ))),
        }
    }

    /// Shared body of `map`/`filter` over a snapshot of elements.
    fn map_or_filter(
        &mut self,
        snapshot: &[Value],
        prop: &str,
        argv: Vec<Value>,
    ) -> Result<Value, InterpError> {
        let f = argv
            .first()
            .cloned()
            .ok_or_else(|| bail(format!("{prop} expects a callback")))?;
        if prop == "map" {
            let mut out = Vec::with_capacity(snapshot.len());
            for (i, item) in snapshot.iter().enumerate() {
                out.push(self.call_value(&f, vec![item.clone(), Value::Num(i as f64)])?);
            }
            Ok(Value::Arr(Rc::new(RefCell::new(out))))
        } else {
            let mut out = Vec::new();
            for (i, item) in snapshot.iter().enumerate() {
                let keep = self.call_value(&f, vec![item.clone(), Value::Num(i as f64)])?;
                if keep.is_truthy() {
                    out.push(item.clone());
                }
            }
            Ok(Value::Arr(Rc::new(RefCell::new(out))))
        }
    }

    /// Node-style inspection of an @own value's contents: the same
    /// format its garbage-collected twin would print.
    fn inspect_own(&self, handle: &crate::mem::OwnHandle) -> Result<String, InterpError> {
        let as_gc = match crate::mem::read_arr(&self.arenas, handle) {
            Ok(items) => Value::Arr(Rc::new(RefCell::new(items))),
            Err(_) => Value::Obj(Rc::new(RefCell::new(crate::mem::read_obj(
                &self.arenas,
                handle,
            )?))),
        };
        Ok(as_gc.inspect())
    }

    /// Calls a function value: closures push an environment holding the
    /// parameters; blocks hoist their own function declarations.
    fn call_value(&mut self, f: &Value, argv: Vec<Value>) -> Result<Value, InterpError> {
        let func = match f {
            Value::Func(f) => f.clone(),
            other => return Err(bail(format!("{} is not a function", other.to_display()))),
        };
        match &*func {
            Func::Native { f, .. } => f(&argv).map_err(|message| InterpError { message }),
            Func::Closure { env: def_env, .. } => {
                crate::mem::push(&self.arenas);
                let result = self.call_closure(&func, argv, def_env);
                crate::mem::pop(&self.arenas);
                result
            }
        }
    }

    /// The body of a closure call, with its activation arena already
    /// pushed.
    fn call_closure(
        &mut self,
        func: &Rc<Func>,
        argv: Vec<Value>,
        def_env: &Rc<Env>,
    ) -> Result<Value, InterpError> {
        match &**func {
            Func::Native { f, .. } => f(&argv).map_err(|message| InterpError { message }),
            Func::Closure { params, body, .. } => {
                let call_env = Env::function_scope(Some(def_env.clone()));
                for (i, p) in params.iter().enumerate() {
                    // Arguments borrow @own values: read-only for the
                    // call, so writes through parameters are rejected.
                    let arg = argv.get(i).cloned().unwrap_or(Value::Undefined);
                    let arg = match arg {
                        Value::Own(handle) => Value::Own(crate::mem::OwnHandle {
                            readonly: true,
                            ..handle
                        }),
                        other => other,
                    };
                    call_env.declare(p, arg, false);
                }
                match body {
                    FnBody::Expr(expr) => self.eval(&call_env, expr),
                    FnBody::Block(stmts) => {
                        self.hoist_fns(&call_env, stmts);
                        for stmt in stmts {
                            hoist_vars(&call_env, stmt);
                        }
                        for stmt in stmts {
                            match self.exec_stmt(&call_env, stmt)? {
                                Ctl::Val => {}
                                Ctl::Return(v) => return Ok(v),
                                Ctl::Break => return Err(bail("illegal break")),
                                Ctl::Continue => return Err(bail("illegal continue")),
                            }
                        }
                        Ok(Value::Undefined)
                    }
                }
            }
        }
    }
}

/// Registers `var` declarations in the nearest function frame before
/// execution reaches them (hoisting): scans `stmt` recursively through
/// control-flow structures, but never into nested functions.
fn hoist_vars(frame: &Rc<Env>, stmt: &Stmt) {
    match stmt {
        Stmt::Var { decls, .. } => {
            for (name, _) in decls {
                if !frame.declares_locally(name) {
                    frame.declare(name, Value::Undefined, false);
                }
            }
        }
        Stmt::Block(stmts) => {
            for stmt in stmts {
                hoist_vars(frame, stmt);
            }
        }
        Stmt::If(_, then, els) => {
            hoist_vars(frame, then);
            if let Some(els) = els {
                hoist_vars(frame, els);
            }
        }
        Stmt::While(_, body) => hoist_vars(frame, body),
        Stmt::For { init, body, .. } => {
            if let Some(init) = init {
                hoist_vars(frame, init);
            }
            hoist_vars(frame, body);
        }
        Stmt::ForOf { body, .. } | Stmt::ForIn { body, .. } => hoist_vars(frame, body),
        _ => {}
    }
}

/// Assigns to a `var`: walks out to the nearest function-scope frame and
/// updates (or creates) the binding there.
fn set_var(env: &Rc<Env>, name: &str, value: Value) {
    if env.is_function_scope {
        if env.declares_locally(name) {
            let _ = env.set(name, value);
        } else {
            env.declare(name, value, false);
        }
        return;
    }
    match &env.parent {
        Some(parent) => set_var(parent, name, value),
        None => env.declare(name, value, false),
    }
}

fn to_num(v: &Value) -> f64 {
    match v.to_number_value() {
        Value::Num(n) => n,
        _ => f64::NAN,
    }
}

fn binary(op: BinOp, lv: Value, rv: Value) -> Value {
    match op {
        BinOp::Add => {
            if matches!(lv, Value::Str(_)) || matches!(rv, Value::Str(_)) {
                Value::Str(Rc::from(
                    format!("{}{}", lv.to_display(), rv.to_display()).as_str(),
                ))
            } else {
                Value::Num(to_num(&lv) + to_num(&rv))
            }
        }
        BinOp::Sub => Value::Num(to_num(&lv) - to_num(&rv)),
        BinOp::Mul => Value::Num(to_num(&lv) * to_num(&rv)),
        BinOp::Div => Value::Num(to_num(&lv) / to_num(&rv)),
        BinOp::Rem => Value::Num(to_num(&lv) % to_num(&rv)),
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            if let (Value::Str(a), Value::Str(b)) = (&lv, &rv) {
                let ord = match op {
                    BinOp::Lt => a < b,
                    BinOp::Gt => a > b,
                    BinOp::Le => a <= b,
                    BinOp::Ge => a >= b,
                    _ => unreachable!(),
                };
                return Value::Bool(ord);
            }
            let (a, b) = (to_num(&lv), to_num(&rv));
            if a.is_nan() || b.is_nan() {
                return Value::Bool(false);
            }
            Value::Bool(match op {
                BinOp::Lt => a < b,
                BinOp::Gt => a > b,
                BinOp::Le => a <= b,
                _ => a >= b,
            })
        }
    }
}

/// The string key for an object index, per JavaScript coercion:
/// `obj[1]` is `obj["1"]`.
fn obj_key(index: &Value) -> Option<String> {
    match index {
        Value::Str(s) => Some(s.to_string()),
        Value::Num(n) => Some(crate::value::fmt_number(*n)),
        _ => None,
    }
}

fn index_get(
    arenas: &crate::mem::Arenas,
    obj: &Value,
    index: &Value,
) -> Result<Value, InterpError> {
    if let Value::Own(handle) = obj {
        // `.length` works on own arrays and objects, like their
        // garbage-collected twins.
        if matches!(index, Value::Str(s) if s.as_ref() == "length") {
            if let Ok(items) = crate::mem::read_arr(arenas, handle) {
                return Ok(Value::Num(items.len() as f64));
            }
            if let Ok(entries) = crate::mem::read_obj(arenas, handle) {
                return Ok(Value::Num(entries.len() as f64));
            }
        }
        let key = obj_key(index).ok_or_else(|| bail("bad @own index"))?;
        if let Ok(entries) = crate::mem::read_obj(arenas, handle) {
            return Ok(entries
                .into_iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v)
                .unwrap_or(Value::Undefined));
        }
        let items = crate::mem::read_arr(arenas, handle)?;
        let idx: Option<usize> = match key.parse::<f64>() {
            Ok(n) if n.fract() == 0.0 && n >= 0.0 => Some(n as usize),
            _ => None,
        };
        return Ok(idx
            .and_then(|i| items.get(i).cloned())
            .unwrap_or(Value::Undefined));
    }
    if let (Value::Obj(obj), Some(key)) = (obj, obj_key(index)) {
        return Ok(obj
            .borrow()
            .iter()
            .find(|(k, _)| k == &key)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Undefined));
    }
    Ok(match (obj, index) {
        (Value::Arr(arr), i) => match i {
            Value::Num(n) if n.fract() == 0.0 && *n >= 0.0 => arr
                .borrow()
                .get(*n as usize)
                .cloned()
                .unwrap_or(Value::Undefined),
            Value::Str(s) if s.as_ref() == "length" => Value::Num(arr.borrow().len() as f64),
            Value::Str(s) => match s.parse::<f64>() {
                Ok(n) if n.fract() == 0.0 && n >= 0.0 => arr
                    .borrow()
                    .get(n as usize)
                    .cloned()
                    .unwrap_or(Value::Undefined),
                _ => Value::Undefined,
            },
            _ => Value::Undefined,
        },
        (Value::Str(s), Value::Num(n)) if n.fract() == 0.0 && *n >= 0.0 => s
            .chars()
            .nth(*n as usize)
            .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
            .unwrap_or(Value::Undefined),
        (Value::Str(s), Value::Str(prop)) if prop.as_ref() == "length" => {
            Value::Num(s.chars().count() as f64)
        }
        _ => Value::Undefined,
    })
}

fn index_set(
    arenas: &crate::mem::Arenas,
    obj: &Value,
    index: &Value,
    value: Value,
) -> Result<(), InterpError> {
    if let Value::Own(handle) = obj {
        let key = obj_key(index).ok_or_else(|| bail("bad @own index"))?;
        if crate::mem::read_obj(arenas, handle).is_ok() {
            return crate::mem::with_obj_mut(arenas, handle, |entries| {
                match entries.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = value,
                    None => entries.push((key, value)),
                }
            })
            .map_err(InterpError::from);
        }
        return crate::mem::with_arr_mut(arenas, handle, |items| {
            let idx = key
                .parse::<f64>()
                .ok()
                .filter(|n| n.fract() == 0.0 && *n >= 0.0);
            if let Some(i) = idx.map(|n| n as usize) {
                if i >= items.len() {
                    items.resize(i + 1, Value::Undefined);
                }
                items[i] = value;
            }
        })
        .map_err(InterpError::from);
    }
    if let (Value::Obj(obj), Some(key)) = (obj, obj_key(index)) {
        let mut obj = obj.borrow_mut();
        match obj.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => obj.push((key, value)),
        }
        return Ok(());
    }
    if let (Value::Arr(arr), Value::Num(n)) = (obj, index) {
        if n.fract() == 0.0 && *n >= 0.0 {
            let mut arr = arr.borrow_mut();
            let idx = *n as usize;
            if idx >= arr.len() {
                arr.resize(idx + 1, Value::Undefined);
            }
            arr[idx] = value;
        }
    }
    Ok(())
}

fn member_get(arenas: &crate::mem::Arenas, obj: &Value, prop: &str) -> Result<Value, InterpError> {
    index_get(arenas, obj, &Value::Str(Rc::from(prop)))
}

fn member_set(
    arenas: &crate::mem::Arenas,
    obj: &Value,
    prop: &str,
    value: Value,
) -> Result<(), InterpError> {
    if let (Value::Arr(arr), "length") = (obj, prop) {
        if let Value::Num(n) = value {
            if n >= 0.0 {
                arr.borrow_mut().truncate(n as usize);
            }
            return Ok(());
        }
    }
    index_set(arenas, obj, &Value::Str(Rc::from(prop)), value)
}
