//! The WASM backend: compiles the strict dialect to WebAssembly.
//!
//! The strict dialect is the thesis made literal. Linear memory has no
//! garbage collector, so this backend accepts only programs whose heap
//! values are `@own`:
//!
//! * **Numbers** (f64), booleans, arithmetic, comparisons, logical
//!   operators, ternaries.
//! * `let`/`const`, assignment, compound assignment, `++`/`--`.
//! * `if`/`else`, `while`, `for`, `for..of` over arrays, `break`,
//!   `continue`.
//! * Functions with numeric parameters and numeric results; direct
//!   calls (including recursion).
//! * `console.log(numeric)`, through an imported host function.
//! * **`// @own` arrays of numbers**: fixed-length, allocated in linear
//!   memory from a bump arena — an `i32` reference, elements at
//!   `ref + 8 + 8*i`, the length in an i32 header. Unannotated arrays
//!   are a compile error: there is no GC to fall back on.
//!
//! Frame teardown is the payoff. Every function saves the arena pointer
//! on entry and restores it on return; that is sound *only because*
//! `@own` values cannot escape, which the memory rules already
//! guarantee. Callee allocations die at the callee's return, for free.
//!
//! Divergences from JavaScript (documented, not bugs): literals
//! evaluate after allocation, `@ref` write-protection is trusted rather
//! than tracked, arrays are fixed-length (no `push`), only numeric
//! parameters cross function boundaries, and every function returns a
//! numeric value.

use std::collections::HashMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, ExportKind, ExportSection, Function, FunctionSection,
    GlobalSection, GlobalType, ImportSection, Instruction as Ins, MemArg, MemorySection,
    MemoryType, Module, TypeSection, ValType,
};

use crate::ast::*;
use crate::interp::Item;

/// Why the strict dialect rejected a program.
#[derive(Debug, Clone, PartialEq)]
pub struct WasmError {
    pub message: String,
}

fn unsupported(what: &str) -> WasmError {
    WasmError {
        message: format!("WASM backend (strict dialect): {what} is not supported"),
    }
}

fn needs_own() -> WasmError {
    WasmError {
        message: "WASM backend: arrays require // @own — linear memory has no garbage collector"
            .into(),
    }
}

/// The static types of the strict dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Num,
    Bool,
    Arr,
}

impl Ty {
    fn val(self) -> ValType {
        match self {
            Ty::Num => ValType::F64,
            Ty::Bool | Ty::Arr => ValType::I32,
        }
    }

    fn zero(self) -> Ins<'static> {
        match self {
            Ty::Num => Ins::F64Const((0.0).into()),
            _ => Ins::I32Const(0),
        }
    }
}

