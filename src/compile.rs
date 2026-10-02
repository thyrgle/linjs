//! The bytecode compiler: AST to instruction sequences.
//!
//! This is the structural rehearsal for a future WASM backend. The
//! compilation makes explicit everything the tree-walking interpreter
//! hides in recursion:
//!
//! * **Slots** — locals live at frame indices, resolved at compile time
//!   through nested block scopes (shadowing gets fresh slots).
//! * **Cells** — locals captured by a nested closure are boxed into
//!   shared cells at compile time, so closures hold `Rc` handles to the
//!   binding itself and mutation stays shared, as JavaScript requires.
//! * **Per-iteration loop variables** — a captured `for (let i ...)`
//!   variable gets a fresh cell each iteration, with the control slot
//!   synced back after the body, matching JavaScript exactly.
//! * **Frames** — every function is a [`FuncProto`]; calls push frames;
//!   return pops the frame *and its arena*, which makes `@own` teardown
//!   structural rather than simulated.
//!
//! Top-level code compiles into a `main` proto whose declarations live
//! in the global table, so functions resolve top-level names as
//! globals.
//!
//! V1 limits (documented): a closure cannot capture a variable from a
//! *grandparent* frame through an intermediate function (pass-through
//! upvalues), and top-level declarations are flat globals (no
//! top-level block shadowing).

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::*;
use crate::interp::Item;

/// One compiled function body.
pub struct FuncProto {
    pub name: Option<String>,
    /// Parameter count.
    pub param_count: usize,
    /// The slot index of each parameter (a captured param shares its
    /// capture slot).
    pub param_slots: Vec<u16>,
    /// Total frame slots (params, locals, hidden temporaries).
    pub slot_count: usize,
    pub code: Vec<Op>,
    /// String/number constants, addressed by index.
    pub constants: Vec<Const>,
    /// Nested function protos, instantiated by `Op::Closure`.
    pub protos: Vec<Rc<FuncProto>>,
    /// For `Op::Closure[i]`: the frame slots the nested proto captures.
    pub captures: Vec<Vec<u16>>,
    /// Slots that hold shared cells (captured locals); `Declare` and
    /// parameter binding box values into cells for these.
    pub cell_slots: Vec<u16>,
    /// Slots declared `const`: assignment through them is an error.
    pub const_slots: Vec<u16>,
    /// Slot names, for diagnostics (use-after-move messages).
    pub slot_names: Vec<(u16, String)>,
}

/// A compile-time constant.
#[derive(Debug, Clone)]
pub enum Const {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
}

/// One instruction. Operands are indices, never names: name resolution
/// happened at compile time.
#[derive(Debug, Clone)]
pub enum Op {
    Const(u32),
    GetLocal(u16),
    /// Store the top of the stack into a slot; the value stays on the
    /// stack (assignment expressions have values).
    SetLocal(u16),
    GetCell(u16),
    SetCell(u16),
    GetGlobal(u32),
    SetGlobal(u32),
    /// Duplicate the top of the stack.
    Dup,
    /// Instantiate a nested proto; captures are read from this frame.
    Closure(u32),
    /// Pop `args` values, then the callee; call it.
    Call {
        args: u16,
    },
    /// `console.log`: pop `args` values, print Node-style.
    Log {
        args: u16,
    },
    /// Method call with a receiver below the args on the stack
    /// (`recv, argN..arg0`); dispatches array methods.
    MethodCall {
        key: u32,
        args: u16,
    },
    Return,
    Pop,
    /// Pop `count` values, push an array.
    Array(u16),
    /// Pop `count` values; the keys are `count` constants starting at
    /// `first_key`. Push an object.
    Object {
        count: u16,
        first_key: u32,
    },
    /// Pop index, pop object; push the result.
    IndexGet,
    /// Pop value, pop index, pop object; store; push the value.
    IndexSet,
    /// Pop object; push `object.key`.
    MemberGet(u32),
    /// Pop value, pop object; store; push the value.
    MemberSet(u32),
    Bin(BinOp),
    Eq(EqOp),
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    BitNot,
    Not,
    Neg,
    Typeof,
    /// Pop the condition; jump if falsy.
    JumpIfFalse(u32),
    /// Peek the condition; jump if falsy (value stays).
    JumpIfFalseKeep(u32),
    /// Peek the condition; jump if truthy (value stays).
    JumpIfTrueKeep(u32),
    Jump(u32),
    /// Pop the iterable; push an iterator over values (for-of) or
    /// keys (for-in).
    MakeIter {
        keys: bool,
    },
    /// Peek the iterator; if exhausted jump to `done`, else pop nothing
    /// and push the next value or key.
    IterNext(u32),
    /// Pop the initializer; declare the single local in `slot` under
    /// the declaration's memory mode (arena allocation, borrow tags,
    /// the no-unannotated-own rule).
    Declare {
        slot: u16,
        mem: Mem,
    },
    /// Assignment to a local: store the top of the stack with the
    /// memory checks (an @own value may only land in an @own binding).
    SetChecked {
        slot: u16,
    },
    /// `@own` move out of a local: push its value and write the moved
    /// sentinel into the slot.
    MoveFrom(u16),
    /// The same through the global table: push the global's value and
    /// mark the global moved.
    MoveGlobalFrom(u32),
    /// Create a fresh cell in `to` holding the value of `from` — the
    /// per-iteration binding of a captured `for (let ...)`.
    FreshCell {
        from: u16,
        to: u16,
    },
    /// Index access through temporary slots (so `obj` and `idx` are
    /// evaluated exactly once): reads `slots[obj]`/`slots[idx]`.
    IndexGetTemp {
        obj: u16,
        idx: u16,
    },
    /// Pop the value; store into `slots[obj][slots[idx]]`; push it.
    IndexSetTemp {
        obj: u16,
        idx: u16,
    },
    /// Pop object to `slots[obj]`; push `slots[obj].key`.
    MemberGetTemp {
        obj: u16,
        key: u32,
    },
    /// Pop value to store, pop object to `slots[obj]`; store; push.
    MemberSetTemp {
        obj: u16,
        key: u32,
    },
    /// End of the main frame.
    Halt,
}

/// A compile failure (should be unreachable for programs that parsed).
#[derive(Debug, Clone, PartialEq)]
pub struct CompileError {
    pub message: String,
}

type CResult = Result<(), CompileError>;

/// A compiled program: one outer proto. Top-level statements run in
/// its frame; top-level function declarations are nested protos
/// instantiated (and stored as globals) as main executes, so they
/// capture main's locals as cells exactly like any closure.
pub struct Program {
    pub main: Rc<FuncProto>,
}

