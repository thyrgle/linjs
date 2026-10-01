//! The stack VM: executes compiled [`Program`]s.
//!
//! Frames are explicit: each call pushes a frame of `slot_count` slots
//! (params first), an instruction pointer, and its own activation
//! arena. Return pops all three — `@own` teardown is structural, which
//! is the whole point of this machine as the rehearsal for a WASM
//! backend.
//!
//! Semantics match the tree-walking interpreter, operation by
//! operation: the same value rules, the same memory checks (moves,
//! read-only borrows, no escapes, no unannotated own bindings), and the
//! same Node-style `console.log`. The differential tests bind the two
//! engines together.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{EqOp, Mem};
use crate::compile::{Const, Op, Program};
use crate::interp::InterpError;
use crate::value::{Func, IterState, ObjMap, Value};

impl From<crate::compile::CompileError> for InterpError {
    fn from(e: crate::compile::CompileError) -> Self {
        InterpError { message: e.message }
    }
}

fn bail(msg: impl Into<String>) -> InterpError {
    InterpError {
        message: msg.into(),
    }
}

/// A call frame.
struct Frame {
    proto: Rc<crate::compile::FuncProto>,
    ip: usize,
    slots: Vec<Value>,
}

/// The virtual machine. One instance runs one program.
pub struct Vm<'o> {
    out: &'o mut dyn std::io::Write,
    arenas: crate::mem::Arenas,
    globals: HashMap<String, Value>,
    stack: Vec<Value>,
    frames: Vec<Frame>,
}

type VResult = Result<(), InterpError>;

