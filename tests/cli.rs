//! The program end to end: arguments, files written, exit codes.
//!
//! Run with `cargo test --test cli`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn run(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_morph-terraform-migrate"))
        .args(args)
        .output()
        .expect("the program runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn out_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mtm-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_four_files_are_written_and_the_summary_printed() {
    let dir = out_dir("plain");
    let (code, stdout, stderr) = run(&[
        "--state",
        fixture("state.json").to_str().unwrap(),
        "--schema",
        fixture("schema.json").to_str().unwrap(),
        "--plan",
        fixture("plan.json").to_str().unwrap(),
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("13 rows in 4 tables, 4 references; 1 data sources and 0 filtered resources not migrated"), "{stdout}");
    assert!(stdout.contains("read the report before loading anything"));
    for name in ["schema.sql", "rows.sql", "controller.toml", "report.md"] {
        assert!(dir.join(name).is_file(), "{name}");
        assert!(stdout.contains(name), "{stdout}");
    }
    let toml_text = std::fs::read_to_string(dir.join("controller.toml")).unwrap();
    assert!(toml_text.contains("kind = \"morph\""));
    assert!(toml_text.contains("[mappings.lifecycle]\nprevent_destroy = true"));

    // Run again with other settings into the same place: the files are
    // replaced.
    let (code, stdout, stderr) = run(&[
        "--state",
        fixture("state.json").to_str().unwrap(),
        "--schema",
        fixture("schema.json").to_str().unwrap(),
        "--out",
        dir.to_str().unwrap(),
        "--engine",
        "pulumi",
        "--pulumi-mapping",
        fixture("pulumi-local.json").to_str().unwrap(),
        "--pulumi-mapping",
        fixture("pulumi-random.json").to_str().unwrap(),
        "--only",
        "module.notes",
        "--stacks-from-modules",
        "--unprotected",
        "--include-sensitive",
        "--guess-references",
        "--pg-schema",
        "tf",
        "--slot",
        "s1",
        "--publication",
        "p1",
        "--project",
        "proj",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("6 rows in 2 tables"), "{stdout}");
    let toml_text = std::fs::read_to_string(dir.join("controller.toml")).unwrap();
    assert!(toml_text.contains("kind = \"pulumi\""));
    assert!(toml_text.contains("project = \"proj\""));
    assert!(toml_text.contains("slot = \"s1\""));
    assert!(toml_text.contains("stack_column = \"tf_module\""));
    assert!(!toml_text.contains("prevent_destroy"));
    let schema = std::fs::read_to_string(dir.join("schema.sql")).unwrap();
    assert!(schema.contains("\"tf\".\"random_pet\""));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_that_cannot_be_read_or_nothing_to_migrate_exits_with_one() {
    let dir = out_dir("fail");
    let (code, _, stderr) = run(&[
        "--state",
        "/nonexistent/state.json",
        "--schema",
        fixture("schema.json").to_str().unwrap(),
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("morph-terraform-migrate: /nonexistent/state.json"),
        "{stderr}"
    );
    assert!(!dir.exists(), "nothing is written");

    let (code, _, stderr) = run(&[
        "--state",
        fixture("state.json").to_str().unwrap(),
        "--schema",
        fixture("schema.json").to_str().unwrap(),
        "--out",
        dir.to_str().unwrap(),
        "--only",
        "module.absent",
    ]);
    assert_eq!(code, 1);
    assert!(stderr.contains("nothing to migrate"), "{stderr}");

    let (code, _, stderr) = run(&[
        "--state",
        fixture("state.json").to_str().unwrap(),
        "--schema",
        fixture("schema.json").to_str().unwrap(),
        "--out",
        dir.to_str().unwrap(),
        "--pulumi-mapping",
        fixture("state.json").to_str().unwrap(),
        "--engine",
        "pulumi",
    ]);
    assert_eq!(
        code, 0,
        "a JSON file without resources is an empty mapping: {stderr}"
    );
    let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
    assert!(report.contains("## Pulumi types missing"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn wrong_arguments_exit_with_two_and_help_names_every_option() {
    let (code, _, stderr) = run(&["--state", "x"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--schema"), "{stderr}");
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    for option in [
        "--state",
        "--schema",
        "--plan",
        "--out",
        "--engine",
        "--pulumi-mapping",
        "--project",
        "--only",
        "--pg-schema",
        "--stacks-from-modules",
        "--include-sensitive",
        "--unprotected",
        "--guess-references",
        "--slot",
        "--publication",
    ] {
        assert!(stdout.contains(option), "{option}: {stdout}");
    }
    let (code, stdout, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("morph-terraform-migrate"));
}