#[derive(Clone, Copy)]
struct Slot {
    idx: u16,
    cell: bool,
    mem: Mem,
}

struct Scope {
    names: HashMap<String, Slot>,
}

/// Where an enclosing `break`/`continue` should jump.
#[derive(Clone)]
struct LoopCtx {
    /// Patch site for `break` (the loop's exit).
    break_patch: usize,
    /// Target for `continue` (the step/condition re-entry); known only
    /// after the body compiles, so `continue` sites are collected and
    /// patched.
    continue_target: u32,
    continue_patches: Vec<usize>,
}

struct FnCompiler {
    name: Option<String>,
    param_count: usize,
    param_slots: Vec<u16>,
    scopes: Vec<Scope>,
    loops: Vec<LoopCtx>,
    next_slot: usize,
    slot_count: usize,
    code: Vec<Op>,
    constants: Vec<Const>,
    const_index: HashMap<ConstKey, u32>,
    protos: Vec<Rc<FuncProto>>,
    captures: Vec<Vec<u16>>,
    /// Names in this frame captured by any nested closure (pre-scanned
    /// before compilation so slot allocation sees them all).
    captured: HashSet<String>,
    /// Patch sites of `break` jumps waiting for their loop's exit.
    pending_breaks: Vec<usize>,
    /// Slots allocated as shared cells.
    cell_slots: Vec<u16>,
    /// Slots declared `const`.
    const_slots: Vec<u16>,
    /// Names for diagnostics.
    slot_names: Vec<(u16, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ConstKey {
    Num(u64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
}

/// Compiles a parsed program to bytecode.
pub fn compile(items: &[Item]) -> Result<Program, CompileError> {
    // One compiler for the whole program. Top-level function
    // declarations compile like nested functions: a Closure op (which
    // captures main's locals as cells) followed by a global store, in
    // source order.
    // Pre-mark captures for the main frame: every name any nested
    // function (at any depth) reaches for.
    let top_stmts: Vec<Stmt> = items
        .iter()
        .filter_map(|item| match item {
            Item::Stmt(stmt) => Some(stmt.clone()),
            Item::Fn(_) => None,
        })
        .collect();
    let mut main = FnCompiler::new(None, &[]);
    main.captured = all_nested_captures(&top_stmts);
    for item in items {
        if let Item::Fn(def) = item {
            // The function's free variables are exactly what it will
            // capture from main's frame.
            let param_names: Vec<String> = def.params.iter().map(|p| p.name.clone()).collect();
            main.captured
                .extend(free_vars(&param_names, &FnBody::Block(def.body.clone())));
        }
    }
    // Top-level function names live in the global table — references
    // to them resolve as globals, never as captures (this is what
    // makes recursion and mutual recursion work).
    let fn_names: HashSet<String> = items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(def) => Some(def.name.clone()),
            Item::Stmt(_) => None,
        })
        .collect();
    main.captured.retain(|n| !fn_names.contains(n));
    let main_captures: HashSet<String> = main.captured.clone();
    main.predeclare_captured(&main_captures);
    // Function declarations hoist: their closures bind before any
    // statement runs.
    for item in items {
        if let Item::Fn(def) = item {
            let param_names: Vec<String> = def.params.iter().map(|p| p.name.clone()).collect();
            main.compile_nested(
                Some(&def.name),
                &param_names,
                &FnBody::Block(def.body.clone()),
            )?;
            let name_idx = main.str_const(&def.name);
            main.emit(Op::SetGlobal(name_idx));
        }
    }
    for item in items {
        if let Item::Stmt(stmt) = item {
            main.compile_stmt(stmt)?;
        }
    }
    main.emit(Op::Halt);
    Ok(Program {
        main: main.finish(),
    })
}

impl FnCompiler {
    fn new(name: Option<&str>, params: &[String]) -> Self {
        Self::new_with_captures(name, &[], params)
    }

    /// Capture slots come first (the VM fills them from the closure's
    /// cells), then parameters, then locals.
    fn new_with_captures(name: Option<&str>, captures: &[String], params: &[String]) -> Self {
        let mut scopes = vec![Scope {
            names: HashMap::new(),
        }];
        let mut next_slot = 0usize;
        let mut cell_slots = Vec::new();
        let mut param_slots = Vec::new();
        for c in captures {
            let idx = next_slot as u16;
            scopes[0].names.insert(
                c.clone(),
                Slot {
                    idx,
                    cell: true,
                    mem: Mem::Gc,
                },
            );
            cell_slots.push(idx);
            next_slot += 1;
        }
        for p in params {
            // A captured parameter reuses its capture slot: one binding,
            // boxed.
            if let Some(slot) = scopes[0].names.get(p) {
                param_slots.push(slot.idx);
                continue;
            }
            let idx = next_slot as u16;
            scopes[0].names.insert(
                p.clone(),
                Slot {
                    idx,
                    cell: false,
                    mem: Mem::Gc,
                },
            );
            param_slots.push(idx);
            next_slot += 1;
        }
        FnCompiler {
            name: name.map(|n| n.to_string()),
            param_count: params.len(),
            param_slots,
            scopes,
            loops: Vec::new(),
            next_slot,
            slot_count: 0,
            code: Vec::new(),
            constants: Vec::new(),
            const_index: HashMap::new(),
            protos: Vec::new(),
            captures: Vec::new(),
            captured: HashSet::new(),
            pending_breaks: Vec::new(),
            cell_slots,
            const_slots: Vec::new(),
            slot_names: Vec::new(),
        }
    }

    fn const_null(&mut self) -> u32 {
        self.constant(ConstKey::Null, || Const::Null)
    }

    fn emit(&mut self, op: Op) {
        self.code.push(op);
    }

    fn err(&self, message: &str) -> CompileError {
        CompileError {
            message: message.to_string(),
        }
    }

    fn finish(mut self) -> Rc<FuncProto> {
        self.slot_count = self.slot_count.max(self.next_slot);
        Rc::new(FuncProto {
            name: self.name,
            param_count: self.param_count,
            param_slots: self.param_slots,
            slot_count: self.slot_count,
            code: self.code,
            constants: self.constants,
            protos: self.protos,
            captures: self.captures,
            cell_slots: self.cell_slots,
            const_slots: self.const_slots,
            slot_names: self.slot_names,
        })
    }

    fn constant(&mut self, key: ConstKey, make: impl FnOnce() -> Const) -> u32 {
        if let Some(idx) = self.const_index.get(&key) {
            return *idx;
        }
        let idx = self.constants.len() as u32;
        self.constants.push(make());
        self.const_index.insert(key, idx);
        idx
    }

    fn num_const(&mut self, n: f64) -> u32 {
        self.constant(ConstKey::Num(n.to_bits()), || Const::Num(n))
    }

