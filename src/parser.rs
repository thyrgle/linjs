//! The linjs parser: tokens to AST via precedence climbing.
//!
//! All positions are absolute byte offsets into the source. Semicolons
//! are required (a documented divergence from JavaScript's ASI). The
//! parser is the only place that decides where top-level items end, so
//! [`top_level_items`] drives linjs's regionization.

use crate::ast::*;
use crate::lexer::{Comment, Tok, Token};

/// A parsing failure at an absolute byte offset.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub at: usize,
    pub message: String,
}

type PResult<T> = Result<T, ParseError>;

/// One top-level item: a function declaration or a statement, plus the
/// absolute span it occupies in the source.
#[derive(Debug, Clone, PartialEq)]
pub enum TopItem {
    Fn {
        span: (usize, usize),
        def: Result<FnDef, ParseError>,
    },
    Stmt {
        span: (usize, usize),
        stmt: Result<Stmt, ParseError>,
    },
}

impl TopItem {
    pub fn span(&self) -> (usize, usize) {
        match self {
            TopItem::Fn { span, .. } | TopItem::Stmt { span, .. } => *span,
        }
    }
}

pub struct Parser<'s> {
    src: &'s str,
    toks: &'s [Token],
    comments: &'s [Comment],
    /// Index of the next unconsumed comment.
    comment_pos: usize,
    pos: usize,
}

/// Parses the whole program into top-level items. A parse failure in one
/// item does not abort the program: the failing item is reported and the
/// remainder becomes a single trailing `Stmt` item carrying the error, so
/// diagnostics can point at one place instead of cascading.
pub fn top_level_items(src: &str, toks: &[Token], comments: &[Comment]) -> Vec<TopItem> {
    let mut p = Parser {
        src,
        toks,
        comments,
        comment_pos: 0,
        pos: 0,
    };
    let mut items = Vec::new();
    loop {
        if matches!(p.peek(), Tok::Eof) {
            break;
        }
        let start = p.start_offset();
        if matches!(p.peek(), Tok::Function) {
            match p.parse_fn_decl() {
                Ok((def, end)) => items.push(TopItem::Fn {
                    span: (start, end),
                    def: Ok(def),
                }),
                Err(e) => {
                    items.push(TopItem::Stmt {
                        span: (e.at, src.len()),
                        stmt: Err(e),
                    });
                    break;
                }
            }
        } else {
            match p.parse_statement() {
                Ok(stmt) => items.push(TopItem::Stmt {
                    span: (start, p.prev_end()),
                    stmt: Ok(stmt),
                }),
                Err(e) => {
                    items.push(TopItem::Stmt {
                        span: (e.at, src.len()),
                        stmt: Err(e),
                    });
                    break;
                }
            }
        }
    }
    items
}

