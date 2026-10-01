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
    BlockType, CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, Function,
    FunctionSection, GlobalSection, GlobalType, ImportSection, Instruction as Ins, MemArg,
    MemorySection, MemoryType, Module, TypeSection, ValType,
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
    /// `number` / `f64` — the JS-compatible default.
    Num,
    Bool,
    /// `f64[]` — f64 elements, 8-byte stride.
    Arr,
    /// `i32[]` / `u32[]` — i32 elements, 4-byte stride.
    ArrI32,
    Str,
    I32,
    U32,
    I64,
    U64,
    F32,
}

impl Ty {
    /// Whether the type is one of the integer types bitwise ops
    /// require.
    fn is_int(self) -> bool {
        matches!(self, Ty::I32 | Ty::U32 | Ty::I64 | Ty::U64)
    }

    /// For UShr result typing: unsigned shift yields the unsigned
    /// variant when available.
    fn max_int(self, other: Ty) -> Ty {
        let _ = other;
        match self {
            Ty::I32 | Ty::Num => Ty::I32,
            Ty::U32 => Ty::U32,
            Ty::I64 => Ty::I64,
            Ty::U64 => Ty::U64,
            other => other,
        }
    }

    /// The element size in linear memory for arrays of this type
    /// (only Num/`f64` and I32 arrays exist in v1).
    #[allow(dead_code)]
    fn elem_size(self) -> Option<u64> {
        match self {
            Ty::Num => Some(8),
            Ty::I32 | Ty::U32 => Some(4),
            _ => None,
        }
    }
}

impl Ty {
    /// The dialect type of an annotation, if it has one. `any` has no
    /// WASM representation: dynamic values need the GC engines.
    fn from_ann(ann: &TypeAnn) -> Option<Ty> {
        match ann {
            TypeAnn::Num => Some(Ty::Num),
            TypeAnn::Str => Some(Ty::Str),
            TypeAnn::Bool => Some(Ty::Bool),
            TypeAnn::I8 | TypeAnn::U8 | TypeAnn::I16 | TypeAnn::U16 | TypeAnn::I32 => Some(Ty::I32),
            TypeAnn::U32 => Some(Ty::U32),
            TypeAnn::I64 => Some(Ty::I64),
            TypeAnn::U64 => Some(Ty::U64),
            TypeAnn::F32 => Some(Ty::F32),
            TypeAnn::Usize | TypeAnn::Isize => Some(Ty::I32),
            TypeAnn::Array(inner) => match &**inner {
                TypeAnn::Num => Some(Ty::Arr),
                TypeAnn::I8
                | TypeAnn::U8
                | TypeAnn::I16
                | TypeAnn::U16
                | TypeAnn::I32
                | TypeAnn::U32 => Some(Ty::ArrI32),
                _ => None,
            },
            TypeAnn::Any => None,
        }
    }
}

impl Ty {
    fn val(self) -> ValType {
        match self {
            Ty::Num => ValType::F64,
            Ty::F32 => ValType::F32,
            Ty::I64 | Ty::U64 => ValType::I64,
            Ty::Bool | Ty::Arr | Ty::ArrI32 | Ty::Str | Ty::I32 | Ty::U32 => ValType::I32,
        }
    }