    fn str_const(&mut self, s: &str) -> u32 {
        self.constant(ConstKey::Str(s.to_string()), || Const::Str(s.to_string()))
    }

    // ---- scopes ----

    fn push_scope(&mut self) {
        self.scopes.push(Scope {
            names: HashMap::new(),
        });
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Declares a local that is never boxed, even when captured — the
    /// control slot of a `for (let ...)` loop, whose per-iteration
    /// cell lives in the body scope.
    /// Declares a local that is always a shared cell (the
    /// per-iteration binding of a captured `for (let ...)`).
    fn declare_forced_cell(&mut self, name: &str, mem: Mem) -> Slot {
        let idx = self.next_slot as u16;
        self.next_slot += 1;
        self.cell_slots.push(idx);
        let slot = Slot {
            idx,
            cell: true,
            mem,
        };
        self.scopes
            .last_mut()
            .unwrap()
            .names
            .insert(name.to_string(), slot);
        slot
    }

    fn declare_plain(&mut self, name: &str, mem: Mem) -> Slot {
        let idx = self.next_slot as u16;
        self.next_slot += 1;
        let slot = Slot {
            idx,
            cell: false,
            mem,
        };
        self.scopes
            .last_mut()
            .unwrap()
            .names
            .insert(name.to_string(), slot);
        slot
    }

    /// Reserves cell slots for names nested functions will capture, so
    /// hoisted closures can reference them before their declarations
    /// compile. Later declarations of the same name reuse the slot.
    fn predeclare_captured(&mut self, names: &HashSet<String>) {
        for name in names {
            let idx = self.next_slot as u16;
            self.next_slot += 1;
            self.cell_slots.push(idx);
            self.slot_names.push((idx, name.clone()));
            self.scopes[0].names.insert(
                name.clone(),
                Slot {
                    idx,
                    cell: true,
                    mem: Mem::Gc,
                },
            );
        }
    }

    fn declare(&mut self, name: &str, mem: Mem, is_const: bool) -> Slot {
        // A pre-reserved (or earlier) declaration in this scope owns
        // the slot already.
        if let Some(slot) = self.scopes.last().unwrap().names.get(name) {
            return *slot;
        }
        let idx = self.next_slot as u16;
        self.next_slot += 1;
        let cell = self.captured.contains(name);
        if cell {
            self.cell_slots.push(idx);
        }
        if is_const {
            self.const_slots.push(idx);
        }
        self.slot_names.push((idx, name.to_string()));
        let slot = Slot { idx, cell, mem };
        self.scopes
            .last_mut()
            .unwrap()
            .names
            .insert(name.to_string(), slot);
        slot
    }

    fn fresh_temp(&mut self) -> Slot {
        let idx = self.next_slot as u16;
        self.next_slot += 1;
        Slot {
            idx,
            cell: false,
            mem: Mem::Gc,
        }
    }

    fn resolve(&self, name: &str) -> Option<Slot> {
        for scope in self.scopes.iter().rev() {
            if let Some(slot) = scope.names.get(name) {
                return Some(*slot);
            }
        }
        None
    }

    fn emit_load(&mut self, slot: Slot) {
        self.emit(if slot.cell {
            Op::GetCell(slot.idx)
        } else {
            Op::GetLocal(slot.idx)
        });
    }

    /// Stores the stack top into a slot; the value stays on the stack.
    fn emit_store(&mut self, slot: Slot) {
        self.emit(if slot.cell {
            Op::SetCell(slot.idx)
        } else {
            Op::SetLocal(slot.idx)
        });
    }

    // ---- statements ----

    fn compile_stmts(&mut self, stmts: &[Stmt]) -> CResult {
        for stmt in stmts {
            self.compile_stmt(stmt)?;
        }
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> CResult {
        match stmt {
            Stmt::Empty | Stmt::FnDecl(..) => {}
            Stmt::Let {
                decls,
                mem,
                is_const,
                ..
            } => {
                for d in decls {
                    let (name, init) = (&d.name, &d.init);
                    let slot = self.declare(name, *mem, *is_const);
                    match init {
                        Some(init) => {
                            // A move from another local compiles to an
                            // explicit move plus the declaration;
                            // everything else evaluates then declares
                            // under the mode.
                            if let (Mem::Own, Expr::Ident(source)) = (*mem, init) {
                                if let Some(src) = self.resolve(source) {
                                    self.emit(Op::MoveFrom(src.idx));
                                    self.emit(Op::Declare {
                                        slot: slot.idx,
                                        mem: *mem,
                                    });
                                    continue;
                                }
                                // Not a local: a global (or an error the
                                // VM reports on lookup).
                                let idx = self.str_const(source);
                                self.emit(Op::MoveGlobalFrom(idx));
                                self.emit(Op::Declare {
                                    slot: slot.idx,
                                    mem: *mem,
                                });
                                continue;
                            }
                            self.compile_expr(init)?;
                            self.emit(Op::Declare {
                                slot: slot.idx,
                                mem: *mem,
                            });
                        }
                        None => {
                            let undef = self.constant(ConstKey::Undefined, || Const::Undefined);
                            self.emit(Op::Const(undef));
                            self.emit(Op::Declare {
                                slot: slot.idx,
                                mem: *mem,
                            });
                        }
                    }
                }
            }
            Stmt::Var { decls, mem } => {
                for d in decls {
                    let (name, init) = (&d.name, &d.init);
                    // `var` is function-scoped: reuse the function
                    // frame's slot if the name is already there.
                    let slot = if let Some(slot) = self.scopes[0].names.get(name) {
                        *slot
                    } else {
                        let idx = self.next_slot as u16;
                        self.next_slot += 1;
                        let slot = Slot {
                            idx,
                            cell: self.captured.contains(name),
                            mem: *mem,
                        };
                        if slot.cell {
                            self.cell_slots.push(idx);
                        }
                        self.slot_names.push((idx, name.clone()));
                        self.scopes[0].names.insert(name.clone(), slot);
                        slot
                    };
                    match init {
                        Some(init) => {
                            if let (Mem::Own, Expr::Ident(source)) = (*mem, init) {
                                if let Some(src) = self.resolve(source) {
                                    self.emit(Op::MoveFrom(src.idx));
                                    self.emit(Op::Declare {
                                        slot: slot.idx,
                                        mem: *mem,
                                    });
                                    continue;
                                }
                            }
                            self.compile_expr(init)?;
                            self.emit(Op::Declare {
                                slot: slot.idx,
                                mem: *mem,
                            });
                        }
                        None => {
                            let undef = self.constant(ConstKey::Undefined, || Const::Undefined);
                            self.emit(Op::Const(undef));
                            self.emit(Op::Declare {
                                slot: slot.idx,
                                mem: *mem,
                            });
                        }
                    }
                }
            }
            Stmt::Expr(expr) => {
                self.compile_expr(expr)?;
                self.emit(Op::Pop);
            }
            Stmt::If(cond, then, els) => {
                self.compile_expr(cond)?;
                let jump = self.code.len();
                self.emit(Op::JumpIfFalse(0));
                self.compile_stmt(then)?;
                if let Some(els) = els {
                    let else_jump = self.code.len();
                    self.emit(Op::Jump(0));
                    let then_end = self.code.len() as u32;
                    self.code[jump] = Op::JumpIfFalse(then_end);
                    self.compile_stmt(els)?;
                    let end = self.code.len() as u32;
                    self.code[else_jump] = Op::Jump(end);
                } else {
                    let then_end = self.code.len() as u32;
                    self.code[jump] = Op::JumpIfFalse(then_end);
                }
            }
            Stmt::While(cond, body) => {
                let loop_top = self.code.len() as u32;
                self.compile_expr(cond)?;
                let jump = self.code.len();
                self.emit(Op::JumpIfFalse(0));
                self.loops.push(LoopCtx {
                    break_patch: jump,
                    continue_target: loop_top,
                    continue_patches: Vec::new(),
                });
                self.compile_stmt(body)?;
                let ctx = self.loops.pop().unwrap();
                self.emit(Op::Jump(loop_top));
                let end = self.code.len() as u32;
                self.code[ctx.break_patch] = Op::JumpIfFalse(end);
                self.patch_breaks(&ctx, end);
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                self.push_scope();
                // A captured `for (let v ...)`: the loop condition and
                // step run on a plain control slot; the body sees a
                // fresh per-iteration cell that closures capture.
                let mut per_iter: Option<(Slot, String)> = None;
                let captured_for_let = match init.as_deref() {
                    Some(Stmt::Let {
                        decls,
                        mem: Mem::Gc,
                        ..
                    }) => decls.len() == 1 && self.captured.contains(&decls[0].name),
                    _ => false,
                };
                if captured_for_let {
                    // Force-plain control slot, then re-declare the name
                    // as the body's per-iteration cell.
                    let name = match init.as_deref() {
                        Some(Stmt::Let { decls, .. }) => decls[0].name.clone(),
                        _ => unreachable!(),
                    };
                    let control = {
                        let saved = std::mem::take(&mut self.captured);
                        let slot = self.declare_plain(&name, Mem::Gc);
                        self.captured = saved;
                        slot
                    };
                    if let Some(init) = init {
                        self.compile_init_values(init, &[control])?;
                    }
                    per_iter = Some((control, name));
                } else if let Some(init) = init {
                    self.compile_stmt(init)?;
                }
                self.compile_for_rest(cond, step, body, per_iter)?;
                self.pop_scope();
            }
            Stmt::ForOf {
                name,
                is_const: _,
                iterable,
                body,
            } => {
                self.compile_for_iter(name, iterable, body, false)?;
            }
            Stmt::ForIn {
                name,
                is_const: _,
                iterable,
                body,
            } => {
                self.compile_for_iter(name, iterable, body, true)?;
            }
            Stmt::Block(stmts) => {
                self.push_scope();
                self.compile_stmts(stmts)?;
                self.pop_scope();
            }
            Stmt::Return(expr) => {
                match expr {
                    Some(expr) => self.compile_expr(expr)?,
                    None => {
                        let null = self.const_null();
                        self.emit(Op::Const(null));
                    }
                }
                self.emit(Op::Return);
            }
            Stmt::Break => {
                if self.loops.is_empty() {
                    return Err(self.err("break outside a loop"));
                }
                self.emit(Op::Jump(0));
                let patch = self.code.len() - 1;
                self.pending_breaks.push(patch);
            }
            Stmt::Continue => {
                if self.loops.is_empty() {
                    return Err(self.err("continue outside a loop"));
                }
                self.emit(Op::Jump(0));
                let patch = self.code.len() - 1;
                self.loops.last_mut().unwrap().continue_patches.push(patch);
            }
        }
        Ok(())
    }

    fn patch_breaks(&mut self, ctx: &LoopCtx, end: u32) {
        let _ = ctx;
        // Pending breaks were collected in pending_breaks.
        for patch in self.pending_breaks.drain(..) {
            self.code[patch] = Op::Jump(end);
        }
    }

    /// Patches the `continue` sites of the loop that just finished.
    fn patch_continues(&mut self, ctx: &LoopCtx) {
        let target = ctx.continue_target;
        for patch in &ctx.continue_patches {
            self.code[*patch] = Op::Jump(target);
        }
    }

    /// The condition/body/step skeleton of a `for` loop after its init
    /// has run. With `per_iter`, the body runs against a fresh cell
    /// seeded from the control slot each round, synced back after.
    fn compile_for_rest(
        &mut self,
        cond: &Option<Expr>,
        step: &Option<Expr>,
        body: &Stmt,
        per_iter: Option<(Slot, String)>,
    ) -> CResult {
        let loop_top = self.code.len() as u32;
        let exit = match cond {
            Some(cond) => {
                self.compile_expr(cond)?;
                let j = self.code.len();
                self.emit(Op::JumpIfFalse(0));
                Some(j)
            }
            None => None,
        };
        // The body's binding is a fresh cell per round: closures in the
        // body capture this iteration's value, while the condition and
        // step keep using the plain control slot.
        if per_iter.is_some() {
            self.push_scope();
        }
        let cell_slot: Option<u16> = per_iter.as_ref().map(|(control, name)| {
            let cell = self.declare_forced_cell(name, Mem::Gc);
            self.emit(Op::FreshCell {
                from: control.idx,
                to: cell.idx,
            });
            cell.idx
        });
        self.loops.push(LoopCtx {
            break_patch: exit.unwrap_or(self.code.len()),
            continue_target: 0, // patched below
            continue_patches: Vec::new(),
        });
        let ctx_idx = self.loops.len() - 1;
        self.compile_stmt(body)?;
        if let Some((control, _)) = per_iter {
            // Sync the body's mutations back to the control slot before
            // `continue` or the step observe them.
            let cell = cell_slot.unwrap();
            self.emit(Op::GetCell(cell));
            self.emit(Op::SetLocal(control.idx));
        }
        let continue_target = self.code.len() as u32;
        self.loops[ctx_idx].continue_target = continue_target;
        let ctx_snapshot = self.loops[ctx_idx].clone();
        self.patch_continues(&ctx_snapshot);
        // The step runs on the control slot: leave the body scope so
        // the name resolves there again.
        if per_iter.is_some() {
            self.pop_scope();
        }
        if let Some(step) = step {
            self.compile_expr(step)?;
            self.emit(Op::Pop);
        }
        self.emit(Op::Jump(loop_top));
        let ctx = self.loops.pop().unwrap();
        if let Some(j) = exit {
            let end = self.code.len() as u32;
            self.code[j] = Op::JumpIfFalse(end);
        }
        let end = self.code.len() as u32;
        self.patch_breaks(&ctx, end);
        Ok(())
    }

    /// Compiles the initializer of a `for` header against explicit
    /// slots (the control-slot path skips normal declaration).
    fn compile_init_values(&mut self, init: &Stmt, slots: &[Slot]) -> CResult {
        if let Stmt::Let { decls, .. } = init {
            for (d, slot) in decls.iter().zip(slots) {
                let init_expr = &d.init;
                match init_expr {
                    Some(expr) => self.compile_expr(expr)?,
                    None => {
                        let null = self.const_null();
                        self.emit(Op::Const(null));
                    }
                }
                self.emit(Op::Declare {
                    slot: slot.idx,
                    mem: slot.mem,
                });
            }
        }
        Ok(())
    }

    fn compile_for_iter(
        &mut self,
        name: &str,
        iterable: &Expr,
        body: &Stmt,
        keys: bool,
    ) -> CResult {
        self.push_scope();
        self.compile_expr(iterable)?;
        self.emit(Op::MakeIter { keys });
        let iter_slot = self.declare("#iter", Mem::Gc, false);
        self.emit_store(iter_slot);

        let loop_top = self.code.len();
        self.emit_load(iter_slot);
        let jump = self.code.len();
        self.emit(Op::IterNext(0));
        // The next value/key is on the stack; store it into the loop
        // variable. The store keeps the value — pop it: only the
        // iterator stays live across the body.
        self.declare(name, Mem::Gc, false);
        let var = self.resolve(name).unwrap();
        self.emit_store(var);
        self.emit(Op::Pop);
        self.loops.push(LoopCtx {
            break_patch: jump,
            continue_target: loop_top as u32,
            continue_patches: Vec::new(),
        });
        self.compile_stmt(body)?;
        let ctx = self.loops.pop().unwrap();
        self.emit(Op::Jump(loop_top as u32));
        let done = self.code.len() as u32;
        self.code[jump] = Op::IterNext(done);
        self.patch_breaks(&ctx, done);
        self.pop_scope();
        Ok(())
    }

    // ---- expressions ----

    fn compile_expr(&mut self, expr: &Expr) -> CResult {
        match expr {
            Expr::Num(n) => {
                let idx = self.num_const(*n);
                self.emit(Op::Const(idx));
            }
            Expr::Str(s) => {
                let idx = self.str_const(s);
                self.emit(Op::Const(idx));
            }
            Expr::Bool(b) => {
                let idx = self.constant(ConstKey::Bool(*b), || Const::Bool(*b));
                self.emit(Op::Const(idx));
            }
            Expr::Null => {
                let idx = self.const_null();
                self.emit(Op::Const(idx));
            }
            Expr::Undefined => {
                let idx = self.constant(ConstKey::Undefined, || Const::Undefined);
                self.emit(Op::Const(idx));
            }
            Expr::Ident(name) => match self.resolve(name) {
                Some(slot) => self.emit_load(slot),
                None => {
                    let idx = self.str_const(name);
                    self.emit(Op::GetGlobal(idx));
                }
            },
            Expr::Array(items) => {
                for item in items {
                    self.compile_expr(item)?;
                }
                self.emit(Op::Array(items.len() as u16));
            }
            Expr::Obj(entries) => {
                // Keys must be consecutive constants: append raw, no
                // deduplication.
                let first_key = self.constants.len() as u32;
                for entry in entries {
                    self.constants.push(Const::Str(entry.key.clone()));
                }
                let _ = first_key;
                let key_base = first_key;
                for entry in entries {
                    self.compile_expr(&entry.value)?;
                }
                self.emit(Op::Object {
                    count: entries.len() as u16,
                    first_key: key_base,
                });
            }
            Expr::Bit(op, l, r) => {
                self.compile_expr(l)?;
                self.compile_expr(r)?;
                self.emit(match op {
                    BitOp::And => Op::BitAnd,
                    BitOp::Or => Op::BitOr,
                    BitOp::Xor => Op::BitXor,
                    BitOp::Shl => Op::Shl,
                    BitOp::Shr => Op::Shr,
                    BitOp::UShr => Op::UShr,
                });
            }
            Expr::BitNot(e) => {
                self.compile_expr(e)?;
                self.emit(Op::BitNot);
            }
            Expr::AsCast(cast) => {
                // Erased at runtime — identity in the dynamic engines.
                self.compile_expr(&cast.expr)?;
            }
            Expr::Unary(op, e) => {
                self.compile_expr(e)?;
                match op {
                    UnaryOp::Not => self.emit(Op::Not),
                    UnaryOp::Neg => self.emit(Op::Neg),
                    UnaryOp::Typeof => self.emit(Op::Typeof),
                }
            }
            Expr::Binary(op, l, r) => {
                self.compile_expr(l)?;
                self.compile_expr(r)?;
                self.emit(Op::Bin(*op));
            }
            Expr::Eq(op, l, r) => {
                self.compile_expr(l)?;
                self.compile_expr(r)?;
                self.emit(Op::Eq(*op));
            }
            Expr::Logical(op, l, r) => {
                self.compile_expr(l)?;
                // The kept operand is the result when short-circuiting.
                let jump = self.code.len();
                match op {
                    LogicalOp::And => self.emit(Op::JumpIfFalseKeep(0)),
                    LogicalOp::Or => self.emit(Op::JumpIfTrueKeep(0)),
                }
                self.emit(Op::Pop);
                self.compile_expr(r)?;
                let end = self.code.len() as u32;
                self.code[jump] = match op {
                    LogicalOp::And => Op::JumpIfFalseKeep(end),
                    LogicalOp::Or => Op::JumpIfTrueKeep(end),
                };
            }
            Expr::Ternary(cond, then, els) => {
                self.compile_expr(cond)?;
                let jump = self.code.len();
                self.emit(Op::JumpIfFalse(0));
                self.compile_expr(then)?;
                let else_jump = self.code.len();
                self.emit(Op::Jump(0));
                let then_end = self.code.len() as u32;
                self.code[jump] = Op::JumpIfFalse(then_end);
                self.compile_expr(els)?;
                let end = self.code.len() as u32;
                self.code[else_jump] = Op::Jump(end);
            }
            Expr::Assign(target, value) => {
                // The assigned value is the expression's value; it stays
                // on the stack.
                match target {
                    Target::Ident(name) => {
                        // `dst = src` between two own locals is a move:
                        // the source becomes the moved sentinel.
                        let move_from = match value.as_ref() {
                            Expr::Ident(src) => self.resolve(src).filter(|s| s.mem == Mem::Own),
                            _ => None,
                        };
                        match self.resolve(name) {
                            Some(slot) => {
                                if let Some(src) = move_from {
                                    self.emit(Op::MoveFrom(src.idx));
                                } else {
                                    self.compile_expr(value)?;
                                }
                                self.emit(Op::SetChecked { slot: slot.idx });
                            }
                            None => {
                                self.compile_expr(value)?;
                                let idx = self.str_const(name);
                                self.emit(Op::SetGlobal(idx));
                            }
                        }
                    }
                    Target::Index(obj, idx) => {
                        // Evaluate obj and idx exactly once, into temps.
                        self.compile_expr(obj)?;
                        let t_obj = self.fresh_temp();
                        self.emit_store(t_obj);
                        self.emit(Op::Pop);
                        self.compile_expr(idx)?;
                        let t_idx = self.fresh_temp();
                        self.emit_store(t_idx);
                        self.emit(Op::Pop);
                        self.compile_expr(value)?;
                        self.emit(Op::IndexSetTemp {
                            obj: t_obj.idx,
                            idx: t_idx.idx,
                        });
                    }
                    Target::Member(obj, prop) => {
                        self.compile_expr(obj)?;
                        let t_obj = self.fresh_temp();
                        self.emit_store(t_obj);
                        self.emit(Op::Pop);
                        let key = self.str_const(prop);
                        self.compile_expr(value)?;
                        self.emit(Op::MemberSetTemp {
                            obj: t_obj.idx,
                            key,
                        });
                    }
                }
            }
            Expr::Index(obj, idx) => {
                // Same once-only evaluation discipline.
                self.compile_expr(obj)?;
                let t_obj = self.fresh_temp();
                self.emit_store(t_obj);
                self.emit(Op::Pop);
                self.compile_expr(idx)?;
                let t_idx = self.fresh_temp();
                self.emit_store(t_idx);
                self.emit(Op::Pop);
                self.emit(Op::IndexGetTemp {
                    obj: t_obj.idx,
                    idx: t_idx.idx,
                });
            }
            Expr::Member(obj, prop) => {
                self.compile_expr(obj)?;
                let t_obj = self.fresh_temp();
                self.emit_store(t_obj);
                self.emit(Op::Pop);
                let key = self.str_const(prop);
                self.emit(Op::MemberGetTemp {
                    obj: t_obj.idx,
                    key,
                });
            }
            Expr::Call(callee, args) => {
                if let Expr::Member(obj_expr, prop) = &**callee {
                    if matches!(**obj_expr, Expr::Ident(ref n) if n == "console") && prop == "log" {
                        for a in args {
                            self.compile_expr(a)?;
                        }
                        self.emit(Op::Log {
                            args: args.len() as u16,
                        });
                        return Ok(());
                    }
                }
                // Method calls dispatch through the receiver: compile
                // the receiver first, then the args, then dispatch.
                if let Expr::Member(obj_expr, prop) = &**callee {
                    let key = self.str_const(prop);
                    self.compile_expr(obj_expr)?;
                    for a in args {
                        self.compile_expr(a)?;
                    }
                    self.emit(Op::MethodCall {
                        key,
                        args: args.len() as u16,
                    });
                    return Ok(());
                }
                for a in args {
                    self.compile_expr(a)?;
                }
                self.compile_expr(callee)?;
                self.emit(Op::Call {
                    args: args.len() as u16,
                });
            }
            Expr::Arrow(params, body) => self.compile_nested(None, params, body)?,
            Expr::Fn(name, params, stmts) => {
                self.compile_nested(name.as_deref(), params, &FnBody::Block(stmts.clone()))?;
            }
            Expr::Update(op, prefix, target) => {
                self.compile_update(*op, *prefix, target)?;
            }
        }
        Ok(())
    }

    /// `++`/`--` with single evaluation of the target; postfix yields
    /// the old value, prefix the new.
    fn compile_update(&mut self, op: UpdateOp, prefix: bool, target: &Target) -> CResult {
        let one = self.num_const(1.0);
        let bin = if op == UpdateOp::Inc {
            BinOp::Add
        } else {
            BinOp::Sub
        };
        match target {
            Target::Ident(name) => {
                let slot = self
                    .resolve(name)
                    .ok_or_else(|| self.err("update target not found"))?;
                self.emit_load(slot);
                if !prefix {
                    self.emit(Op::Dup); // keep the old value as the result
                }
                self.emit(Op::Const(one));
                self.emit(Op::Bin(bin));
                if prefix {
                    self.emit(Op::Dup); // the new value is the result
                }
                self.emit_store(slot);
                self.emit(Op::Pop); // drop the stored duplicate; result remains
                Ok(())
            }
            Target::Index(obj, idx) => {
                self.compile_expr(obj)?;
                let t_obj = self.fresh_temp();
                self.emit_store(t_obj);
                self.emit(Op::Pop);
                self.compile_expr(idx)?;
                let t_idx = self.fresh_temp();
                self.emit_store(t_idx);
                self.emit(Op::Pop);
                self.emit(Op::IndexGetTemp {
                    obj: t_obj.idx,
                    idx: t_idx.idx,
                });
                if !prefix {
                    self.emit(Op::Dup);
                }
                self.emit(Op::Const(one));
                self.emit(Op::Bin(bin));
                self.emit(Op::IndexSetTemp {
                    obj: t_obj.idx,
                    idx: t_idx.idx,
                });
                if !prefix {
                    // Stack: [old, new]; the result is the old one.
                    self.emit(Op::Pop);
                }
                Ok(())
            }
            Target::Member(obj, prop) => {
                self.compile_expr(obj)?;
                let t_obj = self.fresh_temp();
                self.emit_store(t_obj);
                self.emit(Op::Pop);
                let key = self.str_const(prop);
                self.emit(Op::MemberGetTemp {
                    obj: t_obj.idx,
                    key,
                });
                if !prefix {
                    self.emit(Op::Dup);
                }
                self.emit(Op::Const(one));
                self.emit(Op::Bin(bin));
                self.emit(Op::MemberSetTemp {
                    obj: t_obj.idx,
                    key,
                });
                if !prefix {
                    self.emit(Op::Pop);
                }
                Ok(())
            }
        }
    }

    /// Compiles a nested function into a sub-proto, capturing this
    /// frame's cells.
    fn compile_nested(&mut self, name: Option<&str>, params: &[String], body: &FnBody) -> CResult {
        // The nested body's free variables are its captures: they must
        // resolve in this frame (locals become cells) or be globals.
        let mut referenced: Vec<String> = {
            let mut refs = free_vars(params, body);
            refs.retain(|n| !params.contains(n));
            let mut names: Vec<String> = refs.into_iter().collect();
            names.sort();
            names
        };
        referenced.retain(|n| self.resolve(n).is_some());
        let mut captures = Vec::new();
        for n in &referenced {
            let slot = self.resolve(n).unwrap();
            captures.push(slot.idx);
        }
        let mut nested = FnCompiler::new_with_captures(name, &referenced, params);
        // The nested frame's own captures: names its inner closures
        // reach for, which must be its capture slots or locals.
        // The nested frame's own captures at any depth: those names
        // must be cells in this frame.
        nested.captured = match body {
            FnBody::Block(stmts) => all_nested_captures(stmts),
            FnBody::Expr(expr) => {
                let mut out = HashSet::new();
                collect_nested_captures_expr(expr, &mut out);
                out
            }
        };
        // Captured parameters become cells: the inner closure shares
        // the binding, and the frame boxes the argument.
        let captured_params: Vec<u16> = nested.scopes[0]
            .names
            .iter()
            .filter(|(n, s)| !s.cell && nested.captured.contains(*n))
            .map(|(_, s)| s.idx)
            .collect();
        for idx in captured_params {
            nested.cell_slots.push(idx);
            for scope in &mut nested.scopes {
                for slot in scope.names.values_mut() {
                    if slot.idx == idx {
                        slot.cell = true;
                    }
                }
            }
        }
        // Hoist `var` declarations: every one gets a function-scope
        // slot before the body compiles.
        if let FnBody::Block(stmts) = body {
            let mut var_names = HashSet::new();
            for stmt in stmts {
                collect_var_names(stmt, &mut var_names);
            }
            for var in var_names {
                if nested.resolve(&var).is_none() && !nested.scopes[0].names.contains_key(&var) {
                    let idx = nested.next_slot as u16;
                    nested.next_slot += 1;
                    nested.scopes[0].names.insert(
                        var.clone(),
                        Slot {
                            idx,
                            cell: false,
                            mem: Mem::Gc,
                        },
                    );
                    nested.slot_names.push((idx, var));
                }
            }
        }
        match body {
            FnBody::Expr(expr) => {
                nested.compile_expr(expr)?;
                nested.emit(Op::Return);
            }
            FnBody::Block(stmts) => {
                nested.compile_stmts(stmts)?;
                nested.emit(Op::Return);
            }
        }
        let proto = nested.finish();
        let idx = self.protos.len() as u32;
        self.protos.push(proto);
        self.captures.push(captures);
        self.emit(Op::Closure(idx));
        Ok(())
    }
}

/// Free variables of a nested function body: every reference minus the
/// body's own parameters and declarations, at any depth. These are the
/// names a closure reaches for in enclosing frames.
fn free_vars(params: &[String], body: &FnBody) -> HashSet<String> {
    let mut refs = HashSet::new();
    match body {
        FnBody::Expr(expr) => collect_all_refs_expr(expr, &mut refs),
        FnBody::Block(stmts) => {
            for stmt in stmts {
                collect_all_refs(stmt, &mut refs);
            }
        }
    }
    for p in params {
        refs.remove(p);
    }
    if let FnBody::Block(stmts) = body {
        let mut declared = HashSet::new();
        for stmt in stmts {
            collect_declared_deep(stmt, &mut declared);
        }
        for name in declared {
            refs.remove(&name);
        }
    }
    refs
}

/// Every name a body's declarations bind, at any depth.
fn collect_declared_deep(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::Let { decls, .. } | Stmt::Var { decls, .. } => {
            for d in decls {
                out.insert(d.name.clone());
            }
        }
        Stmt::FnDecl(name, ..) => {
            out.insert(name.clone());
        }
        Stmt::ForOf { name, body, .. } | Stmt::ForIn { name, body, .. } => {
            out.insert(name.clone());
            collect_declared_deep(body, out);
        }
        Stmt::For { init, body, .. } => {
            if let Some(init) = init {
                collect_declared_deep(init, out);
            }
            collect_declared_deep(body, out);
        }
        Stmt::If(_, then, els) => {
            collect_declared_deep(then, out);
            if let Some(els) = els {
                collect_declared_deep(els, out);
            }
        }
        Stmt::While(_, body) => collect_declared_deep(body, out),
        Stmt::Block(stmts) => {
            for s in stmts {
                collect_declared_deep(s, out);
            }
        }
        _ => {}
    }
}

/// The union of free variables over every nested function in a body,
/// at any depth. A frame pre-marks these names as cells, so by the time
/// a nested closure is instantiated its captures are real cells.
fn all_nested_captures(stmts: &[Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    for stmt in stmts {
        collect_nested_captures_stmt(stmt, &mut out);
    }
    out
}

fn collect_nested_captures_stmt(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::Let { decls, .. } | Stmt::Var { decls, .. } => {
            for d in decls {
                if let Some(init) = &d.init {
                    collect_nested_captures_expr(init, out);
                }
            }
        }
        Stmt::Expr(expr) => collect_nested_captures_expr(expr, out),
        Stmt::If(cond, then, els) => {
            collect_nested_captures_expr(cond, out);
            collect_nested_captures_stmt(then, out);
            if let Some(els) = els {
                collect_nested_captures_stmt(els, out);
            }
        }
        Stmt::While(cond, body) => {
            collect_nested_captures_expr(cond, out);
            collect_nested_captures_stmt(body, out);
        }
        Stmt::For {
            init,
            cond,
            step,
            body,
        } => {
            if let Some(init) = init {
                collect_nested_captures_stmt(init, out);
            }
            if let Some(cond) = cond {
                collect_nested_captures_expr(cond, out);
            }
            if let Some(step) = step {
                collect_nested_captures_expr(step, out);
            }
            collect_nested_captures_stmt(body, out);
        }
        Stmt::ForOf { iterable, body, .. } | Stmt::ForIn { iterable, body, .. } => {
            collect_nested_captures_expr(iterable, out);
            collect_nested_captures_stmt(body, out);
        }
        Stmt::Block(stmts) => {
            for s in stmts {
                collect_nested_captures_stmt(s, out);
            }
        }
        Stmt::Return(Some(expr)) => collect_nested_captures_expr(expr, out),
        _ => {}
    }
}

fn collect_nested_captures_expr(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Arrow(params, body) => {
            out.extend(free_vars(params, body));
            if let FnBody::Block(stmts) = &**body {
                collect_nested_captures_stmt_in_fn(stmts, out);
            }
        }
        Expr::Fn(_, params, stmts) => {
            out.extend(free_vars(params, &FnBody::Block(stmts.clone())));
            collect_nested_captures_stmt_in_fn(stmts, out);
        }
        Expr::Array(items) => items
            .iter()
            .for_each(|e| collect_nested_captures_expr(e, out)),
        Expr::Obj(entries) => entries
            .iter()
            .for_each(|e| collect_nested_captures_expr(&e.value, out)),
        Expr::Unary(_, e) => collect_nested_captures_expr(e, out),
        Expr::Binary(_, l, r) | Expr::Logical(_, l, r) | Expr::Eq(_, l, r) => {
            collect_nested_captures_expr(l, out);
            collect_nested_captures_expr(r, out);
        }
        Expr::Assign(target, value) => {
            collect_nested_captures_target(target, out);
            collect_nested_captures_expr(value, out);
        }
        Expr::Index(obj, idx) => {
            collect_nested_captures_expr(obj, out);
            collect_nested_captures_expr(idx, out);
        }
        Expr::Member(obj, _) => collect_nested_captures_expr(obj, out),
        Expr::Call(callee, args) => {
            collect_nested_captures_expr(callee, out);
            args.iter()
                .for_each(|a| collect_nested_captures_expr(a, out));
        }
        Expr::Ternary(c, t, e) => {
            collect_nested_captures_expr(c, out);
            collect_nested_captures_expr(t, out);
            collect_nested_captures_expr(e, out);
        }
        Expr::Update(_, _, target) => collect_nested_captures_target(target, out),
        _ => {}
    }
}

fn collect_nested_captures_stmt_in_fn(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        collect_nested_captures_stmt(s, out);
    }
}

