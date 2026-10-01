//! memjs runtime values: JavaScript semantics in miniature.
//!
//! Everything heap-shaped lives behind `Rc` — the "loose mode" of the
//! gradual-memory story. The M3 `@own`/`@ref` annotations will route
//! annotated values into per-activation arenas instead.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

/// The shape of a native function: arguments in, value or message out.
pub type NativeFn = dyn Fn(&[Value]) -> Result<Value, String>;

/// A user-defined or native function value.
pub enum Func {
    /// A closure over its defining environment.
    Closure {
        name: Option<String>,
        params: Vec<String>,
        body: crate::ast::FnBody,
        env: Rc<Env>,
    },
    Native {
        name: &'static str,
        f: Box<NativeFn>,
    },
}

impl std::fmt::Debug for Func {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Func::Closure { name, .. } => write!(f, "Closure({name:?})"),
            Func::Native { name, .. } => write!(f, "Native({name})"),
        }
    }
}

/// A lexical environment: a mutable frame plus the parent chain.
#[derive(Debug)]
pub struct Env {
    pub vars: RefCell<Vec<(String, Value, bool)>>, // name, value, is_const
    pub parent: Option<Rc<Env>>,
    /// Function-scope frames (the global frame and call frames) are
    /// where `var` declarations land.
    pub is_function_scope: bool,
}

impl Env {
    /// A function-scope frame: the target for `var` declarations.
    pub fn function_scope(parent: Option<Rc<Env>>) -> Rc<Env> {
        Rc::new(Env {
            vars: RefCell::new(Vec::new()),
            parent,
            is_function_scope: true,
        })
    }

    /// Whether this frame itself declares `name`.
    pub fn declares_locally(&self, name: &str) -> bool {
        self.vars.borrow().iter().any(|(n, _, _)| n == name)
    }

    pub fn new(parent: Option<Rc<Env>>) -> Rc<Env> {
        Rc::new(Env {
            vars: RefCell::new(Vec::new()),
            parent,
            is_function_scope: false,
        })
    }

    /// Declares a variable in this frame. Re-declaration shadows.
    pub fn declare(&self, name: &str, value: Value, is_const: bool) {
        self.vars
            .borrow_mut()
            .push((name.to_string(), value, is_const));
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        for (n, v, _) in self.vars.borrow().iter().rev() {
            if n == name {
                return Some(v.clone());
            }
        }
        self.parent.as_ref().and_then(|p| p.lookup(name))
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        self.lookup(name)
    }

    /// Assigns to the nearest binding of `name` in the chain. Returns
    /// `Err(())` if the name is not found, and `Err(())`-with-const when
    /// the binding is const (distinguished by the caller via lookup).
    pub fn set(&self, name: &str, value: Value) -> Result<(), SetError> {
        let mut borrow = self.vars.borrow_mut();
        for (n, v, is_const) in borrow.iter_mut().rev() {
            if n == name {
                if *is_const {
                    return Err(SetError::Const);
                }
                *v = value;
                return Ok(());
            }
        }
        drop(borrow);
        match &self.parent {
            Some(p) => p.set(name, value),
            None => Err(SetError::NotFound),
        }
    }

    /// Whether `name` is bound anywhere in the chain.
    pub fn has(&self, name: &str) -> bool {
        if self.vars.borrow().iter().any(|(n, _, _)| n == name) {
            return true;
        }
        self.parent.as_ref().is_some_and(|p| p.has(name))
    }

    /// Whether `name` is a const binding anywhere in the chain.
    pub fn is_const(&self, name: &str) -> bool {
        if self.vars.borrow().iter().any(|(n, _, c)| n == name && *c) {
            return true;
        }
        self.parent.as_ref().is_some_and(|p| p.is_const(name))
    }
}

/// Why an assignment failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetError {
    Const,
    NotFound,
}

/// An object's property list, in insertion order (JavaScript string keys
/// preserve insertion order).
pub type ObjMap = Vec<(String, Value)>;

/// A runtime value.
#[derive(Clone, Debug)]
pub enum Value {
    Num(f64),
    Str(Rc<str>),
    Bool(bool),
    Undefined,
    Null,
    Arr(Rc<RefCell<Vec<Value>>>),
    Obj(Rc<RefCell<ObjMap>>),
    /// An `@own` value: a handle into an activation arena. `readonly`
    /// marks borrows reached through `@ref` bindings.
    Own(crate::mem::OwnHandle),
    /// The previous owner of a moved `@own` value. Reading it is a
    /// use-after-move error.
    Moved,
    Func(Rc<Func>),
}