/// Validates `.wasm` bytes with wasmparser.
pub fn validate(bytes: &[u8]) -> Result<(), String> {
    use wasmparser::Validator;
    let mut v = Validator::new();
    match v.validate_all(bytes) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Compiles the strict dialect to raw `.wasm` bytes (validated before
/// returning).
pub fn compile(source: &str) -> Result<Vec<u8>, WasmError> {
    let (items, errors) =
        crate::passes::parse_program(source).map_err(|e| WasmError { message: e })?;
    if !errors.is_empty() {
        return Err(WasmError {
            message: format!("program has parse errors: {}", errors[0].1),
        });
    }

    // Function index space: the `log` import is 0, `$alloc` is 1, user
    // functions from 2, `$run` last.
    let mut user_fns: Vec<&FnDef> = Vec::new();
    let mut main_stmts: Vec<&Stmt> = Vec::new();
    for item in &items {
        match item {
            Item::Fn(def) => user_fns.push(def),
            Item::Stmt(stmt) => main_stmts.push(stmt),
        }
    }

    // The log imports: one per console.log arity used (log1..logN),
    // each printing its arguments space-joined on one line.
    fn max_log_args(stmts: &[&Stmt]) -> usize {
        let mut max = 1usize;
        let mut queue: Vec<&Stmt> = stmts.to_vec();
        while let Some(stmt) = queue.pop() {
            walk_for_log(stmt, &mut max);
        }
        max
    }
    fn walk_for_log(stmt: &Stmt, max: &mut usize) {
        match stmt {
            Stmt::Expr(e) => visit_log_expr(e, max),
            Stmt::Let { decls, .. } | Stmt::Var { decls, .. } => {
                for (_, init) in decls {
                    if let Some(init) = init {
                        visit_log_expr(init, max);
                    }
                }
            }
            Stmt::If(cond, then, els) => {
                visit_log_expr(cond, max);
                walk_for_log(then, max);
                if let Some(els) = els {
                    walk_for_log(els, max);
                }
            }
            Stmt::While(cond, body) => {
                visit_log_expr(cond, max);
                walk_for_log(body, max);
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                if let Some(init) = init {
                    walk_for_log(init, max);
                }
                if let Some(cond) = cond {
                    visit_log_expr(cond, max);
                }
                if let Some(step) = step {
                    visit_log_expr(step, max);
                }
                walk_for_log(body, max);
            }
            Stmt::ForOf { iterable, body, .. } | Stmt::ForIn { iterable, body, .. } => {
                visit_log_expr(iterable, max);
                walk_for_log(body, max);
            }
            Stmt::Block(stmts) => {
                for s in stmts {
                    walk_for_log(s, max);
                }
            }
            Stmt::Return(Some(e)) => visit_log_expr(e, max),
            _ => {}
        }
    }
    fn visit_log_expr(expr: &Expr, max: &mut usize) {
        match expr {
            Expr::Call(callee, args) => {
                if let Expr::Member(obj_expr, prop) = &**callee {
                    if matches!(**obj_expr, Expr::Ident(ref n) if n == "console") && prop == "log" {
                        *max = (*max).max(args.len());
                    }
                }
                for a in args {
                    visit_log_expr(a, max);
                }
                if let Expr::Call(c, _) = expr {
                    let _ = c;
                }
            }
            Expr::Array(items) => items.iter().for_each(|e| visit_log_expr(e, max)),
            Expr::Unary(_, e) => visit_log_expr(e, max),
            Expr::Binary(_, l, r) | Expr::Logical(_, l, r) | Expr::Eq(_, l, r) => {
                visit_log_expr(l, max);
                visit_log_expr(r, max);
            }
            Expr::Ternary(c, t, e) => {
                visit_log_expr(c, max);
                visit_log_expr(t, max);
                visit_log_expr(e, max);
            }
            _ => {}
        }
    }

    // The log imports: log1..logN, one per console.log arity used.
    // Function index space: log1..logN are 0..N-1, $alloc is N, user
    // functions follow, $run last.
    let all_stmts: Vec<&Stmt> = items
        .iter()
        .filter_map(|item| match item {
            Item::Stmt(stmt) => Some(stmt),
            Item::Fn(_) => None,
        })
        .collect();
    let mut max_log = max_log_args(&all_stmts);
    for def in &user_fns {
        let stmts: Vec<&Stmt> = def.body.iter().collect();
        max_log = max_log.max(max_log_args(&stmts));
    }

    let alloc_idx: u32 = max_log as u32;
    let mut signatures: HashMap<String, (u32, usize)> = HashMap::new();
    for (i, def) in user_fns.iter().enumerate() {
        signatures.insert(
            def.name.clone(),
            (alloc_idx + 1 + i as u32, def.params.len()),
        );
    }

    let mut types = TypeSection::new();
    let mut type_ids: HashMap<(Vec<ValType>, Vec<ValType>), u32> = HashMap::new();
    let ty = |types: &mut TypeSection,
              type_ids: &mut HashMap<(Vec<ValType>, Vec<ValType>), u32>,
              params: Vec<ValType>,
              results: Vec<ValType>|
     -> u32 {
        let key = (params.clone(), results.clone());
        if let Some(idx) = type_ids.get(&key) {
            return *idx;
        }
        let idx = type_ids.len() as u32;
        types.ty().function(params, results);
        type_ids.insert(key, idx);
        idx
    };

    let mut log_types = Vec::new();
    for k in 1..=max_log {
        let params = vec![ValType::F64; k];
        log_types.push(ty(&mut types, &mut type_ids, params, vec![]));
    }
    let alloc_ty = ty(
        &mut types,
        &mut type_ids,
        vec![ValType::I32],
        vec![ValType::I32],
    );

    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();

    // $alloc(len: i32) -> i32: bump-allocate an array (header + elems),
    // growing memory as needed.
    let mut alloc_fn = Function::new(vec![(2, ValType::I32)]); // 0: len, 1: bytes, 2: grow result
    for ins in [
        // bytes = 8 + len * 8
        Ins::LocalGet(0),
        Ins::I32Const(8),
        Ins::I32Mul,
        Ins::I32Const(8),
        Ins::I32Add,
        Ins::LocalSet(1),
        Ins::Block(BlockType::Empty),
        Ins::Loop(BlockType::Empty),
        // fits? (arena + bytes <= memory.size * 65536)
        Ins::GlobalGet(0),
        Ins::LocalGet(1),
        Ins::I32Add,
        Ins::MemorySize(0),
        Ins::I32Const(16),
        Ins::I32Shl,
        Ins::I32LeU,
        Ins::BrIf(1),
        // grow one page; trap on failure (result -1 < 0)
        Ins::I32Const(1),
        Ins::MemoryGrow(0),
        Ins::I32Const(0),
        Ins::I32LtS,
        Ins::If(BlockType::Empty),
        Ins::Unreachable,
        Ins::End,
        Ins::Br(0),
        Ins::End,
        Ins::End,
        // header: length at ptr + 0
        Ins::GlobalGet(0),
        Ins::LocalGet(0),
        Ins::I32Store(MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        }),
        // result = old arena; arena += bytes
        Ins::GlobalGet(0),
        Ins::GlobalGet(0),
        Ins::LocalGet(1),
        Ins::I32Add,
        Ins::GlobalSet(0),
        Ins::End, // function body terminator
    ] {
        alloc_fn.instruction(&ins);
    }
    functions.function(alloc_ty);
    code.function(&alloc_fn);

    // User functions.
    let mut fn_count = alloc_idx + 1;
    for def in &user_fns {
        let mut c = FnCompiler::new(&def.params, &signatures, alloc_idx);
        c.compile_stmts(&def.body.iter().collect::<Vec<_>>())?;
        // Safety net for paths that fall off the end.
        c.emit(Ins::F64Const((0.0).into()));
        c.emit(Ins::Return);
        let params = vec![ValType::F64; def.params.len()];
        let results = vec![ValType::F64];
        let sig = ty(&mut types, &mut type_ids, params.clone(), results);
        functions.function(sig);
        let mut wasm_fn = Function::new(c.local_decls);
        for ins in &c.code {
            wasm_fn.instruction(ins);
        }
        wasm_fn.instruction(&Ins::End); // function body terminator
        code.function(&wasm_fn);
        fn_count += 1;
    }

    // $run: the top-level statements.
    let mut run = FnCompiler::new(&[], &signatures, alloc_idx);
    run.compile_stmts(&main_stmts)?;
    let mut run_fn = Function::new(run.local_decls);
    for ins in &run.code {
        run_fn.instruction(ins);
    }
    run_fn.instruction(&Ins::End); // function body terminator
    let run_ty = ty(&mut types, &mut type_ids, vec![], vec![]);
    functions.function(run_ty);
    code.function(&run_fn);

    // ---- assemble ----
    let mut module = Module::new();
    // Spec section order: type, import, function, memory, global,
    // export, code.
    module.section(&types);
    let mut imports = ImportSection::new();
    for (i, log_ty) in log_types.iter().enumerate() {
        imports.import(
            "env",
            &format!("log{}", i + 1),
            wasm_encoder::EntityType::Function(*log_ty),
        );
    }
    module.section(&imports);
    module.section(&functions);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memories);
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(1024),
    );
    module.section(&globals);
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("run", ExportKind::Func, fn_count);
    module.section(&exports);
    module.section(&code);
    let bytes = module.finish();
    validate(&bytes).map_err(|e| WasmError {
        message: format!("internal: emitted module failed validation: {e}"),
    })?;
    Ok(bytes)
}