fn collect_nested_captures_target(target: &Target, out: &mut HashSet<String>) {
    match target {
        Target::Ident(_) => {}
        Target::Index(obj, idx) => {
            collect_nested_captures_expr(obj, out);
            collect_nested_captures_expr(idx, out);
        }
        Target::Member(obj, _) => collect_nested_captures_expr(obj, out),
    }
}

/// Collects every identifier referenced anywhere in a statement,
/// including inside nested closures.
pub(crate) fn collect_all_refs(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::Let { decls, .. } | Stmt::Var { decls, .. } => {
            for d in decls {
                out.insert(d.name.clone());
                if let Some(init) = &d.init {
                    collect_all_refs_expr(init, out);
                }
            }
        }
        Stmt::Expr(expr) => collect_all_refs_expr(expr, out),
        Stmt::If(cond, then, els) => {
            collect_all_refs_expr(cond, out);
            collect_all_refs(then, out);
            if let Some(els) = els {
                collect_all_refs(els, out);
            }
        }
        Stmt::While(cond, body) => {
            collect_all_refs_expr(cond, out);
            collect_all_refs(body, out);
        }
        Stmt::For {
            init,
            cond,
            step,
            body,
        } => {
            if let Some(init) = init {
                collect_all_refs(init, out);
            }
            if let Some(cond) = cond {
                collect_all_refs_expr(cond, out);
            }
            if let Some(step) = step {
                collect_all_refs_expr(step, out);
            }
            collect_all_refs(body, out);
        }
        Stmt::ForOf {
            name,
            iterable,
            body,
            ..
        }
        | Stmt::ForIn {
            name,
            iterable,
            body,
            ..
        } => {
            out.insert(name.clone());
            collect_all_refs_expr(iterable, out);
            collect_all_refs(body, out);
        }
        Stmt::Block(stmts) => {
            for s in stmts {
                collect_all_refs(s, out);
            }
        }
        Stmt::Return(Some(expr)) => collect_all_refs_expr(expr, out),
        Stmt::FnDecl(name, ..) => {
            out.insert(name.clone());
        }
        _ => {}
    }
}

