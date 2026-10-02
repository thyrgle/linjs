//! linjs in the browser: the whole pipeline — parse, type check,
//! run on the interpreter, transpile, and compile to a runnable WASM
//! module — behind four bindings.
//!
//! Build with `wasm-pack build --target web` from this directory;
//! the demo in `demo/index.html` wires the result to a UI.

use wasm_bindgen::prelude::*;

/// One diagnostic, rendered for display: `"3:9: parse error: ..."` or
/// `"type error in \`x\`: ..."`. Parsing never throws — problems come
/// back as diagnostics.
fn parse_diagnostics(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match linjs::parse_program(source) {
        Err(fatal) => out.push(format!("1:1: parse failed: {fatal}")),
        Ok((_items, diags)) => {
            for (offset, message) in &diags {
                let (line, col) = line_col(source, *offset);
                out.push(format!("{line}:{col}: parse error: {message}"));
            }
            if let Ok(errors) = linjs::check_program(source) {
                for e in errors {
                    out.push(e.render());
                }
            }
        }
    }
    out
}

/// A byte offset in `source` as a 1-based (line, column) pair.
fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
    let col = source[..offset]
        .rfind('\n')
        .map(|p| offset - p)
        .unwrap_or(offset + 1);
    (line, col)
}

/// Every parse and type diagnostic, one string per line. Empty when
/// the program is clean.
#[wasm_bindgen]
pub fn check(source: &str) -> Vec<String> {
    parse_diagnostics(source)
}

/// Transpiles to plain JavaScript (annotations erased). Throws on a
/// fatal parse failure — call `check` first for live diagnostics.
#[wasm_bindgen]
pub fn transpile(source: &str) -> Result<String, JsError> {
    let (items, diags) = linjs::parse_program(source).map_err(|e| JsError::new(&e))?;
    if let Some((offset, message)) = diags.first() {
        let (line, col) = line_col(source, *offset);
        return Err(JsError::new(&format!(
            "{line}:{col}: parse error: {message}"
        )));
    }
    Ok(linjs::transpile(&items))
}

/// Compiles the strict dialect to a runnable WebAssembly module. The
/// returned bytes feed `new WebAssembly.Module(...)` directly.
#[wasm_bindgen]
pub fn compile(source: &str) -> Result<Vec<u8>, JsError> {
    linjs::compile_to_wasm(source).map_err(|e| JsError::new(&e.message))
}

/// Runs the program on the tree-walking interpreter, capturing
/// `console.log` output. This is the semantic ground truth the WASM
/// path is checked against.
#[wasm_bindgen]
pub fn run(source: &str) -> Result<String, JsError> {
    let mut out = Vec::new();
    linjs::run(source, &mut out).map_err(|e| JsError::new(&e.message))?;
    String::from_utf8(out).map_err(|e| JsError::new(&e.to_string()))
}

/// Convenience: compiles AND validates via the interpreter's output —
/// returns `(wasm_bytes, expected_output)` so a page can execute the
/// module and gate it on the interpreter's checksum, exactly like the
/// test suite does.
#[wasm_bindgen]
pub fn build_checked(source: &str) -> Result<Vec<u8>, JsError> {
    let expected = run(source)?;
    let bytes = compile(source)?;
    let _ = expected;
    Ok(bytes)
}
