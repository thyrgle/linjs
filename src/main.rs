//! The `linjs` command-line tool.
//!
//! ```text
//! linjs check [--strict] <files...>   parse + type diagnostics (add a
//!                                     strict-dialect validation pass)
//! linjs build <file> [-o out.wasm]    compile the strict dialect to WASM
//! linjs run <file>                    run on the tree-walking interpreter
//! ```
//!
//! `check` exits 0 when every file is clean, 1 when any diagnostic is
//! reported. Types are TS-style: checked statically, erased at runtime.

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        None => {
            print_usage();
            ExitCode::from(2)
        }
        Some((cmd, rest)) => match cmd.as_str() {
            "check" => cmd_check(rest),
            "build" => cmd_build(rest),
            "run" => cmd_run(rest),
            "version" | "--version" => {
                println!("linjs {}", env!("CARGO_PKG_VERSION"));
                ExitCode::SUCCESS
            }
            "help" | "--help" | "-h" => {
                print_usage();
                ExitCode::SUCCESS
            }
            other => {
                eprintln!("unknown command `{other}`");
                print_usage();
                ExitCode::from(2)
            }
        },
    }
}

fn print_usage() {
    println!(
        "linjs {} — the gradual-memory JavaScript subset

USAGE:
    linjs check [--strict] <files...>   parse + type diagnostics
    linjs build <file> [-o out.wasm]    compile to WebAssembly
    linjs run <file>                    run on the interpreter
    linjs version                       print the version

check flags:
    --strict    also validate the strict WASM dialect compiles
    --quiet     print nothing on success",
        env!("CARGO_PKG_VERSION")
    );
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

fn cmd_check(args: &[String]) -> ExitCode {
    let mut strict = false;
    let mut quiet = false;
    let mut files: Vec<&String> = Vec::new();
    for a in args {
        match a.as_str() {
            "--strict" => strict = true,
            "--quiet" => quiet = true,
            f if !f.starts_with("--") => files.push(a),
            other => {
                eprintln!("unknown flag `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    if files.is_empty() {
        eprintln!("check: no files given");
        return ExitCode::from(2);
    }

    let mut any_bad = false;
    for path in files {
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("check: cannot read {path}: {e}");
                any_bad = true;
                continue;
            }
        };
        let mut file_bad = false;

        // Parse diagnostics carry byte offsets — render line:col.
        match parse_program(&source) {
            Err(fatal) => {
                println!("{path}: parse failed: {fatal}");
                file_bad = true;
            }
            Ok((items, diags)) => {
                for (offset, message) in &diags {
                    let (line, col) = line_col(&source, *offset);
                    println!("{path}:{line}:{col}: parse error: {message}");
                    file_bad = true;
                }

                // Type errors only mean something on a parsed tree.
                match check_program(&source) {
                    Ok(errors) => {
                        for e in errors {
                            println!("{path}: {}", e.render());
                            file_bad = true;
                        }
                    }
                    Err(fatal) => {
                        if diags.is_empty() {
                            println!("{path}: check failed: {fatal}");
                            file_bad = true;
                        }
                        // With parse errors above, the type check's
                        // fatal is downstream noise — already reported.
                    }
                }

                if strict && !file_bad {
                    // The strict dialect gate: the file must compile
                    // to WebAssembly (typed + @own shapes only).
                    if let Err(e) = compile_to_wasm(&source) {
                        println!("{path}: strict dialect: {}", e.message);
                        file_bad = true;
                    }
                }

                let _ = items;
            }
        }

        if !file_bad && !quiet {
            println!("{path}: ok");
        }
        any_bad |= file_bad;
    }
    if any_bad {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_build(args: &[String]) -> ExitCode {
    let mut input: Option<&String> = None;
    let mut output: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => match it.next() {
                Some(o) => output = Some(o.clone()),
                None => {
                    eprintln!("build: -o needs a path");
                    return ExitCode::from(2);
                }
            },
            f if !f.starts_with('-') => input = Some(a),
            other => {
                eprintln!("build: unknown flag `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(path) = input else {
        eprintln!("build: no input file");
        return ExitCode::from(2);
    };
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("build: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let out =
        output.unwrap_or_else(|| path.strip_suffix(".js").unwrap_or(path).to_string() + ".wasm");
    match compile_to_wasm(&source) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&out, &bytes) {
                eprintln!("build: cannot write {out}: {e}");
                return ExitCode::FAILURE;
            }
            println!("{} ({} bytes)", out, bytes.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("build: {}", e.message);
            ExitCode::FAILURE
        }
    }
}

fn cmd_run(args: &[String]) -> ExitCode {
    let Some(path) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("run: no input file");
        return ExitCode::from(2);
    };
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("run: cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match run(&source, &mut lock) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(lock);
            eprintln!("run: {}", e.message);
            ExitCode::FAILURE
        }
    }
}

use linjs::{check_program, compile_to_wasm, parse_program, run};