pub(crate) fn collect_all_refs_expr(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Ident(name) => {
            out.insert(name.clone());
        }
        Expr::Array(items) => items.iter().for_each(|e| collect_all_refs_expr(e, out)),
        Expr::Obj(entries) => entries
            .iter()
            .for_each(|e| collect_all_refs_expr(&e.value, out)),
        Expr::Unary(_, e) => collect_all_refs_expr(e, out),
        Expr::Binary(_, l, r) | Expr::Logical(_, l, r) | Expr::Eq(_, l, r) => {
            collect_all_refs_expr(l, out);
            collect_all_refs_expr(r, out);
        }
        Expr::Assign(target, value) => {
            collect_target_refs(target, out);
            collect_all_refs_expr(value, out);
        }
        Expr::Index(obj, idx) => {
            collect_all_refs_expr(obj, out);
            collect_all_refs_expr(idx, out);
        }
        Expr::Member(obj, _) => collect_all_refs_expr(obj, out),
        Expr::Call(callee, args) => {
            collect_all_refs_expr(callee, out);
            args.iter().for_each(|a| collect_all_refs_expr(a, out));
        }
        Expr::Ternary(c, t, e) => {
            collect_all_refs_expr(c, out);
            collect_all_refs_expr(t, out);
            collect_all_refs_expr(e, out);
        }
        Expr::Update(_, _, target) => collect_target_refs(target, out),
        Expr::Arrow(..) | Expr::Fn(..) => {}
        _ => {}
    }
}

