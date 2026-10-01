//! The memory modes: arenas for `@own`, handles, moves.
//!
//! `@own` values allocate into the current activation's arena — a
//! `Vec` dropped the moment the function returns, so annotated values
//! never touch the reference-counted heap and are freed deterministically.
//! `@ref` values are handles into the same storage tagged read-only.
//!
//! The rules the interpreter enforces on top of this storage:
//!
//! * **Moves**: an `@own` declaration initialized from another `@own`
//!   binding moves it — the source becomes a use-after-move error.
//! * **Borrows**: `@ref` copies the handle as read-only; writes through
//!   it (index, property, `push`/`pop`) are rejected. Function
//!   parameters receive `@own` arguments as borrows too.
//! * **No escapes**: returning an `@own` value, storing one in a
//!   garbage-collected container, or assigning one to an unannotated
//!   binding are all errors. Dropped arenas are therefore unreachable
//!   — the defensive `dropped` check never fires in valid programs.
//! * **Depth one**: members of owned containers are ordinary
//!   garbage-collected values; `@own` values cannot be nested.

use std::cell::RefCell;
use std::rc::Rc;

use crate::value::{ObjMap, Value};

/// Heap-shaped data an `@own` value can hold.
pub enum OwnData {
    Arr(Vec<Value>),
    Obj(ObjMap),
}

/// One activation's storage.
#[derive(Default)]
pub struct Arena {
    pub slots: Vec<OwnData>,
}

/// A handle to arena storage. `readonly` marks borrows (`@ref`); the
/// same slot accessed through an `@own` binding is writable.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnHandle {
    pub arena: usize,
    pub slot: usize,
    pub readonly: bool,
}

/// The arena store. The bottom arena (index 0) is the global
/// activation; each call pushes one and pops it on return.
pub type Arenas = Rc<RefCell<Vec<Arena>>>;

pub fn new_store() -> Arenas {
    Rc::new(RefCell::new(vec![Arena::default()]))
}

/// Pushes an activation arena.
pub fn push(arenas: &Arenas) {
    arenas.borrow_mut().push(Arena::default());
}

/// Pops the top activation arena, dropping every `@own` value created
/// during the call.
pub fn pop(arenas: &Arenas) {
    arenas.borrow_mut().pop();
}

/// Allocates into the current (top) arena.
pub fn alloc(arenas: &Arenas, data: OwnData) -> OwnHandle {
    let mut arenas = arenas.borrow_mut();
    let arena = arenas.len() - 1;
    let slot = arenas[arena].slots.len();
    arenas[arena].slots.push(data);
    OwnHandle {
        arena,
        slot,
        readonly: false,
    }
}

/// Whether the handle's arena is gone (defensive: escape checks should
/// make this unreachable).
pub fn dropped(arenas: &Arenas, handle: &OwnHandle) -> bool {
    handle.arena >= arenas.borrow().len()
}

/// Reads a handle's array contents (cloned; elements are GC values).
pub fn read_arr(arenas: &Arenas, handle: &OwnHandle) -> Result<Vec<Value>, String> {
    if dropped(arenas, handle) {
        return Err("an @own value's arena was dropped".into());
    }
    match &arenas.borrow()[handle.arena].slots[handle.slot] {
        OwnData::Arr(items) => Ok(items.clone()),
        OwnData::Obj(_) => Err("expected an @own array".into()),
    }
}

/// Reads a handle's object entries (cloned).
pub fn read_obj(arenas: &Arenas, handle: &OwnHandle) -> Result<ObjMap, String> {
    if dropped(arenas, handle) {
        return Err("an @own value's arena was dropped".into());
    }
    match &arenas.borrow()[handle.arena].slots[handle.slot] {
        OwnData::Obj(entries) => Ok(entries.clone()),
        OwnData::Arr(_) => Err("expected an @own object".into()),
    }
}

/// Mutates a handle's array in place. Rejects borrows.
pub fn with_arr_mut(
    arenas: &Arenas,
    handle: &OwnHandle,
    f: impl FnOnce(&mut Vec<Value>),
) -> Result<(), String> {
    if handle.readonly {
        return Err("@ref is read-only".into());
    }
    if dropped(arenas, handle) {
        return Err("an @own value's arena was dropped".into());
    }
    match &mut arenas.borrow_mut()[handle.arena].slots[handle.slot] {
        OwnData::Arr(items) => {
            f(items);
            Ok(())
        }
        OwnData::Obj(_) => Err("expected an @own array".into()),
    }
}

/// Mutates a handle's object in place. Rejects borrows.
pub fn with_obj_mut(
    arenas: &Arenas,
    handle: &OwnHandle,
    f: impl FnOnce(&mut ObjMap),
) -> Result<(), String> {
    if handle.readonly {
        return Err("@ref is read-only".into());
    }
    if dropped(arenas, handle) {
        return Err("an @own value's arena was dropped".into());
    }
    match &mut arenas.borrow_mut()[handle.arena].slots[handle.slot] {
        OwnData::Obj(entries) => {
            f(entries);
            Ok(())
        }
        OwnData::Arr(_) => Err("expected an @own object".into()),
    }
}

/// Converts a freshly created garbage-collected container into arena
/// storage, taking its contents (the value was never shared).
pub fn take_gc(arenas: &Arenas, value: &Value) -> Result<OwnHandle, String> {
    match value {
        Value::Arr(arr) => match Rc::try_unwrap(arr.clone()) {
            Ok(cell) => Ok(alloc(arenas, OwnData::Arr(cell.into_inner()))),
            Err(shared) => Ok(alloc(arenas, OwnData::Arr(shared.borrow().clone()))),
        },
        Value::Obj(obj) => match Rc::try_unwrap(obj.clone()) {
            Ok(cell) => Ok(alloc(arenas, OwnData::Obj(cell.into_inner()))),
            Err(shared) => Ok(alloc(arenas, OwnData::Obj(shared.borrow().clone()))),
        },
        other => Err(format!(
            "@own applies to arrays and objects, not {}",
            other.to_display()
        )),
    }
}