/// Compiles one function body.
struct FnCompiler<'s> {
    /// name -> (local index, type)
    locals: HashMap<String, (u32, Ty)>,
    next_local: u32,
    local_decls: Vec<(u32, ValType)>,
    code: Vec<Ins<'static>>,
    /// (break base, continue base): absolute label levels. The branch
    /// distance is `current depth - 1 - base`.
    loops: Vec<(u32, u32)>,
    alloc_idx: u32,
    /// Current label depth (blocks, loops, and ifs are labels).
    depth: u32,
    signatures: &'s HashMap<String, (u32, usize)>,
}

impl<'s> FnCompiler<'s> {
    fn new(
        params: &[String],
        signatures: &'s HashMap<String, (u32, usize)>,
        alloc_idx: u32,
    ) -> Self {
        let mut locals = HashMap::new();
        let mut next_local = 0u32;
        let mut local_decls = Vec::new();
        for p in params {
            locals.insert(p.clone(), (next_local, Ty::Num));
            local_decls.push((1, ValType::F64));
            next_local += 1;
        }
        FnCompiler {
            locals,
            next_local,
            local_decls,
            code: Vec::new(),
            loops: Vec::new(),
            depth: 0,
            alloc_idx,
            signatures,
        }
    }

    /// Emits JavaScript truthiness for the f64 on the stack: zero and
    /// NaN are falsy. Leaves an i32.
    fn emit_truthiness(&mut self) {
        let t = self.fresh(ValType::F64);
        self.emit(Ins::LocalSet(t));
        self.emit(Ins::LocalGet(t));
        self.emit(Ins::F64Const((0.0).into()));
        self.emit(Ins::F64Ne);
        self.emit(Ins::LocalGet(t));
        self.emit(Ins::LocalGet(t));
        self.emit(Ins::F64Eq);
        self.emit(Ins::I32And);
    }

    fn fresh(&mut self, vt: ValType) -> u32 {
        let idx = self.next_local;
        self.next_local += 1;
        self.local_decls.push((1, vt));
        idx
    }

    fn emit(&mut self, ins: Ins<'static>) {
        self.code.push(ins);
    }