impl<'o> Vm<'o> {
    pub fn new(out: &'o mut dyn std::io::Write) -> Self {
        Vm {
            out,
            arenas: crate::mem::new_store(),
            globals: HashMap::new(),
            stack: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// How many activation arenas exist (1 = every call arena dropped).
    pub fn arena_count(&self) -> usize {
        self.arenas.borrow().len()
    }

    /// Compiles and runs `source` (with the standard pre-passes).
    pub fn run_source(&mut self, source: &str) -> Result<(), InterpError> {
        let (items, _) = crate::passes::parse_program(source)?;
        self.run_items(&items)
    }

    /// Compiles parsed items and runs them.
    pub fn run_items(&mut self, items: &[crate::interp::Item]) -> Result<(), InterpError> {
        let program = crate::compile::compile(items)?;
        self.run(&program)
    }

    /// Runs a compiled program.
    pub fn run(&mut self, program: &Program) -> Result<(), InterpError> {
        // The non-writable numeric globals JavaScript always provides.
        self.globals.insert("NaN".into(), Value::Num(f64::NAN));
        self.globals
            .insert("Infinity".into(), Value::Num(f64::INFINITY));

        let main = program.main.clone();
        self.frames.push(Frame {
            proto: main,
            ip: 0,
            slots: Vec::new(),
        });
        let slot_count = self.frames[0].proto.slot_count;
        self.frames[0].slots = vec![Value::Undefined; slot_count];

        self.loop_over()
    }

    fn loop_over(&mut self) -> VResult {
        self.run_until(1)
    }

    /// Runs until fewer than `min_frames` frames remain (the program
    /// ends, or the call this re-entrant run started returns).
    fn run_until(&mut self, min_frames: usize) -> VResult {
        let mut fuel: u64 = 50_000_000;
        while let Some(frame_idx) = self.frames.len().checked_sub(1) {
            if self.frames.len() < min_frames {
                return Ok(());
            }
            fuel -= 1;
            if fuel == 0 {
                return Err(bail("execution ran too long (fuel exhausted)"));
            }
            let proto = self.frames[frame_idx].proto.clone();
            let op = match proto.code.get(self.frames[frame_idx].ip) {
                Some(op) => op.clone(),
                None => {
                    // Ran off the end: implicit return.
                    if self.frames.len() == 1 {
                        return Ok(());
                    }
                    self.do_return(frame_idx, Value::Undefined)?;
                    continue;
                }
            };
            self.frames[frame_idx].ip += 1;
            match self.step(frame_idx, &proto, op)? {
                Step::Continue => {}
                Step::Returned(value) => {
                    if self.frames.len() <= min_frames.min(self.frames.len()) {
                        // The re-entrant run's frame (or the program's
                        // main frame) returned: leave the value.
                        self.frames.truncate(frame_idx);
                        crate::mem::pop(&self.arenas);
                        self.stack.push(value);
                        return Ok(());
                    }
                    self.do_return(frame_idx, value)?;
                }
                Step::Halted => return Ok(()),
            }
        }
        Ok(())
    }

    fn do_return(&mut self, frame_idx: usize, value: Value) -> VResult {
        if matches!(value, Value::Own(_)) {
            return Err(bail(
                "an @own value cannot escape its activation — return a copy or restructure",
            ));
        }
        self.frames.truncate(frame_idx);
        crate::mem::pop(&self.arenas);
        self.stack.push(value);
        Ok(())
    }

    fn step(
        &mut self,
        frame_idx: usize,
        proto: &Rc<crate::compile::FuncProto>,
        op: Op,
    ) -> Result<Step, InterpError> {
        match op {
            Op::Const(idx) => {
                let value = match &proto.constants[idx as usize] {
                    Const::Num(n) => Value::Num(*n),
                    Const::Str(s) => Value::Str(Rc::from(s.as_str())),
                    Const::Bool(b) => Value::Bool(*b),
                    Const::Null => Value::Null,
                    Const::Undefined => Value::Undefined,
                };
                self.stack.push(value);
            }
            Op::GetLocal(slot) => {
                let v = self.frames[frame_idx].slots[slot as usize].clone();
                if matches!(v, Value::Moved) {
                    let name = slot_name(proto, slot);
                    return Err(bail(format!("use after move of `{name}`")));
                }
                self.stack.push(v);
            }
            Op::SetLocal(slot) => {
                // Keep semantics: the value stays (assignment expressions
                // have values; the statement wrapper pops).
                let v = self.stack.last().unwrap().clone();
                self.frames[frame_idx].slots[slot as usize] = v;
            }
            Op::GetCell(slot) => {
                let existing = self.frames[frame_idx].slots[slot as usize].clone();
                let cell = as_cell(&existing).ok_or_else(|| {
                    bail(format!(
                        "bad cell read (slot {slot} holds {:?} in {})",
                        existing,
                        self.frames[frame_idx]
                            .proto
                            .name
                            .clone()
                            .unwrap_or_default()
                    ))
                })?;
                let value = cell.borrow().clone();
                if matches!(value, Value::Moved) {
                    let name = slot_name(proto, slot);
                    return Err(bail(format!("use after move of `{name}`")));
                }
                self.stack.push(value);
            }
            Op::SetCell(slot) => {
                let v = self.stack.last().unwrap().clone();
                let existing = self.frames[frame_idx].slots[slot as usize].clone();
                if let Some(cell) = as_cell(&existing) {
                    *cell.borrow_mut() = v;
                } else {
                    return Err(bail("bad cell store"));
                }
            }
            Op::GetGlobal(idx) => {
                let name = proto_string(proto, idx);
                match self.globals.get(name) {
                    Some(v) => {
                        let v = v.clone();
                        self.check_moved(&v)?;
                        self.stack.push(v);
                    }
                    None => return Err(bail(format!("{name} is not defined"))),
                }
            }
            Op::SetGlobal(idx) => {
                let name = proto_string(proto, idx).to_string();
                let v = self.stack.last().unwrap().clone();
                self.globals.insert(name, v);
            }
            Op::Dup => {
                let v = self.stack.last().unwrap().clone();
                self.stack.push(v);
            }
            Op::Closure(idx) => {
                let captures = proto.captures[idx as usize].clone();
                let mut cells = Vec::new();
                for slot in captures {
                    let existing = self.frames[frame_idx].slots[slot as usize].clone();
                    // Lazy boxing: a pre-reserved capture slot (one no
                    // declaration ever targeted) holds a plain value
                    // until the first closure captures it.
                    let cell = match as_cell(&existing) {
                        Some(cell) => cell,
                        None => {
                            let cell = Rc::new(RefCell::new(existing.clone()));
                            self.frames[frame_idx].slots[slot as usize] = Value::Cell(cell.clone());
                            cell
                        }
                    };
                    cells.push(cell);
                }
                let nested = proto.protos[idx as usize].clone();
                self.stack.push(Value::Func(Rc::new(Func::VmClosure {
                    name: nested.name.clone(),
                    proto: nested,
                    cells,
                })));
            }
            Op::MethodCall { key, args } => {
                let mut argv = Vec::with_capacity(args as usize);
                for _ in 0..args {
                    argv.push(self.stack.pop().unwrap());
                }
                argv.reverse();
                let recv = self.stack.pop().unwrap();
                let name = proto_string(proto, key).to_string();
                self.call_method(&recv, &name, argv)?;
            }
            Op::Call { args } => {
                let callee = self.stack.pop().unwrap();
                let mut argv = Vec::with_capacity(args as usize);
                for _ in 0..args {
                    argv.push(self.stack.pop().unwrap());
                }
                argv.reverse();
                self.call_value(&callee, argv)?;
            }
            Op::Log { args } => {
                let mut argv = Vec::with_capacity(args as usize);
                for _ in 0..args {
                    argv.push(self.stack.pop().unwrap());
                }
                argv.reverse();
                let mut line = String::new();
                for (i, v) in argv.iter().enumerate() {
                    if i > 0 {
                        line.push(' ');
                    }
                    line.push_str(&match v {
                        Value::Own(handle) => self.inspect_own(handle)?,
                        Value::Arr(_) | Value::Obj(_) | Value::Func(_) => v.inspect(),
                        other => other.to_display(),
                    });
                }
                writeln!(self.out, "{line}").map_err(|e| bail(format!("write failed: {e}")))?;
                self.stack.push(Value::Undefined);
            }
            Op::Return => {
                let v = self.stack.pop().unwrap();
                return Ok(Step::Returned(v));
            }
            Op::Pop => {
                self.stack.pop().unwrap();
            }
            Op::Array(count) => {
                let mut items = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    items.push(self.stack.pop().unwrap());
                }
                items.reverse();
                self.stack.push(Value::Arr(Rc::new(RefCell::new(items))));
            }
            Op::Object { count, first_key } => {
                let mut entries: ObjMap = Vec::with_capacity(count as usize);
                for i in (0..count).rev() {
                    let value = self.stack.pop().unwrap();
                    let key = proto_string(proto, first_key + i as u32).to_string();
                    entries.push((key, value));
                }
                // The pop loop built the entries back to front.
                entries.reverse();
                self.stack.push(Value::Obj(Rc::new(RefCell::new(entries))));
            }
            Op::IndexGet => {
                let idx = self.stack.pop().unwrap();
                let obj = self.stack.pop().unwrap();
                let v = crate::interp::index_read(&self.arenas, &obj, &idx)?;
                self.stack.push(v);
            }
            Op::IndexSet => {
                let value = self.stack.pop().unwrap();
                let idx = self.stack.pop().unwrap();
                let obj = self.stack.pop().unwrap();
                if matches!(value, Value::Own(_)) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                crate::interp::index_write(&self.arenas, &obj, &idx, value.clone())?;
                self.stack.push(value);
            }
            Op::MemberGet(key) => {
                let obj = self.stack.pop().unwrap();
                let k = Value::Str(Rc::from(proto_string(proto, key)));
                let v = crate::interp::index_read(&self.arenas, &obj, &k)?;
                self.stack.push(v);
            }
            Op::MemberSet(key) => {
                let value = self.stack.pop().unwrap();
                let obj = self.stack.pop().unwrap();
                if matches!(value, Value::Own(_)) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                let k = Value::Str(Rc::from(proto_string(proto, key)));
                crate::interp::index_write(&self.arenas, &obj, &k, value.clone())?;
                self.stack.push(value);
            }
            Op::Bin(op) => {
                let rv = self.stack.pop().unwrap();
                let lv = self.stack.pop().unwrap();
                if matches!(lv, Value::Own(_)) || matches!(rv, Value::Own(_)) {
                    return Err(bail(
                        "@own values cannot take part in arithmetic — read their parts instead",
                    ));
                }
                self.stack.push(crate::interp::binary(op, lv, rv));
            }
            Op::Eq(op) => {
                let rv = self.stack.pop().unwrap();
                let lv = self.stack.pop().unwrap();
                let result = match op {
                    EqOp::Strict => lv.strict_eq(&rv),
                    EqOp::StrictNe => !lv.strict_eq(&rv),
                    EqOp::Loose => lv.loose_eq(&rv),
                    EqOp::LooseNe => !lv.loose_eq(&rv),
                };
                self.stack.push(Value::Bool(result));
            }
            Op::Not => {
                let v = self.stack.pop().unwrap();
                self.stack.push(Value::Bool(!v.is_truthy()));
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr | Op::UShr => {
                let rv = self.stack.pop().unwrap();
                let lv = self.stack.pop().unwrap();
                let (a, b) = (js_to_int32(&lv), js_to_int32(&rv));
                let result: i32 = match op {
                    Op::BitAnd => a & b,
                    Op::BitOr => a | b,
                    Op::BitXor => a ^ b,
                    Op::Shl => a.wrapping_shl(b as u32 & 31),
                    Op::Shr => a.wrapping_shr(b as u32 & 31),
                    Op::UShr => ((a as u32).wrapping_shr(b as u32 & 31)) as i32,
                    _ => unreachable!(),
                };
                self.stack.push(Value::Num(result as f64));
            }
            Op::BitNot => {
                let v = self.stack.pop().unwrap();
                self.stack.push(Value::Num(!js_to_int32(&v) as f64));
            }
            Op::Neg => {
                let v = self.stack.pop().unwrap();
                if matches!(v, Value::Own(_)) {
                    return Err(bail("@own values cannot take part in arithmetic"));
                }
                self.stack.push(Value::Num(-to_num(&v)));
            }
            Op::Typeof => {
                let v = self.stack.pop().unwrap();
                self.stack.push(crate::interp::type_of(&v));
            }
            Op::JumpIfFalse(target) => {
                let v = self.stack.pop().unwrap();
                if !v.is_truthy() {
                    self.frames[frame_idx].ip = target as usize;
                }
            }
            Op::JumpIfFalseKeep(target) => {
                let v = self.stack.last().unwrap().clone();
                if !v.is_truthy() {
                    self.frames[frame_idx].ip = target as usize;
                }
            }
            Op::JumpIfTrueKeep(target) => {
                let v = self.stack.last().unwrap().clone();
                if v.is_truthy() {
                    self.frames[frame_idx].ip = target as usize;
                }
            }
            Op::Jump(target) => {
                self.frames[frame_idx].ip = target as usize;
            }
            Op::MakeIter { keys } => {
                let iterable = self.stack.pop().unwrap();
                let items: Vec<Value> = match &iterable {
                    Value::Arr(arr) => arr.borrow().clone(),
                    Value::Own(handle) => crate::mem::read_arr(&self.arenas, handle)?,
                    Value::Iter(_) => Vec::new(),
                    Value::Str(s) => s
                        .chars()
                        .map(|c| Value::Str(Rc::from(c.to_string().as_str())))
                        .collect(),
                    other if keys => match other {
                        Value::Obj(obj) => obj
                            .borrow()
                            .iter()
                            .map(|(k, _)| Value::Str(Rc::from(k.as_str())))
                            .collect(),
                        Value::Arr(arr) => (0..arr.borrow().len())
                            .map(|i| {
                                Value::Str(Rc::from(crate::value::fmt_number(i as f64).as_str()))
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                    // for-of over non-iterables is an error; for-in
                    // iterates nothing.
                    Value::Obj(_)
                    | Value::Num(_)
                    | Value::Bool(_)
                    | Value::Null
                    | Value::Undefined
                    | Value::Moved
                    | Value::Cell(_)
                    | Value::Func(_) => {
                        if keys {
                            Vec::new()
                        } else {
                            return Err(bail(format!("{} is not iterable", iterable.to_display())));
                        }
                    }
                };
                self.stack.push(Value::Iter(Rc::new(RefCell::new(IterState {
                    items,
                    pos: 0,
                }))));
            }
            Op::IterNext(done) => {
                let iter = self.stack.last().unwrap().clone();
                let state = match &iter {
                    Value::Iter(state) => state.clone(),
                    _ => return Err(bail("bad iterator")),
                };
                let mut state = state.borrow_mut();
                if state.pos < state.items.len() {
                    let v = state.items[state.pos].clone();
                    state.pos += 1;
                    drop(state);
                    self.stack.push(v);
                } else {
                    drop(state);
                    self.frames[frame_idx].ip = done as usize;
                }
            }
            Op::Declare { slot, mem } => {
                let raw = self.stack.pop().unwrap();
                let value = self.declare_value(mem, raw)?;
                if proto.cell_slots.contains(&slot) {
                    // Write through a lazily boxed cell if one exists —
                    // hoisted closures may already hold it.
                    let existing = self.frames[frame_idx].slots[slot as usize].clone();
                    match as_cell(&existing) {
                        Some(cell) => *cell.borrow_mut() = value,
                        None => {
                            self.frames[frame_idx].slots[slot as usize] =
                                Value::Cell(Rc::new(RefCell::new(value)))
                        }
                    }
                } else {
                    self.frames[frame_idx].slots[slot as usize] = value;
                }
            }
            Op::SetChecked { slot } => {
                let value = self.stack.last().unwrap().clone();
                if proto.const_slots.contains(&slot) {
                    let name = slot_name(proto, slot);
                    return Err(bail(format!("assignment to constant variable `{name}`")));
                }
                let current = self.frames[frame_idx].slots[slot as usize].clone();
                if let Some(cell) = as_cell(&current) {
                    // Captured local: write through the shared cell.
                    let was = cell.borrow().clone();
                    if matches!(value, Value::Own(_))
                        && !matches!(was, Value::Own(_) | Value::Undefined | Value::Moved)
                    {
                        return Err(bail("cannot store an @own value in an unannotated binding"));
                    }
                    *cell.borrow_mut() = value;
                } else {
                    if matches!(value, Value::Own(_))
                        && !matches!(current, Value::Own(_) | Value::Undefined | Value::Moved)
                    {
                        return Err(bail("cannot store an @own value in an unannotated binding"));
                    }
                    self.frames[frame_idx].slots[slot as usize] = value;
                }
            }
            Op::MoveFrom(slot) => {
                let existing = self.frames[frame_idx].slots[slot as usize].clone();
                if let Some(cell) = as_cell(&existing) {
                    // Captured local: move through the shared cell.
                    let v = cell.borrow().clone();
                    if matches!(&v, Value::Own(handle) if handle.readonly) {
                        return Err(bail("cannot move through @ref"));
                    }
                    if !matches!(v, Value::Own(_)) {
                        return Err(bail("@own applies to arrays and objects"));
                    }
                    *cell.borrow_mut() = Value::Moved;
                    self.stack.push(v);
                } else {
                    let v = existing;
                    if matches!(&v, Value::Own(handle) if handle.readonly) {
                        return Err(bail("cannot move through @ref"));
                    }
                    if !matches!(v, Value::Own(_)) {
                        return Err(bail("@own applies to arrays and objects"));
                    }
                    self.frames[frame_idx].slots[slot as usize] = Value::Moved;
                    self.stack.push(v);
                }
            }
            Op::MoveGlobalFrom(idx) => {
                let name = proto_string(proto, idx).to_string();
                let v = self
                    .globals
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| bail(format!("{name} is not defined")))?;
                if !matches!(v, Value::Own(_)) {
                    return Err(bail("@own applies to arrays and objects"));
                }
                self.globals.insert(name, Value::Moved);
                self.stack.push(v);
            }
            Op::FreshCell { from, to } => {
                // from == to is the common case: read the current value
                // (through the old cell) and install a fresh cell.
                let existing = self.frames[frame_idx].slots[from as usize].clone();
                let v = match as_cell(&existing) {
                    Some(cell) => cell.borrow().clone(),
                    None => existing,
                };
                self.frames[frame_idx].slots[to as usize] = Value::Cell(Rc::new(RefCell::new(v)));
            }
            Op::IndexGetTemp { obj, idx } => {
                let o = self.frames[frame_idx].slots[obj as usize].clone();
                let i = self.frames[frame_idx].slots[idx as usize].clone();
                let v = crate::interp::index_read(&self.arenas, &o, &i)?;
                self.stack.push(v);
            }
            Op::IndexSetTemp { obj, idx } => {
                let value = self.stack.last().unwrap().clone();
                let o = self.frames[frame_idx].slots[obj as usize].clone();
                let i = self.frames[frame_idx].slots[idx as usize].clone();
                if matches!(value, Value::Own(_)) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                crate::interp::index_write(&self.arenas, &o, &i, value.clone())?;
            }
            Op::MemberGetTemp { obj, key } => {
                let o = self.frames[frame_idx].slots[obj as usize].clone();
                let k = Value::Str(Rc::from(proto_string(proto, key)));
                let v = crate::interp::index_read(&self.arenas, &o, &k)?;
                self.stack.push(v);
            }
            Op::MemberSetTemp { obj, key } => {
                let value = self.stack.last().unwrap().clone();
                let o = self.frames[frame_idx].slots[obj as usize].clone();
                if matches!(value, Value::Own(_)) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                let k = Value::Str(Rc::from(proto_string(proto, key)));
                crate::interp::index_write(&self.arenas, &o, &k, value.clone())?;
            }
            Op::Halt => return Ok(Step::Halted),
        }
        Ok(Step::Continue)
    }

    /// Applies a declaration's memory mode to the initializer.
    fn declare_value(&mut self, mem: Mem, raw: Value) -> Result<Value, InterpError> {
        match mem {
            Mem::Gc => match raw {
                Value::Own(_) => Err(bail(
                    "cannot assign an @own value to unannotated binding — declare it with // @own or // @ref",
                )),
                other => Ok(other),
            },
            Mem::Own => match raw {
                Value::Own(handle) => Ok(Value::Own(handle)),
                fresh @ (Value::Arr(_) | Value::Obj(_)) => Ok(Value::Own(
                    crate::mem::take_gc(&self.arenas, &fresh)?,
                )),
                other => Err(bail(format!(
                    "@own applies to arrays and objects, not {}",
                    other.to_display()
                ))),
            },
            Mem::Ref => match raw {
                Value::Own(handle) => Ok(Value::Own(crate::mem::OwnHandle {
                    readonly: true,
                    ..handle
                })),
                other => Err(bail(format!(
                    "@ref requires an @own value, not {}",
                    other.to_display()
                ))),
            },
        }
    }

    fn check_moved(&self, v: &Value) -> VResult {
        if matches!(v, Value::Moved) {
            return Err(bail("use after move"));
        }
        Ok(())
    }

    /// Calls a function value: closures push frames (with an arena,
    /// read-only borrowed @own params), natives do their thing.
    fn call_value(&mut self, callee: &Value, argv: Vec<Value>) -> VResult {
        // Parameters borrow @own values, as the interpreter specifies.
        let argv: Vec<Value> = argv
            .into_iter()
            .map(|arg| match arg {
                Value::Own(handle) => Value::Own(crate::mem::OwnHandle {
                    readonly: true,
                    ..handle
                }),
                other => other,
            })
            .collect();
        match callee {
            Value::Func(f) => match &**f {
                Func::VmClosure { proto, cells, .. } => {
                    self.push_frame_with_cells(proto.clone(), argv, cells.clone())?;
                    Ok(())
                }
                Func::Closure { .. } => Err(bail("internal: AST closure reached the VM")),
                Func::Native { name, .. } => {
                    let _ = name;
                    // The only native the language exposes today is
                    // reached through Op::Log; direct calls report.
                    Err(bail("native functions cannot be called directly"))
                }
            },
            other => Err(bail(format!("{} is not a function", other.to_display()))),
        }
    }

    fn push_frame_with_cells(
        &mut self,
        proto: Rc<crate::compile::FuncProto>,
        argv: Vec<Value>,
        incoming_cells: Vec<Rc<RefCell<Value>>>,
    ) -> VResult {
        crate::mem::push(&self.arenas);
        let mut slots = vec![Value::Undefined; proto.slot_count];
        // Capture slots first: the closure's cells, boxed as-is.
        for (i, cell) in incoming_cells.iter().enumerate() {
            slots[i] = Value::Cell(cell.clone());
        }
        // Parameters borrow @own values, then land in their declared
        // slots (a captured param's slot is its cell).
        for (i, param_slot) in proto.param_slots.iter().enumerate() {
            let arg = argv.get(i).cloned().unwrap_or(Value::Undefined);
            let arg = match arg {
                Value::Own(handle) => Value::Own(crate::mem::OwnHandle {
                    readonly: true,
                    ..handle
                }),
                other => other,
            };
            if proto.cell_slots.contains(param_slot) {
                slots[*param_slot as usize] = Value::Cell(Rc::new(RefCell::new(arg)));
            } else {
                slots[*param_slot as usize] = arg;
            }
        }
        self.frames.push(Frame {
            proto,
            ip: 0,
            slots,
        });
        Ok(())
    }

    /// Array method dispatch, matching the interpreter's semantics:
    /// push/pop mutate (borrows rejected), map/filter run their
    /// callbacks on a re-entrant execution of the frame stack.
    fn call_method(&mut self, recv: &Value, name: &str, argv: Vec<Value>) -> VResult {
        match (recv, name) {
            (Value::Arr(arr), "push") => {
                if argv.iter().any(|v| matches!(v, Value::Own(_))) {
                    return Err(bail(
                        "cannot store an @own value in a garbage-collected container",
                    ));
                }
                let mut arr = arr.borrow_mut();
                arr.extend(argv);
                self.stack.push(Value::Num(arr.len() as f64));
                Ok(())
            }
            (Value::Arr(arr), "pop") => {
                let popped = arr.borrow_mut().pop().unwrap_or(Value::Undefined);
                self.stack.push(popped);
                Ok(())
            }
            (Value::Own(handle), "push") => {
                if argv.iter().any(|v| matches!(v, Value::Own(_))) {
                    return Err(bail("@own values cannot be nested (depth-one ownership)"));
                }
                crate::mem::with_arr_mut(&self.arenas, handle, |items| items.extend(argv))?;
                let len = crate::mem::read_arr(&self.arenas, handle)?.len();
                self.stack.push(Value::Num(len as f64));
                Ok(())
            }
            (Value::Own(handle), "pop") => {
                let mut popped = Value::Undefined;
                crate::mem::with_arr_mut(&self.arenas, handle, |items| {
                    popped = items.pop().unwrap_or(Value::Undefined);
                })?;
                self.stack.push(popped);
                Ok(())
            }
            (Value::Arr(arr), "map" | "filter") => {
                let snapshot = arr.borrow().clone();
                self.run_map_or_filter(&snapshot, name, argv)
            }
            (Value::Own(handle), "map" | "filter") => {
                let snapshot = crate::mem::read_arr(&self.arenas, handle)?;
                self.run_map_or_filter(&snapshot, name, argv)
            }
            (recv, _) => Err(bail(format!(
                "{}.{} is not a function",
                recv.to_display(),
                name
            ))),
        }
    }

    fn run_map_or_filter(
        &mut self,
        snapshot: &[Value],
        name: &str,
        mut argv: Vec<Value>,
    ) -> VResult {
        let callback = argv
            .pop()
            .ok_or_else(|| bail(format!("{name} expects a callback")))?;
        let mut out = Vec::with_capacity(snapshot.len());
        for (i, item) in snapshot.iter().enumerate() {
            // Push the call, run it re-entrantly, take the result.
            self.stack.push(callback.clone());
            self.stack.push(item.clone());
            self.stack.push(Value::Num(i as f64));
            let depth = self.frames.len() + 1;
            self.call_value(&callback, vec![item.clone(), Value::Num(i as f64)])?;
            self.run_until(depth)?;
            let result = self.stack.pop().unwrap();
            if name == "map" {
                out.push(result);
            } else if result.is_truthy() {
                out.push(item.clone());
            }
        }
        self.stack.push(Value::Arr(Rc::new(RefCell::new(out))));
        Ok(())
    }

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
}

enum Step {
    Continue,
    Returned(Value),
    Halted,
}

fn as_cell(v: &Value) -> Option<Rc<RefCell<Value>>> {
    match v {
        Value::Cell(cell) => Some(cell.clone()),
        _ => None,
    }
}

/// JavaScript ToInt32: truncate toward zero, then wrap modulo 2^32.
fn js_to_int32(v: &Value) -> i32 {
    let n = match v {
        Value::Num(n) => *n,
        other => to_num(other),
    };
    if n.is_nan() || n.is_infinite() {
        return 0;
    }
    (n.trunc() as i64) as i32
}

fn to_num(v: &Value) -> f64 {
    match v.to_number_value() {
        Value::Num(n) => n,
        _ => f64::NAN,
    }
}

fn slot_name(proto: &crate::compile::FuncProto, slot: u16) -> String {
    proto
        .slot_names
        .iter()
        .find(|(idx, _)| *idx == slot)
        .map(|(_, name)| name.clone())
        .unwrap_or_else(|| format!("slot {slot}"))
}

fn proto_string(proto: &crate::compile::FuncProto, idx: u32) -> &str {
    match &proto.constants[idx as usize] {
        Const::Str(s) => s,
        _ => "",
    }
}