    fn zero(self) -> Ins<'static> {
        match self {
            Ty::Num => Ins::F64Const((0.0).into()),
            Ty::F32 => Ins::F32Const((0.0).into()),
            Ty::I64 | Ty::U64 => Ins::I64Const(0),
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
                for d in decls {
                    if let Some(init) = &d.init {
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

    // The log imports: log1..logN (numeric), then logstr. Function
    // index space: log1..logN are 0..N-1, logstr is N, $allocb is N+1,
    // $alloc is N+2, $concat is N+3, user functions follow, $run last.
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

    let logstr_idx: u32 = max_log as u32;
    let allocb_idx: u32 = logstr_idx + 1;
    let alloc_idx: u32 = allocb_idx + 1;
    let concat_idx: u32 = alloc_idx + 1;
    let mut signatures: HashMap<String, (u32, usize)> = HashMap::new();
    for (i, def) in user_fns.iter().enumerate() {
        signatures.insert(
            def.name.clone(),
            (concat_idx + 1 + i as u32, def.params.len()),
        );
    }

    // Static string literals: collected up front so every `I32Const`
    // reference is stable. Layout per literal: i32 byte length at +0,
    // UTF-8 bytes at +8 (the same shape arrays use).
    const DATA_BASE: usize = 1024;
    let mut strings: HashMap<String, usize> = HashMap::new();
    let mut data_blob: Vec<(usize, Vec<u8>)> = Vec::new();
    {
        let mut next = DATA_BASE;
        let mut lits: Vec<String> = Vec::new();
        for item in &*items {
            collect_str_literals(item, &mut lits);
        }
        for lit in lits {
            if strings.contains_key(&lit) {
                continue;
            }
            strings.insert(lit.clone(), next);
            let mut blob = Vec::new();
            blob.extend_from_slice(&(lit.len() as u32).to_le_bytes());
            blob.extend_from_slice(&[0, 0, 0, 0]);
            blob.extend_from_slice(lit.as_bytes());
            data_blob.push((next, blob));
            next += 8 + lit.len();
        }
    }
    let data_end = data_blob
        .iter()
        .map(|(addr, blob)| addr + blob.len())
        .max()
        .unwrap_or(DATA_BASE);
    let arena_start = (data_end + 7) & !7;

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
    let logstr_ty = ty(&mut types, &mut type_ids, vec![ValType::I32], vec![]);
    let allocb_ty = ty(
        &mut types,
        &mut type_ids,
        vec![ValType::I32],
        vec![ValType::I32],
    );
    let alloc_ty = ty(
        &mut types,
        &mut type_ids,
        vec![ValType::I32],
        vec![ValType::I32],
    );
    let concat_ty = ty(
        &mut types,
        &mut type_ids,
        vec![ValType::I32, ValType::I32],
        vec![ValType::I32],
    );

    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();

    // $allocb(bytes: i32) -> i32: raw bump allocation, growing memory
    // as needed. The workhorse under both arrays and strings.
    let mut allocb_fn = Function::new(vec![(2, ValType::I32)]); // 0: bytes, 1: grow result
    for ins in [
        Ins::Block(BlockType::Empty),
        Ins::Loop(BlockType::Empty),
        // fits? (arena + bytes <= memory.size * 65536)
        Ins::GlobalGet(0),
        Ins::LocalGet(0),
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
        // result = old arena; arena += bytes
        Ins::GlobalGet(0),
        Ins::GlobalGet(0),
        Ins::LocalGet(0),
        Ins::I32Add,
        Ins::GlobalSet(0),
        Ins::End, // function body terminator
    ] {
        allocb_fn.instruction(&ins);
    }
    functions.function(allocb_ty);
    code.function(&allocb_fn);

    // $alloc(len: i32) -> i32: element arrays (i32 length header at
    // +0, f64 elements at +8).
    let mut alloc_fn = Function::new(vec![(1, ValType::I32), (1, ValType::I32)]); // 0: len, 1: ptr
    for ins in [
        Ins::LocalGet(0),
        Ins::I32Const(8),
        Ins::I32Mul,
        Ins::I32Const(8),
        Ins::I32Add,
        Ins::Call(allocb_idx),
        Ins::LocalSet(1),
        // header: length at ptr + 0
        Ins::LocalGet(1),
        Ins::LocalGet(0),
        Ins::I32Store(MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        }),
        Ins::LocalGet(1),
        Ins::End, // function body terminator
    ] {
        alloc_fn.instruction(&ins);
    }
    functions.function(alloc_ty);
    code.function(&alloc_fn);

    // $concat(a: i32, b: i32) -> i32: a fresh arena string holding the
    // UTF-8 bytes of both inputs.
    let mut concat_fn = Function::new(vec![(5, ValType::I32)]); // a, b, la, lb, ptr
    for ins in [
        Ins::LocalGet(0),
        Ins::I32Load(MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        }),
        Ins::LocalSet(2),
        Ins::LocalGet(1),
        Ins::I32Load(MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        }),
        Ins::LocalSet(3),
        // ptr = $allocb(8 + la + lb)
        Ins::LocalGet(2),
        Ins::LocalGet(3),
        Ins::I32Add,
        Ins::I32Const(8),
        Ins::I32Add,
        Ins::Call(allocb_idx),
        Ins::LocalSet(4),
        // header: total byte length
        Ins::LocalGet(4),
        Ins::LocalGet(2),
        Ins::LocalGet(3),
        Ins::I32Add,
        Ins::I32Store(MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        }),
        // memory.copy(dst = ptr + 8, src = a + 8, size = la)
        Ins::LocalGet(4),
        Ins::I32Const(8),
        Ins::I32Add,
        Ins::LocalGet(0),
        Ins::I32Const(8),
        Ins::I32Add,
        Ins::LocalGet(2),
        Ins::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        },
        // memory.copy(dst = ptr + 8 + la, src = b + 8, size = lb)
        Ins::LocalGet(4),
        Ins::I32Const(8),
        Ins::I32Add,
        Ins::LocalGet(2),
        Ins::I32Add,
        Ins::LocalGet(1),
        Ins::I32Const(8),
        Ins::I32Add,
        Ins::LocalGet(3),
        Ins::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        },
        Ins::LocalGet(4),
        Ins::End, // function body terminator
    ] {
        concat_fn.instruction(&ins);
    }
    functions.function(concat_ty);
    code.function(&concat_fn);

    // User functions.
    let mut fn_count = concat_idx + 1;
    for def in &user_fns {
        let param_names: Vec<String> = def.params.iter().map(|p| p.name.clone()).collect();
        let mut c = FnCompiler::new(
            &param_names,
            &signatures,
            alloc_idx,
            concat_idx,
            logstr_idx,
            &strings,
        );
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
    let mut run = FnCompiler::new(
        &[],
        &signatures,
        alloc_idx,
        concat_idx,
        logstr_idx,
        &strings,
    );
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
    imports.import(
        "env",
        "logstr",
        wasm_encoder::EntityType::Function(logstr_ty),
    );
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
        &ConstExpr::i32_const(arena_start as i32),
    );
    module.section(&globals);
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("run", ExportKind::Func, fn_count);
    module.section(&exports);
    module.section(&code);
    if !data_blob.is_empty() {
        let mut data = DataSection::new();
        for (addr, blob) in &data_blob {
            data.active(0, &ConstExpr::i32_const(*addr as i32), blob.iter().copied());
        }
        module.section(&data);
    }
    let bytes = module.finish();
    if std::env::var("MEMJS_DEBUG").is_err() {
        validate(&bytes).map_err(|e| WasmError {
            message: format!("internal: emitted module failed validation: {e}"),
        })?;
    }
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
    concat_idx: u32,
    logstr_idx: u32,
    /// Static string literal addresses (data segment), by content.
    strings: &'s HashMap<String, usize>,
    /// Current label depth (blocks, loops, and ifs are labels).
    depth: u32,
    signatures: &'s HashMap<String, (u32, usize)>,
}