    fn err(&self, what: &str) -> WasmError {
        unsupported(what)
    }

    // ---- statements ----

    fn compile_stmts(&mut self, stmts: &[&Stmt]) -> Result<(), WasmError> {
        for stmt in stmts {
            self.compile_stmt(stmt)?;
        }
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), WasmError> {
        match stmt {
            Stmt::Empty | Stmt::FnDecl(..) => Ok(()),
            Stmt::Let {
                decls,
                mem,
                is_const: _,
            }
            | Stmt::Var { decls, mem } => {
                for (name, init) in decls {
                    let ty = match init {
                        Some(init) => {
                            let ty = self.infer_expr(init)?;
                            if ty == Ty::Arr && *mem != Mem::Own {
                                return Err(needs_own());
                            }
                            self.compile_expr(init, Some(ty))?;
                            ty
                        }
                        None => Ty::Num,
                    };
                    let idx = self.fresh(ty.val());
                    self.locals.insert(name.clone(), (idx, ty));
                    if init.is_none() {
                        self.emit(ty.zero());
                    }
                    self.emit(Ins::LocalSet(idx));
                }
                Ok(())
            }
            Stmt::Expr(expr) => {
                self.compile_expr(expr, None)?;
                self.emit(Ins::Drop);
                Ok(())
            }
            Stmt::If(cond, then, els) => {
                self.compile_expr(cond, Some(Ty::Bool))?;
                self.emit(Ins::If(BlockType::Empty));
                self.depth += 1;
                self.compile_stmt(then)?;
                if let Some(els) = els {
                    self.emit(Ins::Else);
                    self.compile_stmt(els)?;
                }
                self.depth -= 1;
                self.emit(Ins::End);
                Ok(())
            }
            Stmt::While(cond, body) => {
                // block $exit { loop { cond; eqz; br_if $exit; body;
                // br $top } }
                let base = self.depth;
                self.emit(Ins::Block(BlockType::Empty));
                self.emit(Ins::Loop(BlockType::Empty));
                self.depth += 2;
                self.loops.push((base, base + 1));
                self.compile_expr(cond, Some(Ty::Bool))?;
                self.emit(Ins::I32Eqz);
                self.emit(Ins::BrIf(1));
                self.compile_stmt(body)?;
                self.emit(Ins::Br(0));
                self.depth -= 2;
                self.emit(Ins::End);
                self.emit(Ins::End);
                self.loops.pop();
                Ok(())
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => self.compile_for(init, cond, step, body),
            Stmt::ForOf {
                name,
                iterable,
                body,
                ..
            } => self.compile_for_of(name, iterable, body),
            Stmt::ForIn { .. } => Err(self.err("for..in (objects are not in the dialect)")),
            Stmt::Block(stmts) => {
                let refs: Vec<&Stmt> = stmts.iter().collect();
                self.compile_stmts(&refs)
            }
            Stmt::Return(expr) => match expr {
                Some(expr) => {
                    self.compile_expr(expr, Some(Ty::Num))?;
                    self.emit(Ins::Return);
                    Ok(())
                }
                None => {
                    Err(self.err("a `return` without a value (functions must return a number)"))
                }
            },
            Stmt::Break => {
                let base = self
                    .loops
                    .last()
                    .map(|(b, _)| *b)
                    .ok_or_else(|| self.err("break outside a loop"))?;
                let d = self.depth - 1 - base;
                self.emit(Ins::Br(d));
                Ok(())
            }
            Stmt::Continue => {
                let base = self
                    .loops
                    .last()
                    .map(|(_, c)| *c)
                    .ok_or_else(|| self.err("continue outside a loop"))?;
                let d = self.depth - 1 - base;
                self.emit(Ins::Br(d));
                Ok(())
            }
        }
    }

    /// for (init; cond; step) body:
    ///
    /// block $exit { loop { init-part done above; cond→br_if $exit;
    /// block $cont { body } step; br $top } }
    ///
    /// Inside the body: continue → br 0 ($cont), break → br 2 ($exit).
    fn compile_for(
        &mut self,
        init: &Option<Box<Stmt>>,
        cond: &Option<Expr>,
        step: &Option<Expr>,
        body: &Stmt,
    ) -> Result<(), WasmError> {
        if let Some(init) = init {
            self.compile_stmt(init)?;
        }
        let base = self.depth;
        self.emit(Ins::Block(BlockType::Empty));
        self.emit(Ins::Loop(BlockType::Empty));
        self.depth += 2;
        self.emit(Ins::Block(BlockType::Empty));
        self.depth += 1;
        self.loops.push((base, base + 2));
        if let Some(cond) = cond {
            self.compile_expr(cond, Some(Ty::Bool))?;
            self.emit(Ins::I32Eqz);
            self.emit(Ins::BrIf(2));
        }
        self.compile_stmt(body)?;
        self.emit(Ins::End); // $cont
        self.depth -= 1;
        if let Some(step) = step {
            self.compile_expr(step, None)?;
            self.emit(Ins::Drop);
        }
        self.emit(Ins::Br(0));
        self.depth -= 2;
        self.emit(Ins::End); // loop
        self.emit(Ins::End); // block
        self.loops.pop();
        Ok(())
    }

    /// for (const v of arr) body: an index loop over the array.
    fn compile_for_of(
        &mut self,
        name: &str,
        iterable: &Expr,
        body: &Stmt,
    ) -> Result<(), WasmError> {
        let arr_ty = self.infer_expr(iterable)?;
        if arr_ty != Ty::Arr {
            return Err(self.err("for..of over a non-array"));
        }
        self.compile_expr(iterable, Some(Ty::Arr))?;
        let arr = self.fresh(ValType::I32);
        self.emit(Ins::LocalSet(arr));
        let i = self.fresh(ValType::I32);
        self.emit(Ins::I32Const(0));
        self.emit(Ins::LocalSet(i));

        let base = self.depth;
        self.emit(Ins::Block(BlockType::Empty));
        self.emit(Ins::Loop(BlockType::Empty));
        self.depth += 2;
        self.emit(Ins::Block(BlockType::Empty));
        self.depth += 1;
        self.loops.push((base, base + 2));
        // if !(i < len(arr)) break
        self.emit(Ins::LocalGet(i));
        self.emit(Ins::LocalGet(arr));
        self.emit(Ins::I32Load(MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        }));
        self.emit(Ins::I32GeU);
        self.emit(Ins::BrIf(2));
        // v = arr[i]
        self.emit(Ins::LocalGet(arr));
        self.emit(Ins::LocalGet(i));
        self.emit(Ins::I32Const(8));
        self.emit(Ins::I32Mul);
        self.emit(Ins::I32Add);
        self.emit(Ins::F64Load(MemArg {
            offset: 8,
            align: 3,
            memory_index: 0,
        }));
        let v = self.fresh(ValType::F64);
        self.emit(Ins::LocalSet(v));
        self.locals.insert(name.to_string(), (v, Ty::Num));
        self.compile_stmt(body)?;
        self.emit(Ins::End); // $cont
        self.depth -= 1;
        self.emit(Ins::LocalGet(i));
        self.emit(Ins::I32Const(1));
        self.emit(Ins::I32Add);
        self.emit(Ins::LocalSet(i));
        self.emit(Ins::Br(0));
        self.depth -= 2;
        self.emit(Ins::End);
        self.emit(Ins::End);
        self.loops.pop();
        self.locals.remove(name);
        Ok(())
    }