fn collect_target_refs(target: &Target, out: &mut HashSet<String>) {
    match target {
        Target::Ident(name) => {
            out.insert(name.clone());
        }
        Target::Index(obj, idx) => {
            collect_all_refs_expr(obj, out);
            collect_all_refs_expr(idx, out);
        }
        Target::Member(obj, _) => collect_all_refs_expr(obj, out),
    }
}

/// Every `var` name declared anywhere in a statement tree, excluding
/// nested function bodies.
fn collect_var_names(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::Var { decls, .. } => {
            for d in decls {
                out.insert(d.name.clone());
            }
        }
        Stmt::Let { .. } | Stmt::FnDecl(..) => {}
        Stmt::Expr(_) => {}
        Stmt::If(_, then, els) => {
            collect_var_names(then, out);
            if let Some(els) = els {
                collect_var_names(els, out);
            }
        }
        Stmt::While(_, body) => collect_var_names(body, out),
        Stmt::For { init, body, .. } => {
            if let Some(init) = init {
                collect_var_names(init, out);
            }
            collect_var_names(body, out);
        }
        Stmt::ForOf { body, .. } | Stmt::ForIn { body, .. } => collect_var_names(body, out),
        Stmt::Block(stmts) => {
            for s in stmts {
                collect_var_names(s, out);
            }
        }
        _ => {}
    }
}