impl Value {
    /// JavaScript truthiness.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::Undefined | Value::Null | Value::Moved => false,
            Value::Arr(_) | Value::Obj(_) | Value::Own(_) | Value::Func(_) => true,
        }
    }

    /// Strict equality (`===`). Note `NaN !== NaN`.
    pub fn strict_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Undefined, Value::Undefined) => true,
            (Value::Null, Value::Null) => true,
            (Value::Arr(a), Value::Arr(b)) => Rc::ptr_eq(a, b),
            (Value::Obj(a), Value::Obj(b)) => Rc::ptr_eq(a, b),
            (Value::Own(a), Value::Own(b)) => a.arena == b.arena && a.slot == b.slot,
            (Value::Func(a), Value::Func(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Loose equality (`==`) for the common cases: same-type strictness,
    /// `null == undefined`, and number/string/bool coercion. Object
    /// coercion edge cases are a documented M1 gap.
    pub fn loose_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Undefined) | (Value::Undefined, Value::Null) => true,
            (Value::Num(_), Value::Num(_))
            | (Value::Str(_), Value::Str(_))
            | (Value::Bool(_), Value::Bool(_)) => self.strict_eq(other),
            (Value::Num(_), Value::Str(b)) => to_number(self) == to_number(&Value::Str(b.clone())),
            (Value::Str(_), Value::Num(_)) => self.loose_eq(&other.to_number_value()),
            (Value::Bool(_), _) => self.to_number_value().loose_eq(other),
            (_, Value::Bool(_)) => self.loose_eq(&other.to_number_value()),
            _ => false,
        }
    }

    /// `console.log` formatting for containers, matching Node's util
    /// inspect for the values memjs can build: `[ 1, 'x' ]`,
    /// `{ a: 1 }`, functions as `[Function: name]`. Strings inside
    /// containers are quoted (double quotes only when the string
    /// contains a single quote and no double quote). Object keys print
    /// bare when identifier-shaped, quoted otherwise. Matches Node for
    /// the nesting depths memjs programs reach in practice; very deep
    /// structures print fully instead of Node's `[Array]` truncation.
    pub fn inspect(&self) -> String {
        match self {
            Value::Arr(arr) => {
                let items = arr.borrow();
                if items.is_empty() {
                    return "[]".into();
                }
                let inner: Vec<String> = items.iter().map(|v| v.inspect()).collect();
                format!("[ {} ]", inner.join(", "))
            }
            Value::Obj(obj) => {
                let entries = obj.borrow();
                if entries.is_empty() {
                    return "{}".into();
                }
                let inner: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| {
                        let value = match v {
                            // Functions inside objects are named by
                            // their key, as Node does.
                            Value::Func(_) => format!("[Function: {k}]"),
                            other => other.inspect(),
                        };
                        format!("{}: {}", quote_key(k), value)
                    })
                    .collect();
                format!("{{ {} }}", inner.join(", "))
            }
            Value::Str(s) => quote_js(s),
            Value::Func(f) => match &**f {
                Func::Closure {
                    name: Some(name), ..
                } => format!("[Function: {name}]"),
                _ => "[Function (anonymous)]".into(),
            },
            other => other.to_display(),
        }
    }

    /// `Number(x)` coercion per JavaScript semantics.
    pub fn to_number_value(&self) -> Value {
        Value::Num(to_number(self))
    }

    /// JavaScript `String(x)`.
    pub fn to_display(&self) -> String {
        match self {
            Value::Num(n) => fmt_number(*n),
            Value::Str(s) => s.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Undefined => "undefined".into(),
            Value::Null => "null".into(),
            Value::Arr(_) => "[object Array]".into(),
            Value::Obj(_) => "[object Object]".into(),
            Value::Own(_) => "[own value]".into(),
            Value::Moved => "[moved]".into(),
            Value::Func(_) => "[function]".into(),
        }
    }
}

/// Formats an f64 the way JavaScript prints numbers.
pub fn fmt_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        }
    } else if n == n.trunc() && n.abs() < 1e21 {
        let mut out = String::new();
        let _ = write!(out, "{}", n as i128);
        out
    } else {
        format!("{n}")
    }
}

/// Quotes a string the way Node's inspect does: single quotes unless
/// the string contains a single quote and no double quote.
fn quote_js(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Object keys print bare when identifier-shaped, quoted otherwise
/// (numeric-ish and spaced keys get quotes, as Node prints them).
fn quote_key(key: &str) -> String {
    let ident_shaped = !key.is_empty()
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ident_shaped {
        key.to_string()
    } else {
        quote_js(key)
    }
}

fn to_number(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Null => 0.0,
        Value::Undefined => f64::NAN,
        Value::Str(s) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
        Value::Arr(_) | Value::Obj(_) | Value::Own(_) | Value::Moved | Value::Func(_) => f64::NAN,
    }
}