    // ---- expressions ----

    /// Infers an expression's type; arrays must be annotated `@own`.
    fn infer_expr(&self, expr: &Expr) -> Result<Ty, WasmError> {
        Ok(match expr {
            Expr::Num(_) => Ty::Num,
            Expr::Bool(_) => Ty::Bool,
            Expr::Array(_) => Ty::Arr,
            Expr::Ident(name) => self
                .locals
                .get(name)
                .map(|(_, ty)| *ty)
                .ok_or_else(|| self.err(&format!("unknown variable `{name}`")))?,
            Expr::Unary(op, e) => match op {
                UnaryOp::Not => Ty::Bool,
                UnaryOp::Neg => self.infer_expr(e)?,
                UnaryOp::Typeof => return Err(self.err("typeof")),
            },
            Expr::Binary(op, l, r) => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                    self.infer_expr(l)?;
                    self.infer_expr(r)?;
                    Ty::Num
                }
                BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    self.infer_expr(l)?;
                    self.infer_expr(r)?;
                    Ty::Bool
                }
            },
            Expr::Eq(..) => Ty::Bool,
            Expr::Logical(_, l, r) => {
                // JavaScript's `&&`/`||` return the selected operand:
                // result type = operand type (which must match).
                let lt = self.infer_expr(l)?;
                let rt = self.infer_expr(r)?;
                if lt != rt {
                    return Err(self.err("logical operands with different types"));
                }
                lt
            }
            Expr::Ternary(c, t, e) => {
                self.infer_expr(c)?;
                let ty = self.infer_expr(t)?;
                if self.infer_expr(e)? != ty {
                    return Err(self.err("ternary branches with different types"));
                }
                ty
            }
            Expr::Assign(target, value) => match target {
                Target::Ident(name) => {
                    let ty = self.infer_expr(value)?;
                    let existing = self
                        .locals
                        .get(name)
                        .map(|(_, ty)| *ty)
                        .ok_or_else(|| self.err(&format!("unknown variable `{name}`")))?;
                    if existing != ty {
                        return Err(self.err("assignment type mismatch"));
                    }
                    existing
                }
                Target::Index(..) => {
                    self.infer_expr(value)?;
                    Ty::Num
                }
                Target::Member(..) => return Err(self.err("member assignment")),
            },
            Expr::Index(obj, _) => match self.infer_expr(obj)? {
                Ty::Arr => Ty::Num,
                other => return Err(self.err(&format!("indexing a non-array ({other:?})"))),
            },
            Expr::Member(obj, prop) => {
                if prop == "length" && self.infer_expr(obj)? == Ty::Arr {
                    Ty::Num
                } else {
                    return Err(self.err("member access (only arr.length exists)"));
                }
            }
            Expr::Call(callee, args) => {
                // console.log(numeric) is the host import.
                if let Expr::Member(obj_expr, prop) = &**callee {
                    if matches!(**obj_expr, Expr::Ident(ref n) if n == "console") && prop == "log" {
                        for a in args {
                            self.infer_expr(a)?;
                        }
                        return Ok(Ty::Num);
                    }
                }
                let name = match &**callee {
                    Expr::Ident(name) => name,
                    _ => return Err(self.err("non-direct calls")),
                };
                let (idx, arity) = self
                    .signatures
                    .get(name)
                    .ok_or_else(|| self.err(&format!("unknown function `{name}`")))?;
                let _ = idx;
                if args.len() != *arity {
                    return Err(self.err(&format!(
                        "`{name}` called with {} arguments, expected {arity}",
                        args.len()
                    )));
                }
                for a in args {
                    self.infer_expr(a)?;
                }
                Ty::Num
            }
            Expr::Update(_, _, Target::Ident(_)) => Ty::Num,
            Expr::Update(..) => return Err(self.err("++/-- on non-locals")),
            Expr::Arrow(..) | Expr::Fn(..) => return Err(self.err("closures")),
            Expr::Str(_) => return Err(self.err("strings")),
            Expr::Obj(_) => return Err(self.err("objects")),
            Expr::Null | Expr::Undefined => return Err(self.err("null/undefined")),
        })
    }

    /// Compiles an expression, leaving exactly one value of the
    /// inferred type on the stack.
    fn compile_expr(&mut self, expr: &Expr, expect: Option<Ty>) -> Result<Ty, WasmError> {
        let mut ty = self.infer_expr(expr)?;
        if let Some(expect) = expect {
            if ty != expect {
                match (ty, expect) {
                    // JavaScript truthiness: a number conditions an `if`
                    // (zero and NaN are falsy).
                    (Ty::Num, Ty::Bool) => {
                        self.compile_expr(expr, None)?;
                        self.emit_truthiness();
                        return Ok(Ty::Bool);
                    }
                    // A boolean coerces to 1/0 in arithmetic.
                    (Ty::Bool, Ty::Num) => {
                        self.compile_expr(expr, None)?;
                        self.emit(Ins::F64ConvertI32U);
                        return Ok(Ty::Num);
                    }
                    _ => return Err(self.err(&format!("expected {expect:?}, found {ty:?}"))),
                }
            }
        }
        let _ = &mut ty;
        match expr {
            Expr::Num(n) => self.emit(Ins::F64Const((*n).into())),
            Expr::Bool(b) => self.emit(Ins::I32Const(*b as i32)),
            Expr::Ident(name) => {
                let (idx, _) = self.locals[name];
                self.emit(Ins::LocalGet(idx));
            }
            Expr::Array(items) => {
                // ptr = $alloc(len); store each element at ptr+8+8i.
                self.emit(Ins::I32Const(items.len() as i32));
                self.emit(Ins::Call(self.alloc_idx)); // $alloc
                let ptr = self.fresh(ValType::I32);
                self.emit(Ins::LocalSet(ptr));
                for (i, item) in items.iter().enumerate() {
                    self.emit(Ins::LocalGet(ptr));
                    self.compile_expr(item, Some(Ty::Num))?;
                    self.emit(Ins::F64Store(MemArg {
                        offset: (8 + 8 * i) as u64,
                        align: 3,
                        memory_index: 0,
                    }));
                }
                self.emit(Ins::LocalGet(ptr));
            }
            Expr::Unary(op, e) => match op {
                UnaryOp::Not => {
                    self.compile_expr(e, Some(Ty::Bool))?;
                    self.emit(Ins::I32Eqz);
                }
                UnaryOp::Neg => {
                    self.compile_expr(e, Some(Ty::Num))?;
                    self.emit(Ins::F64Neg);
                }
                UnaryOp::Typeof => return Err(self.err("typeof")),
            },
            Expr::Binary(op, l, r) => {
                self.compile_expr(l, Some(Ty::Num))?;
                self.compile_expr(r, Some(Ty::Num))?;
                self.emit(match op {
                    BinOp::Add => Ins::F64Add,
                    BinOp::Sub => Ins::F64Sub,
                    BinOp::Mul => Ins::F64Mul,
                    BinOp::Div => Ins::F64Div,
                    // Wasm has no f64 remainder: a % b = a - trunc(a/b)*b.
                    BinOp::Rem => {
                        let a = self.fresh(ValType::F64);
                        let b = self.fresh(ValType::F64);
                        self.emit(Ins::LocalSet(b));
                        self.emit(Ins::LocalSet(a));
                        self.emit(Ins::LocalGet(a));
                        self.emit(Ins::LocalGet(a));
                        self.emit(Ins::LocalGet(b));
                        self.emit(Ins::F64Div);
                        self.emit(Ins::F64Trunc);
                        self.emit(Ins::LocalGet(b));
                        self.emit(Ins::F64Mul);
                        self.emit(Ins::F64Sub);
                        return Ok(Ty::Num);
                    }
                    BinOp::Lt => Ins::F64Lt,
                    BinOp::Gt => Ins::F64Gt,
                    BinOp::Le => Ins::F64Le,
                    BinOp::Ge => Ins::F64Ge,
                });
            }
            Expr::Eq(op, l, r) => {
                self.compile_expr(l, Some(Ty::Num))?;
                self.compile_expr(r, Some(Ty::Num))?;
                self.emit(Ins::F64Eq);
                if matches!(op, EqOp::LooseNe | EqOp::StrictNe) {
                    self.emit(Ins::I32Eqz);
                }
            }
            Expr::Logical(op, l, r) => {
                // `a && b` returns b when a is truthy, else a; `||`
                // mirrors it. The result type is the operands' type.
                let op_ty = self.infer_expr(l)?;
                self.compile_expr(l, Some(op_ty))?;
                let tmp = self.fresh(op_ty.val());
                self.emit(Ins::LocalSet(tmp));
                // Condition: truthiness of the saved operand; `||`
                // branches when it is FALSY (the If yields b, the Else
                // yields a).
                self.emit(Ins::LocalGet(tmp));
                if op_ty == Ty::Num {
                    self.emit_truthiness();
                }
                if *op == LogicalOp::Or {
                    self.emit(Ins::I32Eqz);
                }
                let block_ty = match op_ty {
                    Ty::Num => BlockType::Result(ValType::F64),
                    _ => BlockType::Result(ValType::I32),
                };
                self.emit(Ins::If(block_ty));
                self.depth += 1;
                self.compile_expr(r, Some(op_ty))?;
                self.emit(Ins::Else);
                self.emit(Ins::LocalGet(tmp));
                self.emit(Ins::End);
                self.depth -= 1;
            }
            Expr::Ternary(cond, then, els) => {
                let ty = self.infer_expr(then)?;
                self.compile_expr(cond, Some(Ty::Bool))?;
                self.emit(Ins::If(BlockType::Result(ty.val())));
                self.depth += 1;
                self.compile_expr(then, Some(ty))?;
                self.emit(Ins::Else);
                self.compile_expr(els, Some(ty))?;
                self.emit(Ins::End);
                self.depth -= 1;
            }
            Expr::Assign(target, value) => match target {
                Target::Ident(name) => {
                    let (idx, ty) = self
                        .locals
                        .get(name)
                        .copied()
                        .ok_or_else(|| self.err("unknown variable"))?;
                    self.compile_expr(value, Some(ty))?;
                    self.emit(Ins::LocalTee(idx));
                }
                Target::Index(obj, idx) => {
                    self.compile_expr(obj, Some(Ty::Arr))?;
                    let t_obj = self.fresh(ValType::I32);
                    self.emit(Ins::LocalSet(t_obj));
                    self.compile_expr(idx, Some(Ty::Num))?;
                    let t_idx = self.fresh(ValType::F64);
                    self.emit(Ins::LocalSet(t_idx));
                    // Bounds: (i as i32) < len, else the store is skipped
                    // and the assigned value is NaN.
                    self.emit(Ins::LocalGet(t_idx));
                    self.emit(Ins::I32TruncF64S);
                    let i32_idx = self.fresh(ValType::I32);
                    self.emit(Ins::LocalTee(i32_idx));
                    self.emit(Ins::LocalGet(t_obj));
                    self.emit(Ins::I32Load(MemArg {
                        offset: 0,
                        align: 2,
                        memory_index: 0,
                    }));
                    self.emit(Ins::I32LtU);
                    self.emit(Ins::If(BlockType::Result(ValType::F64)));
                    self.depth += 1;
                    // The dialect's expressions have no side effects,
                    // so the value is evaluated twice: once to keep as
                    // the result, once for the store ([addr, value]).
                    self.compile_expr(value, Some(Ty::Num))?;
                    self.emit(Ins::LocalGet(t_obj));
                    self.emit(Ins::LocalGet(i32_idx));
                    self.emit(Ins::I32Const(8));
                    self.emit(Ins::I32Mul);
                    self.emit(Ins::I32Add);
                    self.compile_expr(value, Some(Ty::Num))?;
                    self.emit(Ins::F64Store(MemArg {
                        offset: 8,
                        align: 3,
                        memory_index: 0,
                    }));
                    self.emit(Ins::Else);
                    self.emit(Ins::F64Const((f64::NAN).into()));
                    self.emit(Ins::End);
                    self.depth -= 1;
                }
                Target::Member(..) => return Err(self.err("member assignment")),
            },
            Expr::Index(obj, idx) => {
                self.compile_expr(obj, Some(Ty::Arr))?;
                let t_obj = self.fresh(ValType::I32);
                self.emit(Ins::LocalSet(t_obj));
                self.compile_expr(idx, Some(Ty::Num))?;
                let i32_idx = self.fresh(ValType::I32);
                self.emit(Ins::I32TruncF64S);
                self.emit(Ins::LocalTee(i32_idx));
                // idx < len? then load, else NaN (JavaScript's
                // out-of-bounds undefined, as NaN in the numeric
                // dialect).
                self.emit(Ins::LocalGet(t_obj));
                self.emit(Ins::I32Load(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                self.emit(Ins::I32LtU);
                self.emit(Ins::If(BlockType::Result(ValType::F64)));
                self.depth += 1;
                self.emit(Ins::LocalGet(t_obj));
                self.emit(Ins::LocalGet(i32_idx));
                self.emit(Ins::I32Const(8));
                self.emit(Ins::I32Mul);
                self.emit(Ins::I32Add);
                self.emit(Ins::F64Load(MemArg {
                    offset: 8,
                    align: 3,
                    memory_index: 0,
                }));
                self.emit(Ins::Else);
                self.emit(Ins::F64Const((f64::NAN).into()));
                self.emit(Ins::End);
                self.depth -= 1;
            }
            Expr::Member(obj, prop) => {
                if prop != "length" {
                    return Err(self.err("member access (only arr.length exists)"));
                }
                self.compile_expr(obj, Some(Ty::Arr))?;
                self.emit(Ins::I32Load(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                self.emit(Ins::F64ConvertI32U);
            }
            Expr::Call(callee, args) => {
                // console.log(numeric): the host import (function 0).
                // It returns nothing; a zero stands in for undefined so
                // statement position stays balanced.
                if let Expr::Member(obj_expr, prop) = &**callee {
                    if matches!(**obj_expr, Expr::Ident(ref n) if n == "console") && prop == "log" {
                        for a in args {
                            self.compile_expr(a, Some(Ty::Num))?;
                        }
                        self.emit(Ins::Call(args.len() as u32 - 1));
                        self.emit(Ins::F64Const(0.0.into()));
                        return Ok(Ty::Num);
                    }
                }
                let name = match &**callee {
                    Expr::Ident(name) => name.clone(),
                    _ => return Err(self.err("non-direct calls")),
                };
                let (fidx, arity) = self
                    .signatures
                    .get(&name)
                    .copied()
                    .ok_or_else(|| self.err(&format!("unknown function `{name}`")))?;
                if args.len() != arity {
                    return Err(self.err(&format!(
                        "`{name}` called with {} arguments, expected {arity}",
                        args.len()
                    )));
                }
                for a in args {
                    self.compile_expr(a, Some(Ty::Num))?;
                }
                self.emit(Ins::Call(fidx));
            }
            Expr::Update(op, prefix, target) => {
                let name = match target {
                    Target::Ident(n) => n.clone(),
                    _ => return Err(self.err("++/-- on non-locals")),
                };
                let (idx, _) = self
                    .locals
                    .get(&name)
                    .copied()
                    .ok_or_else(|| self.err("unknown variable"))?;
                if !*prefix {
                    // The result is the old value, loaded first; the
                    // store below consumes the new one.
                    self.emit(Ins::LocalGet(idx));
                }
                self.emit(Ins::LocalGet(idx));
                self.emit(Ins::F64Const((1.0).into()));
                self.emit(match op {
                    UpdateOp::Inc => Ins::F64Add,
                    UpdateOp::Dec => Ins::F64Sub,
                });
                if *prefix {
                    // The new value is the result: Tee stores it and
                    // leaves it on the stack.
                    self.emit(Ins::LocalTee(idx));
                } else {
                    // The old value is the result: the store consumes
                    // the new one, leaving [old].
                    self.emit(Ins::LocalSet(idx));
                }
            }
            Expr::Arrow(..) | Expr::Fn(..) => return Err(self.err("closures")),
            Expr::Str(_) => return Err(self.err("strings")),
            Expr::Obj(_) => return Err(self.err("objects")),
            Expr::Null | Expr::Undefined => return Err(self.err("null/undefined")),
        }
        Ok(ty)
    }
}