impl<'s> FnCompiler<'s> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        params: &[String],
        signatures: &'s HashMap<String, (u32, usize)>,
        alloc_idx: u32,
        concat_idx: u32,
        logstr_idx: u32,
        strings: &'s HashMap<String, usize>,
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
            concat_idx,
            logstr_idx,
            strings,
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

    /// The static data-segment address of a string literal.
    fn string_addr(&self, expr: &Expr) -> Result<usize, WasmError> {
        match expr {
            Expr::Str(text) => self
                .strings
                .get(text)
                .copied()
                .ok_or_else(|| self.err("unresolved string literal")),
            _ => Err(self.err("not a string literal")),
        }
    }

    /// Emits the conversion sequence for a numeric `as` cast. Same-type
    /// casts are no-ops.
    fn emit_cast(&mut self, from: Ty, to: Ty) {
        use Ty::{F32, I32, I64, U32, U64};
        match (from, to) {
            (a, b) if a == b => {}
            // To i32
            (Ty::Num, I32) | (Ty::Num, U32) => self.emit(Ins::I32TruncF64S),
            (F32, I32) | (F32, U32) => self.emit(Ins::I32TruncF32S),
            (I64, I32) | (I64, U32) => self.emit(Ins::I32WrapI64),
            (U64, I32) | (U64, U32) => self.emit(Ins::I32WrapI64),
            // To u32 (zero-extend from i32 where relevant)
            (I32, U32) => {}
            // To i64 / u64
            (I32, I64) => self.emit(Ins::I64ExtendI32S),
            (I32, U64) | (U32, I64) | (U32, U64) => self.emit(Ins::I64ExtendI32U),
            (Ty::Num, I64) | (Ty::Num, U64) => self.emit(Ins::I64TruncF64S),
            (F32, I64) | (F32, U64) => self.emit(Ins::I64TruncF32S),
            (I64, U64) => {}
            // To f64
            (I32, Ty::Num) | (U32, Ty::Num) => self.emit(Ins::F64ConvertI32S),
            (I64, Ty::Num) | (U64, Ty::Num) => self.emit(Ins::F64ConvertI64S),
            (F32, Ty::Num) => self.emit(Ins::F64PromoteF32),
            // To f32
            (Ty::Num, F32) => self.emit(Ins::F32DemoteF64),
            (I32, F32) | (U32, F32) => self.emit(Ins::F32ConvertI32S),
            (I64, F32) | (U64, F32) => self.emit(Ins::F32ConvertI64S),
            _ => {}
        }
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
                for d in decls {
                    // An explicit annotation decides the dialect type:
                    // `any` is rejected (dynamic values need the GC
                    // engines), and the concrete types override
                    // inference.
                    let ann_ty = match &d.ann {
                        Some(ann) => match Ty::from_ann(ann) {
                            Some(ty) => Some(ty),
                            None => {
                                return Err(self.err(
                                    "`any` needs the dynamic engines — the WASM dialect requires concrete types",
                                ))
                            }
                        },
                        None => None,
                    };
                    let (name, init) = (&d.name, &d.init);
                    let ty = match init {
                        Some(init) => match ann_ty {
                            // The annotation decides: compile against
                            // it directly.
                            Some(t) => {
                                self.compile_expr(init, Some(t))?;
                                t
                            }
                            // Unannotated: infer, and require @own for
                            // arrays.
                            None => {
                                let ty = self.infer_expr(init)?;
                                if ty == Ty::Arr && *mem != Mem::Own {
                                    return Err(needs_own());
                                }
                                self.compile_expr(init, Some(ty))?;
                                ty
                            }
                        },
                        None => ann_ty.unwrap_or(Ty::Num),
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
            Expr::Str(_) => Ty::Str,
            Expr::Bit(op, l, r) => {
                // Bitwise ops require integer operands (literals adapt:
                // a Num operand reads as i32 in a bitwise context).
                let lt = match self.infer_expr(l)? {
                    Ty::Num => Ty::I32,
                    other => other,
                };
                let rt = match self.infer_expr(r)? {
                    Ty::Num => Ty::I32,
                    other => other,
                };
                if !lt.is_int() || !rt.is_int() || lt != rt {
                    return Err(self.err(
                        "bitwise operators require matching integer operands (i32/u32/i64/u64)",
                    ));
                }
                match op {
                    BitOp::UShr => Ty::U32.max_int(lt),
                    _ => lt,
                }
            }
            Expr::BitNot(e) => match self.infer_expr(e)? {
                Ty::Num => Ty::I32,
                ty if ty.is_int() => ty,
                _ => return Err(self.err("bitwise not requires an integer operand")),
            },
            Expr::AsCast(cast) => {
                self.infer_expr(&cast.expr)?;
                Ty::from_ann(&cast.ann)
                    .ok_or_else(|| self.err(&format!("cannot cast to `{}`", cast.ann.name())))?
            }
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
                BinOp::Add => {
                    // String concatenation: `+` with a string on either
                    // side requires both sides to be strings and yields
                    // a string.
                    let lt = self.infer_expr(l)?;
                    let rt = self.infer_expr(r)?;
                    if lt == Ty::Str || rt == Ty::Str {
                        if lt != Ty::Str || rt != Ty::Str {
                            return Err(
                                self.err("`+` between a string and a number (convert explicitly)")
                            );
                        }
                        Ty::Str
                    } else {
                        Ty::Num
                    }
                }
                BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
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
                let obj_ty = self.infer_expr(obj)?;
                if prop == "length" && matches!(obj_ty, Ty::Arr | Ty::Str) {
                    Ty::Num
                } else {
                    return Err(self.err("member access (only .length exists)"));
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
                    // An integral literal adapts to the expected integer
                    // or half-width float type (`let x: i32 = 5`).
                    (Ty::Num, Ty::I32) | (Ty::Num, Ty::U32) if matches!(expr, Expr::Num(n) if n.fract() == 0.0 && *n >= i32::MIN as f64 && *n <= u32::MAX as f64) =>
                    {
                        let v = match expr {
                            Expr::Num(n) => *n as i32,
                            _ => unreachable!(),
                        };
                        self.emit(Ins::I32Const(v));
                        return Ok(expect);
                    }
                    (Ty::Num, Ty::I64) | (Ty::Num, Ty::U64) if matches!(expr, Expr::Num(n) if n.fract() == 0.0) =>
                    {
                        let v = match expr {
                            Expr::Num(n) => *n as i64,
                            _ => unreachable!(),
                        };
                        self.emit(Ins::I64Const(v));
                        return Ok(expect);
                    }
                    (Ty::Num, Ty::F32) if matches!(expr, Expr::Num(_)) => {
                        self.compile_expr(expr, Some(Ty::Num))?;
                        self.emit(Ins::F32DemoteF64);
                        return Ok(Ty::F32);
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
            Expr::Binary(_op @ BinOp::Add, l, r) => {
                let lt = self.infer_expr(l)?;
                let rt = self.infer_expr(r)?;
                if lt == Ty::Str && rt == Ty::Str {
                    // concat: $concat(a, b) -> ptr (a fresh arena copy).
                    self.compile_expr(l, Some(Ty::Str))?;
                    self.compile_expr(r, Some(Ty::Str))?;
                    self.emit(Ins::Call(self.concat_idx));
                    return Ok(Ty::Str);
                }
                self.compile_expr(l, Some(Ty::Num))?;
                self.compile_expr(r, Some(Ty::Num))?;
                self.emit(Ins::F64Add);
            }
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
            Expr::Bit(op, l, r) => {
                let lt = match self.infer_expr(l)? {
                    Ty::Num => Ty::I32,
                    other => other,
                };
                self.compile_expr(l, Some(lt))?;
                self.compile_expr(r, Some(lt))?;
                match (lt, op) {
                    (Ty::I32 | Ty::U32, BitOp::And) => self.emit(Ins::I32And),
                    (Ty::I32 | Ty::U32, BitOp::Or) => self.emit(Ins::I32Or),
                    (Ty::I32 | Ty::U32, BitOp::Xor) => self.emit(Ins::I32Xor),
                    (Ty::I32 | Ty::U32, BitOp::Shl) => self.emit(Ins::I32Shl),
                    (Ty::I32, BitOp::Shr) => self.emit(Ins::I32ShrS),
                    (Ty::U32, BitOp::Shr) => self.emit(Ins::I32ShrU),
                    (Ty::I32 | Ty::U32, BitOp::UShr) => self.emit(Ins::I32ShrU),
                    (Ty::I64 | Ty::U64, BitOp::And) => self.emit(Ins::I64And),
                    (Ty::I64 | Ty::U64, BitOp::Or) => self.emit(Ins::I64Or),
                    (Ty::I64 | Ty::U64, BitOp::Xor) => self.emit(Ins::I64Xor),
                    (Ty::I64 | Ty::U64, BitOp::Shl) => self.emit(Ins::I64Shl),
                    (Ty::I64, BitOp::Shr) => self.emit(Ins::I64ShrS),
                    (Ty::U64, BitOp::Shr) => self.emit(Ins::I64ShrU),
                    (Ty::I64 | Ty::U64, BitOp::UShr) => self.emit(Ins::I64ShrU),
                    (ty, _) => {
                        return Err(self.err(&format!(
                            "bitwise operators require integer operands, found {ty:?}"
                        )))
                    }
                }
            }
            Expr::BitNot(e) => {
                let ty = self.infer_expr(e)?;
                self.compile_expr(e, Some(ty))?;
                match ty {
                    Ty::I32 | Ty::U32 => {
                        self.emit(Ins::I32Const(-1));
                        self.emit(Ins::I32Xor);
                    }
                    Ty::I64 | Ty::U64 => {
                        self.emit(Ins::I64Const(-1));
                        self.emit(Ins::I64Xor);
                    }
                    _ => return Err(self.err("bitwise not requires an integer operand")),
                }
            }
            Expr::AsCast(cast) => {
                let from = self.infer_expr(&cast.expr)?;
                let to = Ty::from_ann(&cast.ann)
                    .ok_or_else(|| self.err(&format!("cannot cast to `{}`", cast.ann.name())))?;
                self.compile_expr(&cast.expr, Some(from))?;
                self.emit_cast(from, to);
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
            Expr::Member(obj, _prop) => {
                // .length: byte length for strings, element count for
                // arrays. Numeric in both cases.
                let obj_ty = self.infer_expr(obj)?;
                self.compile_expr(obj, Some(obj_ty))?;
                self.emit(Ins::I32Load(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                self.emit(Ins::F64ConvertI32U);
            }
            Expr::Call(callee, args) => {
                // console.log: all-numeric args go to log1..logN; a
                // single string goes to logstr (the host decodes it
                // from linear memory). Mixed calls are a v1 error.
                // Integer-typed args coerce to f64 at the log boundary.
                if let Expr::Member(obj_expr, prop) = &**callee {
                    if matches!(**obj_expr, Expr::Ident(ref n) if n == "console") && prop == "log" {
                        let mut all_num = true;
                        let mut all_str = true;
                        let mut int_tys: Vec<Ty> = Vec::new();
                        for a in args {
                            match self.infer_expr(a)? {
                                Ty::Num => all_str = false,
                                Ty::Str => all_num = false,
                                ty if ty.is_int() => {
                                    all_str = false;
                                    int_tys.push(ty);
                                }
                                Ty::F32 => {
                                    all_str = false;
                                    int_tys.push(Ty::F32);
                                }
                                _ => {
                                    return Err(self
                                        .err("console.log arguments must be numbers or strings"))
                                }
                            }
                        }
                        if all_str {
                            if args.len() != 1 {
                                return Err(self.err("log one string per call in the WASM dialect"));
                            }
                            self.compile_expr(&args[0], Some(Ty::Str))?;
                            self.emit(Ins::Call(self.logstr_idx));
                            self.emit(Ins::F64Const(0.0.into()));
                            return Ok(Ty::Num);
                        }
                        if !all_num {
                            return Err(
                                self.err("console.log cannot mix strings and numbers in one call")
                            );
                        }
                        for (i, a) in args.iter().enumerate() {
                            self.compile_expr(a, None)?;
                            match int_tys.get(i) {
                                Some(Ty::I32 | Ty::U32) => self.emit(Ins::F64ConvertI32S),
                                Some(Ty::I64 | Ty::U64) => self.emit(Ins::F64ConvertI64S),
                                Some(Ty::F32) => self.emit(Ins::F64PromoteF32),
                                _ => {}
                            }
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
            Expr::Str(_) => {
                // Static literal: its address lives in the data segment
                // (i32 constant), collected by the module assembler.
                let addr = self.string_addr(expr)?;
                self.emit(Ins::I32Const(addr as i32));
            }
            Expr::Obj(_) => return Err(self.err("objects")),
            Expr::Null | Expr::Undefined => return Err(self.err("null/undefined")),
        }
        Ok(ty)
    }
}

/// Collects every string literal in a program item, in source order,
/// deduplicated by content.
fn collect_str_literals(item: &Item, out: &mut Vec<String>) {
    fn push_str(expr: &Expr, out: &mut Vec<String>) {
        if let Expr::Str(text) = expr {
            if !out.contains(text) {
                out.push(text.clone());
            }
        }
    }
    fn walk_expr(expr: &Expr, out: &mut Vec<String>) {
        match expr {
            Expr::Str(_) => push_str(expr, out),
            Expr::Array(items) => items.iter().for_each(|e| walk_expr(e, out)),
            Expr::Obj(entries) => entries.iter().for_each(|e| walk_expr(&e.value, out)),
            Expr::Unary(_, e) => walk_expr(e, out),
            Expr::Binary(_, l, r) | Expr::Logical(_, l, r) | Expr::Eq(_, l, r) => {
                walk_expr(l, out);
                walk_expr(r, out);
            }
            Expr::Assign(target, value) => {
                match target {
                    Target::Ident(_) => {}
                    Target::Index(obj, idx) => {
                        walk_expr(obj, out);
                        walk_expr(idx, out);
                    }
                    Target::Member(obj, _) => walk_expr(obj, out),
                }
                walk_expr(value, out);
            }
            Expr::Index(obj, idx) => {
                walk_expr(obj, out);
                walk_expr(idx, out);
            }
            Expr::Member(obj, _) => walk_expr(obj, out),
            Expr::Call(callee, args) => {
                walk_expr(callee, out);
                args.iter().for_each(|a| walk_expr(a, out));
            }
            Expr::Ternary(c, t, e) => {
                walk_expr(c, out);
                walk_expr(t, out);
                walk_expr(e, out);
            }
            Expr::Update(_, _, target) => match target {
                Target::Ident(_) => {}
                Target::Index(obj, idx) => {
                    walk_expr(obj, out);
                    walk_expr(idx, out);
                }
                Target::Member(obj, _) => walk_expr(obj, out),
            },
            _ => {}
        }
    }
    fn walk_stmt(stmt: &Stmt, out: &mut Vec<String>) {
        match stmt {
            Stmt::Let { decls, .. } | Stmt::Var { decls, .. } => {
                for d in decls {
                    if let Some(init) = &d.init {
                        walk_expr(init, out);
                    }
                }
            }
            Stmt::Expr(expr) => walk_expr(expr, out),
            Stmt::If(cond, then, els) => {
                walk_expr(cond, out);
                walk_stmt(then, out);
                if let Some(els) = els {
                    walk_stmt(els, out);
                }
            }
            Stmt::While(cond, body) => {
                walk_expr(cond, out);
                walk_stmt(body, out);
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                if let Some(init) = init {
                    walk_stmt(init, out);
                }
                if let Some(cond) = cond {
                    walk_expr(cond, out);
                }
                if let Some(step) = step {
                    walk_expr(step, out);
                }
                walk_stmt(body, out);
            }
            Stmt::ForOf { iterable, body, .. } | Stmt::ForIn { iterable, body, .. } => {
                walk_expr(iterable, out);
                walk_stmt(body, out);
            }
            Stmt::Block(stmts) => stmts.iter().for_each(|s| walk_stmt(s, out)),
            Stmt::Return(Some(expr)) => walk_expr(expr, out),
            _ => {}
        }
    }
    match item {
        Item::Fn(def) => {
            for stmt in &def.body {
                walk_stmt(stmt, out);
            }
        }
        Item::Stmt(stmt) => walk_stmt(stmt, out),
    }
}
