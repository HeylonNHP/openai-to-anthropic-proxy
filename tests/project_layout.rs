//! Project-hygiene tests: the repo's own conventions, checked by `cargo test`.
//!
//! These are integration tests (they live in `tests/`, so they run as their
//! own crate and can read the working tree) rather than unit tests of the
//! proxy itself.

use std::fs;
use std::path::{Path, PathBuf};

/// The repository root (the crate's manifest directory).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `src/`, sorted, so failures are deterministic.
fn source_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .expect("read src/ directory")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo_root().join("src"), &mut out);
    out
}

/// The file a `mod NAME;` declaration in `declaring_file` resolves to.
///
/// Rust's module layout (The Rust Book, "Separating Modules into Different
/// Files"):
///
/// * `src/lib.rs` / `src/main.rs` (a crate root) -> `src/NAME.rs`;
/// * `src/thing/mod.rs` -> `src/thing/NAME.rs`;
/// * `src/thing.rs` -> `src/thing/NAME.rs`.
fn module_file(declaring_file: &Path, name: &str) -> PathBuf {
    let crate_root_src = repo_root().join("src");
    let parent = declaring_file.parent().expect("source file has a parent");
    let dir = if declaring_file.file_name().is_some_and(|f| f == "mod.rs") {
        parent.to_path_buf()
    } else if parent == crate_root_src
        && declaring_file
            .file_stem()
            .is_some_and(|s| s == "lib" || s == "main")
    {
        // Only the crate roots at the top of `src/` are crate roots; a
        // nested `foo/main.rs` is an ordinary file module.
        parent.to_path_buf()
    } else {
        parent.join(declaring_file.file_stem().expect("source file has a stem"))
    };
    dir.join(format!("{name}.rs"))
}

/// The item that follows a `#[cfg(test)]` attribute, skipping any other
/// attributes in between (e.g. `#[allow(dead_code)]`).
///
/// Returns the item's text, or `None` when the `#[cfg(test)]` applies to
/// something that is not a module at all (a test-only helper function, say).
fn item_after_cfg_test(lines: &[&str], attr_index: usize) -> Option<String> {
    for line in lines.iter().skip(attr_index + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue; // blank line or another attribute — keep looking
        }
        return Some(trimmed.to_string());
    }
    None
}

/// The module name a `mod` item declares, if it declares one at all.
fn module_name_of(item: &str) -> Option<&str> {
    let rest = item.strip_prefix("pub ").unwrap_or(item);
    let rest = rest.strip_prefix("mod ")?;
    rest.strip_suffix(';')
        .map(str::trim)
        .or_else(|| rest.strip_suffix('{').map(str::trim))
}

/// Every `#[cfg(test)]` in the tree, paired with the item it applies to.
fn cfg_test_items() -> Vec<(PathBuf, usize, String)> {
    let mut found = Vec::new();
    for path in source_files() {
        let text = fs::read_to_string(&path).expect("read source file");
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            if !line.trim().starts_with("#[cfg(test)]") {
                continue;
            }
            let same_line = line
                .trim()
                .strip_prefix("#[cfg(test)]")
                .unwrap_or("")
                .trim();
            let item = if same_line.is_empty() {
                item_after_cfg_test(&lines, n)
            } else {
                Some(same_line.to_string())
            };
            if let Some(item) = item {
                found.push((path.clone(), n + 1, item));
            }
        }
    }
    found
}

/// The project standard: every unit-test module lives in its OWN file, next
/// to the module that owns it, and the source file declares it with
/// `#[cfg(test)] mod NAME;`.
///
/// There is no "is the file big enough?" judgement any more: the rule is
/// uniform, so the layout is predictable and `git blame` points at the test
/// file rather than a 7,000-line source file.
///
/// The test module is still a CHILD module, so `use super::*;` keeps
/// working and private items stay testable — that is the whole point of
/// choosing this layout over `tests/` (which only sees the public API).
#[test]
fn unit_test_modules_are_declared_not_inlined() {
    let mut offenders = Vec::new();
    for (path, line_no, item) in cfg_test_items() {
        let Some(name) = module_name_of(&item) else {
            continue; // `#[cfg(test)]` on a non-module item — fine
        };
        let is_inline = item.ends_with('{');
        if is_inline {
            offenders.push(format!(
                "{}:{line_no}: `mod {name} {{ … }}` is inline — the standard is \
                 `#[cfg(test)] mod {name};` with the test module in its own file",
                path.display()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "inline test modules found — the project standard is one test module per file:\n{}",
        offenders.join("\n")
    );
}

/// Every `#[cfg(test)] mod NAME;` declaration resolves to a file that
/// exists and is not empty, so a moved test module can never silently
/// disappear.
#[test]
fn every_declared_test_module_has_a_file() {
    let mut missing = Vec::new();
    for (path, line_no, item) in cfg_test_items() {
        let Some(name) = module_name_of(&item) else {
            continue;
        };
        if !item.ends_with(';') {
            continue; // inline module — covered by the other test
        }
        let target = module_file(&path, name);
        if !target.exists() {
            missing.push(format!(
                "{}:{line_no}: `mod {name};` points at {} which does not exist",
                path.display(),
                target.display()
            ));
        } else if fs::metadata(&target).is_ok_and(|m| m.len() == 0) {
            missing.push(format!(
                "{}:{line_no}: `mod {name};` points at {} which is empty",
                path.display(),
                target.display()
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "declared test modules without a usable file:\n{}",
        missing.join("\n")
    );
}
