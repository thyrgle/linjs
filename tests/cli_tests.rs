//! The `linjs` binary: check/build/run end to end, including exit
//! codes — the toolchain contract users and editors see.

use std::process::Command;

fn linjs() -> Command {
    Command::new(env!("CARGO_BIN_EXE_linjs"))
}

fn write_tmp(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("linjs_cli_tests");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let p = dir.join(name);
    std::fs::write(&p, src).expect("write");
    p
}

#[test]
fn check_clean_file_exits_zero() {
    let p = write_tmp(
        "clean.js",
        "// @own\nlet a = [1, 2, 3];\nlet t = 0;\nfor (let i = 0; i < a.length; i++) { t += a[i]; }\nconsole.log(t);\n",
    );
    let out = linjs().arg("check").arg(&p).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ok"), "stdout: {text}");
}

#[test]
fn check_reports_type_errors_with_path() {
    let p = write_tmp(
        "typed.js",
        "let x: number = 5;\nx = \"oops\";\nconsole.log(x);\n",
    );
    let out = linjs().arg("check").arg(&p).output().unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let path = p.to_string_lossy();
    assert!(
        text.contains(&format!("{path}: type error in `x`")),
        "stdout: {text}"
    );
}

#[test]
fn check_renders_parse_errors_as_line_col() {
    let p = write_tmp("broken.js", "let a = 1;\nlet b = ;\n");
    let out = linjs().arg("check").arg(&p).output().unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("broken.js:2:9: parse error"),
        "stdout: {text}"
    );
}

#[test]
fn check_strict_validates_the_dialect() {
    // The strict dialect: untyped `let x = 5` in a value position is
    // fine, but bare dynamic shapes (e.g. object literals) are not.
    let good = write_tmp("strict_ok.js", "let x = 1;\nconsole.log(x + 1);\n");
    let out = linjs()
        .args(["check", "--strict"])
        .arg(&good)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let bad = write_tmp("strict_bad.js", "let o = {a: 1};\nconsole.log(o.a);\n");
    let out = linjs()
        .args(["check", "--strict"])
        .arg(&bad)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("strict dialect"), "stdout: {text}");
}

#[test]
fn check_missing_file_fails() {
    let out = linjs()
        .args(["check", "/nonexistent/linjs_no_such.js"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot read"), "stderr: {err}");
}

#[test]
fn run_executes_via_the_interpreter() {
    let p = write_tmp("runme.js", "console.log(1 + 2, \"three\");\n");
    let out = linjs().arg("run").arg(&p).output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3 three\n");
}

#[test]
fn build_emits_a_validated_wasm_module() {
    let p = write_tmp(
        "buildme.js",
        "// @own\nlet a = [1, 2, 3];\nlet s = 0;\nfor (let i = 0; i < a.length; i++) { s += a[i]; }\nconsole.log(s);\n",
    );
    let out_path = std::env::temp_dir()
        .join("linjs_cli_tests")
        .join("buildme.wasm");
    let out = linjs()
        .args(["build"])
        .arg(&p)
        .args(["-o"])
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bytes = std::fs::read(&out_path).expect("wasm written");
    assert_eq!(&bytes[..4], b"\0asm", "wasm magic");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!("{} bytes", bytes.len())),
        "stdout: {text}"
    );

    // The module itself must be valid: the same bytes compile a
    // checksum-identical run in the interpreter.
    let src = std::fs::read_to_string(&p).unwrap();
    linjs::check_wasm_against_node(&src)
        .map_err(|e| format!("differential: {e}"))
        .unwrap_or_else(|e| {
            if e == "node not available" {
                eprintln!("skipping node differential");
            } else {
                panic!("{e}");
            }
        });
}

#[test]
fn build_reports_dialect_errors_and_defaults_output_path() {
    let p = write_tmp("nobuild.js", "let o = {a: 1};\nconsole.log(o.a);\n");
    let out = linjs().arg("build").arg(&p).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.is_empty());

    // Default output path: <input minus .js>.wasm
    let ok = write_tmp("default_out.js", "console.log(40 + 2);\n");
    let out = linjs().arg("build").arg(&ok).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let default_path = ok.with_extension("wasm");
    assert!(
        default_path.exists(),
        "default -o path: {}",
        default_path.display()
    );
    let _ = std::fs::remove_file(default_path);
}

#[test]
fn version_and_help_exit_zero() {
    for flag in ["version", "--version", "help", "--help"] {
        let out = linjs().arg(flag).output().unwrap();
        assert!(out.status.success(), "flag: {flag}");
    }
    let out = linjs().output().unwrap();
    assert_eq!(out.status.code(), Some(2), "no args = usage error");
}