impl<'s> Parser<'s> {
    pub fn new(toks: &'s [Token], pos: usize) -> Self {
        Self {
            src: "",
            toks,
            comments: &[],
            comment_pos: 0,
            pos,
        }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].kind
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].kind
    }

    fn start_offset(&self) -> usize {
        self.toks[self.pos.min(self.toks.len() - 1)].start
    }

    fn prev_end(&self) -> usize {
        self.toks[self.pos.saturating_sub(1)].end
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == tok {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok) -> PResult<Token> {
        if self.peek() == tok {
            Ok(self.bump())
        } else {
            Err(self.err(format!("expected {tok:?}, found {:?}", self.peek())))
        }
    }

    fn err(&self, message: String) -> ParseError {
        ParseError {
            at: self.start_offset(),
            message,
        }
    }

    /// The line number (0-based) of a byte offset.
    fn line_of(&self, at: usize) -> usize {
        self.src[..at.min(self.src.len())]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
    }

    /// Consumes every comment that started before the current
    /// statement, returning the last `@own`/`@ref` annotation found
    /// among them (whole-line comments above the declaration).
    fn take_annotations(&mut self) -> Mem {
        let hi = self.start_offset();
        let mut mem = Mem::Gc;
        while self.comment_pos < self.comments.len() && self.comments[self.comment_pos].start < hi {
            if let Some(mode) = self.comments[self.comment_pos].annotation() {
                mem = mode;
            }
            self.comment_pos += 1;
        }
        mem
    }

    /// Consumes comments on the same line as `at` (trailing
    /// annotations), returning the last annotation among them.
    fn take_trailing(&mut self, at: usize) -> Mem {
        let line = self.line_of(at);
        let mut mem = Mem::Gc;
        while self.comment_pos < self.comments.len()
            && self.line_of(self.comments[self.comment_pos].start) == line
        {
            if let Some(mode) = self.comments[self.comment_pos].annotation() {
                mem = mode;
            }
            self.comment_pos += 1;
        }
        mem
    }

    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                Ok(name)
            }
            other => Err(self.err(format!("expected identifier, found {other:?}"))),
        }
    }

    // ---- statements ----

    pub fn parse_statement(&mut self) -> PResult<Stmt> {
        match self.peek().clone() {
            Tok::Semi => {
                self.bump();
                Ok(Stmt::Empty)
            }
            Tok::LBrace => Ok(Stmt::Block(self.parse_block()?)),
            Tok::Let | Tok::Const => {
                let above = self.take_annotations();
                let is_const = self.peek() == &Tok::Const;
                let decls = self.parse_declarators(is_const)?;
                let semi = self.expect(&Tok::Semi)?;
                let mem = self.take_trailing(semi.end);
                let mem = if matches!(mem, Mem::Gc) { above } else { mem };
                Ok(Stmt::Let {
                    is_const,
                    decls,
                    mem,
                })
            }
            Tok::Var => {
                let above = self.take_annotations();
                let decls = self.parse_declarators(false)?;
                let semi = self.expect(&Tok::Semi)?;
                let mem = self.take_trailing(semi.end);
                let mem = if matches!(mem, Mem::Gc) { above } else { mem };
                Ok(Stmt::Var { decls, mem })
            }
            Tok::If => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let cond = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                let then = self.parse_statement()?;
                let els = if self.eat(&Tok::Else) {
                    Some(Box::new(self.parse_statement()?))
                } else {
                    None
                };
                Ok(Stmt::If(Box::new(cond), Box::new(then), els))
            }
            Tok::While => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let cond = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                let body = self.parse_statement()?;
                Ok(Stmt::While(Box::new(cond), Box::new(body)))
            }
            Tok::For => {
                self.bump();
                self.expect(&Tok::LParen)?;
                // for (let/const/var x of iter) and for (let/const/var k in obj)
                let declared = matches!(self.peek(), Tok::Let | Tok::Const | Tok::Var)
                    && matches!(self.peek_at(2), Tok::Of | Tok::In);
                if declared {
                    let is_const = self.peek() == &Tok::Const;
                    self.bump(); // the keyword
                    let name = self.ident()?;
                    let is_in = self.peek() == &Tok::In;
                    self.bump(); // of | in
                    let iterable = self.parse_expr()?;
                    self.expect(&Tok::RParen)?;
                    let body = self.parse_statement()?;
                    return if is_in {
                        Ok(Stmt::ForIn {
                            name,
                            is_const,
                            iterable,
                            body: Box::new(body),
                        })
                    } else {
                        Ok(Stmt::ForOf {
                            name,
                            is_const,
                            iterable,
                            body: Box::new(body),
                        })
                    };
                }
                let init = if self.eat(&Tok::Semi) {
                    None
                } else {
                    let stmt = match self.peek() {
                        Tok::Let | Tok::Const => {
                            let is_const = self.peek() == &Tok::Const;
                            let decls = self.parse_declarators(is_const)?;
                            Stmt::Let {
                                is_const,
                                decls,
                                mem: Mem::Gc,
                            }
                        }
                        Tok::Var => {
                            let decls = self.parse_declarators(false)?;
                            Stmt::Var {
                                decls,
                                mem: Mem::Gc,
                            }
                        }
                        _ => Stmt::Expr(self.parse_expr()?),
                    };
                    self.expect(&Tok::Semi)?;
                    Some(Box::new(stmt))
                };
                let cond = if matches!(self.peek(), Tok::Semi) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.expect(&Tok::Semi)?;
                let step = if matches!(self.peek(), Tok::RParen) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.expect(&Tok::RParen)?;
                let body = self.parse_statement()?;
                Ok(Stmt::For {
                    init,
                    cond,
                    step,
                    body: Box::new(body),
                })
            }
            Tok::Return => {
                self.bump();
                let value = if matches!(self.peek(), Tok::Semi) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Return(value))
            }
            Tok::Break => {
                self.bump();
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Break)
            }
            Tok::Continue => {
                self.bump();
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Continue)
            }
            Tok::Function => {
                let parts = self.parse_fn_parts()?;
                Ok(Stmt::FnDecl(
                    parts.name,
                    parts.params.into_iter().map(|p| p.name).collect(),
                    parts.body,
                ))
            }
            _ => {
                let expr = self.parse_expr()?;
                self.expect(&Tok::Semi)?;
                Ok(Stmt::Expr(expr))
            }
        }
    }

    /// Parses the declarator list of `let`/`const`/`var` — no trailing
    /// semicolon. Shared by statement position and the `for` header.
    /// Type annotations (`let x: number = ...`) are parsed and carried
    /// on the declarator; they are erased at runtime.
    fn parse_declarators(&mut self, is_const: bool) -> PResult<Vec<Declarator>> {
        self.bump(); // the keyword
        let mut decls = Vec::new();
        loop {
            let name = self.ident()?;
            let ann = if self.eat(&Tok::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            let init = if self.eat(&Tok::Assign) {
                Some(self.parse_expr()?)
            } else {
                if is_const {
                    return Err(self.err("`const` requires an initializer".into()));
                }
                None
            };
            decls.push(Declarator { name, ann, init });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(decls)
    }

    /// Parses a type annotation: `number`, `string`, `boolean`, `any`,
    /// or `T[]` (any nesting of `[]`).
    fn parse_type(&mut self) -> PResult<TypeAnn> {
        let base = match self.peek() {
            Tok::Ident(name) => match name.as_str() {
                "number" | "f64" => TypeAnn::Num,
                "string" => TypeAnn::Str,
                "boolean" => TypeAnn::Bool,
                "any" => TypeAnn::Any,
                "i8" => TypeAnn::I8,
                "u8" => TypeAnn::U8,
                "i16" => TypeAnn::I16,
                "u16" => TypeAnn::U16,
                "i32" => TypeAnn::I32,
                "u32" => TypeAnn::U32,
                "i64" => TypeAnn::I64,
                "u64" => TypeAnn::U64,
                "f32" => TypeAnn::F32,
                "usize" => TypeAnn::Usize,
                "isize" => TypeAnn::Isize,
                other => {
                    return Err(self.err(format!(
                        "unknown type `{other}` (expected number, string, boolean, any, i32..i64, u8..u64, f32, or T[])"
                    )))
                }
            },
            other => return Err(self.err(format!("expected a type, found {other:?}"))),
        };
        self.bump();
        let mut ty = base;
        while self.eat(&Tok::LBracket) {
            self.expect(&Tok::RBracket)?;
            ty = TypeAnn::Array(Box::new(ty));
        }
        Ok(ty)
    }

    pub fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(&Tok::LBrace)?;
        let mut stmts = Vec::new();
        while !matches!(self.peek(), Tok::RBrace) {
            if matches!(self.peek(), Tok::Eof) {
                return Err(self.err("unterminated block".into()));
            }
            stmts.push(self.parse_statement()?);
        }
        self.expect(&Tok::RBrace)?;
        Ok(stmts)
    }

    fn parse_fn_parts(&mut self) -> PResult<crate::ast::FnParts> {
        self.expect(&Tok::Function)?;
        let name = self.ident()?;
        let params = self.parse_params()?;
        let ret = if self.eat(&Tok::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        Ok(FnParts {
            name,
            params,
            body,
            ret,
        })
    }

    pub fn parse_fn_decl(&mut self) -> PResult<(FnDef, usize)> {
        let parts = self.parse_fn_parts()?;
        Ok((
            FnDef {
                name: parts.name,
                params: parts.params,
                body: parts.body,
                ret: parts.ret,
            },
            self.prev_end(),
        ))
    }

    fn parse_params(&mut self) -> PResult<Vec<Param>> {
        self.expect(&Tok::LParen)?;
        let mut params = Vec::new();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                let name = self.ident()?;
                let ann = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push(Param { name, ann });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(params)
    }

    // ---- expressions (precedence climbing) ----

    pub fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_assignment()
    }

    fn parse_assignment(&mut self) -> PResult<Expr> {
        let lhs = self.parse_ternary()?;
        let op = self.peek().clone();
        let target_op = match op {
            Tok::Assign => Some(None),
            Tok::PlusAssign => Some(Some(BinOp::Add)),
            Tok::MinusAssign => Some(Some(BinOp::Sub)),
            Tok::StarAssign => Some(Some(BinOp::Mul)),
            Tok::SlashAssign => Some(Some(BinOp::Div)),
            _ => None,
        };
        if let Some(combine) = target_op {
            let target =
                expr_to_target(&lhs).ok_or_else(|| self.err("invalid assignment target".into()))?;
            self.bump();
            let mut value = self.parse_assignment()?;
            // Desugar `t op= v` into `t = t op v`. The target is
            // evaluated twice — a documented M1 simplification.
            if let Some(bin) = combine {
                value = Expr::Binary(bin, Box::new(lhs), Box::new(value));
            }
            return Ok(Expr::Assign(target, Box::new(value)));
        }
        Ok(lhs)
    }

    fn parse_ternary(&mut self) -> PResult<Expr> {
        let cond = self.parse_logical_or()?;
        if self.eat(&Tok::Question) {
            let then = self.parse_assignment()?;
            self.expect(&Tok::Colon)?;
            let els = self.parse_assignment()?;
            return Ok(Expr::Ternary(Box::new(cond), Box::new(then), Box::new(els)));
        }
        Ok(cond)
    }

    fn parse_logical_or(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_logical_and()?;
        while self.peek() == &Tok::Or {
            self.bump();
            let rhs = self.parse_logical_and()?;
            lhs = Expr::Logical(LogicalOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_logical_and(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_equality()?;
        while self.peek() == &Tok::And {
            self.bump();
            let rhs = self.parse_equality()?;
            lhs = Expr::Logical(LogicalOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Tok::EqStrict => EqOp::Strict,
                Tok::NeStrict => EqOp::StrictNe,
                Tok::Eq => EqOp::Loose,
                Tok::Ne => EqOp::LooseNe,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_comparison()?;
            lhs = Expr::Eq(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Shifts bind tighter than relational comparisons (JS precedence).
    fn parse_shift(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_bit_and()?;
        loop {
            let op = match self.peek() {
                Tok::Shl => BitOp::Shl,
                Tok::Shr => BitOp::Shr,
                Tok::UShr => BitOp::UShr,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.parse_bit_and()?;
            lhs = Expr::Bit(op, Box::new(lhs), Box::new(rhs));
        }
    }

    fn parse_bit_and(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_bit_xor()?;
        while self.peek() == &Tok::Amp {
            self.bump();
            let rhs = self.parse_bit_xor()?;
            lhs = Expr::Bit(BitOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_bit_xor(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_bit_or()?;
        while self.peek() == &Tok::Caret {
            self.bump();
            let rhs = self.parse_bit_or()?;
            lhs = Expr::Bit(BitOp::Xor, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_bit_or(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_additive()?;
        loop {
            if self.peek() != &Tok::Pipe {
                return Ok(lhs);
            }
            self.bump();
            let rhs = self.parse_additive()?;
            lhs = Expr::Bit(BitOp::Or, Box::new(lhs), Box::new(rhs));
        }
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_shift()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => BinOp::Lt,
                Tok::Gt => BinOp::Gt,
                Tok::Le => BinOp::Le,
                Tok::Ge => BinOp::Ge,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_additive()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let op = match self.peek() {
            Tok::Not => Some(UnaryOp::Not),
            Tok::Minus => Some(UnaryOp::Neg),
            Tok::Typeof => Some(UnaryOp::Typeof),
            Tok::Inc | Tok::Dec => {
                let upd = if self.peek() == &Tok::Inc {
                    UpdateOp::Inc
                } else {
                    UpdateOp::Dec
                };
                self.bump();
                let target = self.parse_unary()?;
                let target = expr_to_target(&target)
                    .ok_or_else(|| self.err("invalid update target".into()))?;
                return Ok(Expr::Update(upd, true, target));
            }
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let expr = self.parse_unary()?;
            return Ok(Expr::Unary(op, Box::new(expr)));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    self.bump();
                    let args = self.parse_args()?;
                    expr = Expr::Call(Box::new(expr), args);
                }
                Tok::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    self.expect(&Tok::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(index));
                }
                Tok::Dot => {
                    self.bump();
                    let prop = self.property_name()?;
                    expr = Expr::Member(Box::new(expr), prop);
                }
                Tok::Inc | Tok::Dec => {
                    let upd = if self.peek() == &Tok::Inc {
                        UpdateOp::Inc
                    } else {
                        UpdateOp::Dec
                    };
                    let target = expr_to_target(&expr)
                        .ok_or_else(|| self.err("invalid update target".into()))?;
                    self.bump();
                    expr = Expr::Update(upd, false, target);
                }
                Tok::As => {
                    // Rust-style cast: `expr as i32`. Binds tighter than
                    // binary operators (parenthesize compound LHS).
                    self.bump();
                    let ann = self.parse_type()?;
                    expr = Expr::AsCast(crate::ast::AsCast {
                        expr: Box::new(expr),
                        ann,
                    });
                }
                Tok::Tilde => {
                    self.bump();
                    let inner = self.parse_postfix()?;
                    expr = Expr::BitNot(Box::new(inner));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_args(&mut self) -> PResult<Vec<Expr>> {
        let mut args = Vec::new();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(args)
    }

    fn property_name(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                Ok(name)
            }
            other => Err(self.err(format!("expected property name, found {other:?}"))),
        }
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        match self.peek().clone() {
            Tok::Num(n) => {
                self.bump();
                Ok(Expr::Num(n))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
            }
            Tok::True => {
                self.bump();
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.bump();
                Ok(Expr::Bool(false))
            }
            Tok::Null => {
                self.bump();
                Ok(Expr::Null)
            }
            Tok::Undefined => {
                self.bump();
                Ok(Expr::Undefined)
            }
            Tok::Ident(name) => {
                // `x => ...` — single-parameter arrow without parens.
                if matches!(self.peek_at(1), Tok::Arrow) {
                    self.bump();
                    self.bump();
                    return self.parse_arrow_body(vec![name]);
                }
                self.bump();
                Ok(Expr::Ident(name))
            }
            Tok::LParen => {
                // `(...) => ...` or a parenthesized expression.
                if self.is_arrow_params()? {
                    let params: Vec<String> = {
                        self.bump(); // (
                        let mut names = Vec::new();
                        if !matches!(self.peek(), Tok::RParen) {
                            loop {
                                let name = self.ident()?;
                                // Typed arrow params: `(a: number) =>`.
                                // Annotations are erased; only the name
                                // reaches the expression tree.
                                if self.eat(&Tok::Colon) {
                                    self.parse_type()?;
                                }
                                names.push(name);
                                if !self.eat(&Tok::Comma) {
                                    break;
                                }
                            }
                        }
                        self.expect(&Tok::RParen)?;
                        self.expect(&Tok::Arrow)?;
                        names
                    };
                    return self.parse_arrow_body(params);
                }
                self.bump();
                let expr = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(expr)
            }
            Tok::LBrace => {
                self.bump();
                let mut entries = Vec::new();
                if !matches!(self.peek(), Tok::RBrace) {
                    loop {
                        let key = match self.peek().clone() {
                            Tok::Ident(k) | Tok::Str(k) => {
                                self.bump();
                                k
                            }
                            Tok::Num(n) => {
                                self.bump();
                                crate::value::fmt_number(n)
                            }
                            other => {
                                return Err(
                                    self.err(format!("expected object key, found {other:?}"))
                                )
                            }
                        };
                        if self.eat(&Tok::Colon) {
                            let value = self.parse_assignment()?;
                            entries.push(ObjEntry { key, value });
                        } else {
                            // Shorthand: `{ a }` is `{ a: a }`.
                            entries.push(ObjEntry {
                                key: key.clone(),
                                value: Expr::Ident(key),
                            });
                        }
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                        // A trailing comma before `}` is legal JavaScript.
                        if matches!(self.peek(), Tok::RBrace) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBrace)?;
                Ok(Expr::Obj(entries))
            }
            Tok::LBracket => {
                self.bump();
                let mut items = Vec::new();
                if !matches!(self.peek(), Tok::RBracket) {
                    loop {
                        items.push(self.parse_assignment()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                        // A trailing comma before `]` is legal JavaScript.
                        if matches!(self.peek(), Tok::RBracket) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBracket)?;
                Ok(Expr::Array(items))
            }
            Tok::Function => {
                self.bump();
                let name = match self.peek() {
                    Tok::Ident(n) => {
                        let n = n.clone();
                        self.bump();
                        Some(n)
                    }
                    _ => None,
                };
                let params = self.parse_params()?;
                let body = self.parse_block()?;
                Ok(Expr::Fn(
                    name,
                    params.into_iter().map(|p| p.name).collect(),
                    body,
                ))
            }
            other => Err(self.err(format!("unexpected token {other:?}"))),
        }
    }

    /// Looks ahead from the current `(` to decide whether this is an
    /// arrow-function parameter list: the matching `)` must be followed
    /// by `=>`.
    fn is_arrow_params(&self) -> PResult<bool> {
        let mut depth = 0usize;
        let mut i = self.pos;
        loop {
            let kind = &self.toks.get(i).map(|t| &t.kind).unwrap_or(&Tok::Eof);
            match kind {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        let next = &self.toks.get(i + 1).map(|t| &t.kind).unwrap_or(&Tok::Eof);
                        return Ok(matches!(next, Tok::Arrow));
                    }
                }
                Tok::Eof => return Err(self.err("unterminated parenthesis".into())),
                _ => {}
            }
            i += 1;
        }
    }

    fn parse_arrow_body(&mut self, params: Vec<String>) -> PResult<Expr> {
        let body = if matches!(self.peek(), Tok::LBrace) {
            FnBody::Block(self.parse_block()?)
        } else {
            FnBody::Expr(Box::new(self.parse_assignment()?))
        };
        Ok(Expr::Arrow(params, Box::new(body)))
    }
}

fn expr_to_target(expr: &Expr) -> Option<Target> {
    match expr {
        Expr::Ident(name) => Some(Target::Ident(name.clone())),
        Expr::Index(obj, index) => Some(Target::Index(obj.clone(), index.clone())),
        Expr::Member(obj, prop) => Some(Target::Member(obj.clone(), prop.clone())),
        _ => None,
    }
}
