//! The linjs abstract syntax tree.
//!
//! Statements and expressions cover the M1 JavaScript subset: `let` /
//! `const`, assignment, `if` / `else`, `while`, `for`, `for..of`,
//! `break` / `continue` / `return`, function declarations and arrow
//! functions (closures), calls, member and index access, ternaries,
//! logical and arithmetic operators. Strings, f64 numbers, booleans,
//! `undefined`, `null`, and arrays.

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
    /// `typeof x`
    Typeof,
}

/// A binary arithmetic/comparison operator (`+ - * / % < > <= >=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Lt,
    Gt,
    Le,
    Ge,
}

/// A short-circuiting logical operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
}

/// An equality operator. `==` performs JavaScript coercion; `===` is
/// strict; the `Ne` variants negate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqOp {
    Loose,
    LooseNe,
    Strict,
    StrictNe,
}

/// `++` / `--`, prefix or postfix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc,
    Dec,
}

/// A bitwise/shift operator. JavaScript defines all of these with
/// i32 semantics on numbers (`>>>` on u32), which maps 1:1 to wasm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOp {
    And,
    Or,
    Xor,
    Shl,
    /// `>>` — sign-propagating right shift.
    Shr,
    /// `>>>` — logical right shift (unsigned).
    UShr,
}

/// A Rust-style numeric cast: `expr as i32`. Checked statically; in
/// the dynamic engines it is erased (identity) — the conversions
/// (truncation, wrapping, extension) materialize in the WASM dialect.
#[derive(Debug, Clone, PartialEq)]
pub struct AsCast {
    pub expr: Box<Expr>,
    pub ann: TypeAnn,
}

/// The target shape of an assignment: `x`, `a[i]`, or `o.p`.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Ident(String),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    Array(Vec<Expr>),
    /// `{ a: 1, b }` — shorthand entries are resolved at parse time
    /// (`b` becomes `b: b`).
    Obj(Vec<ObjEntry>),
    Unary(UnaryOp, Box<Expr>),
    /// `~x` — bitwise not, i32 semantics.
    BitNot(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Bit(BitOp, Box<Expr>, Box<Expr>),
    AsCast(AsCast),
    Logical(LogicalOp, Box<Expr>, Box<Expr>),
    Eq(EqOp, Box<Expr>, Box<Expr>),
    /// `target = value` — `target` is an [`Expr::Ident`], index, or member.
    Assign(Target, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
    Call(Box<Expr>, Vec<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `(params) => expr` or `(params) => { ... }`; a single unparenthesized
    /// parameter (`x => ...`) is normalized into the same shape.
    Arrow(Vec<String>, Box<FnBody>),
    /// A `function` expression, including named declarations.
    Fn(Option<String>, Vec<String>, Vec<Stmt>),
    Update(UpdateOp, bool, Target),
}

/// The memory mode of a declaration, from `// @own` / `// @ref`
/// comment annotations. The default is [`Mem::Gc`] — ordinary
/// garbage-collected values. Annotated programs are still valid
/// JavaScript: the annotations are comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mem {
    /// An ordinary value behind reference counting.
    #[default]
    Gc,
    /// The value allocates into the current activation's arena and
    /// declarations of this mode take ownership (moves).
    Own,
    /// The declaration borrows an `@own` value: read-only, cannot
    /// outlive the activation that owns it.
    Ref,
}

/// One `key: value` entry of an object literal. Keys are normalized to
/// strings (identifier, string, and numeric keys all become strings, as
/// JavaScript object keys are).
#[derive(Debug, Clone, PartialEq)]
pub struct ObjEntry {
    pub key: String,
    pub value: Expr,
}

/// The body of an arrow function: a bare expression or a block.
#[derive(Debug, Clone, PartialEq)]
pub enum FnBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `let x = e, y;` / `const x = e;` — one keyword, any number of
    /// declarators (JavaScript does not allow mixing keywords). The
    /// memory mode comes from `// @own` / `// @ref` annotations and
    /// applies to every declarator in the statement.
    Let {
        is_const: bool,
        decls: Vec<Declarator>,
        mem: Mem,
    },
    /// `var x = e, y;` — function-scoped and hoisted to the function
    /// frame (unlike `let`, which is block-scoped).
    Var {
        decls: Vec<Declarator>,
        mem: Mem,
    },
    Expr(Expr),
    If(Box<Expr>, Box<Stmt>, Option<Box<Stmt>>),
    While(Box<Expr>, Box<Stmt>),
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Expr>,
        body: Box<Stmt>,
    },
    /// `for (const x of iter) { ... }` / `for (let x of iter) { ... }`
    ForOf {
        name: String,
        is_const: bool,
        iterable: Expr,
        body: Box<Stmt>,
    },
    /// `for (const k in obj) { ... }` — iterates string keys (array
    /// indices as strings, per JavaScript).
    ForIn {
        name: String,
        is_const: bool,
        iterable: Expr,
        body: Box<Stmt>,
    },
    Block(Vec<Stmt>),
    Return(Option<Expr>),
    Break,
    Continue,
    /// A `function` declaration inside a body (hoisted within its block).
    FnDecl(String, Vec<String>, Vec<Stmt>),
    Empty,
}

/// A parsed function head: name, params, body, declared return type.
#[derive(Debug, Clone, PartialEq)]
pub struct FnParts {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub ret: Option<TypeAnn>,
}

/// A parsed top-level function.
#[derive(Debug, Clone, PartialEq)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    /// The declared return type, if any. Erased at runtime.
    pub ret: Option<TypeAnn>,
}

/// One function parameter: a name plus an optional type annotation.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ann: Option<TypeAnn>,
}

/// A type annotation. The `number`/`f64` pair are aliases (`number` is
/// the JS-compatible default); the integer and float annotations are
/// Rust-style types whose exact semantics — real i32/i64 valtypes,
/// wrapping arithmetic, trapping division — materialize in the WASM
/// dialect. Purely static — parsed, checked, and erased at runtime in
/// the dynamic engines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeAnn {
    /// `number` / `f64`.
    Num,
    Str,
    Bool,
    /// The gradual escape hatch: compatible with everything.
    Any,
    Array(Box<TypeAnn>),
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    Usize,
    Isize,
}

impl TypeAnn {
    /// The source-level name (`"number[]"` for nested arrays).
    pub fn name(&self) -> String {
        match self {
            TypeAnn::Num => "number".into(),
            TypeAnn::Str => "string".into(),
            TypeAnn::Bool => "boolean".into(),
            TypeAnn::Any => "any".into(),
            TypeAnn::Array(inner) => format!("{}[]", inner.name()),
            TypeAnn::I8 => "i8".into(),
            TypeAnn::U8 => "u8".into(),
            TypeAnn::I16 => "i16".into(),
            TypeAnn::U16 => "u16".into(),
            TypeAnn::I32 => "i32".into(),
            TypeAnn::U32 => "u32".into(),
            TypeAnn::I64 => "i64".into(),
            TypeAnn::U64 => "u64".into(),
            TypeAnn::F32 => "f32".into(),
            TypeAnn::Usize => "usize".into(),
            TypeAnn::Isize => "isize".into(),
        }
    }
}

/// One `let`/`const`/`var` declarator: name, optional type annotation,
/// optional initializer.
#[derive(Debug, Clone, PartialEq)]
pub struct Declarator {
    pub name: String,
    pub ann: Option<TypeAnn>,
    pub init: Option<Expr>,
}
