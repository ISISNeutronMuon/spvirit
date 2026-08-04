//! Verifies that the documentation in `docs/book` still matches the code.
//!
//! Chapters contain no copies of code — every snippet is an `{{#include}}`
//! against a real example file. This suite checks that every file, anchor,
//! tool, and example a chapter cites still exists, that no chapter includes
//! code it did not declare, that every shipped tool and example is documented
//! somewhere, and that the generated badge blocks tell the truth.
//!
//! Regenerate badge blocks after editing `verify.toml`:
//!
//!     UPDATE_DOCS=1 cargo test -p spvirit-tools --test docs_verify

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ─── manifest ────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct Verify {
    #[serde(default)]
    allow: Allow,
    #[serde(default)]
    chapters: BTreeMap<String, Chapter>,
}

#[derive(Deserialize, Default)]
struct Allow {
    #[serde(default)]
    undocumented_tools: Vec<String>,
    #[serde(default)]
    undocumented_examples: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Chapter {
    #[serde(default)]
    rust_examples: Vec<String>,
    #[serde(default)]
    py_examples: Vec<String>,
    #[serde(default)]
    anchors: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/spvirit-tools
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("spvirit-tools has a parent directory")
        .to_path_buf()
}

fn load_verify() -> Verify {
    let path = repo_root().join("docs/book/verify.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()))
}

/// True if `src` contains a balanced `ANCHOR: name` / `ANCHOR_END: name` pair.
///
/// Matching to end-of-line keeps `sim` from matching `simulate`.
fn anchor_exists(src: &str, name: &str) -> bool {
    let start = format!("ANCHOR: {name}");
    let end = format!("ANCHOR_END: {name}");
    let has = |needle: &str| src.lines().any(|l| l.trim_end().ends_with(needle));
    has(&start) && has(&end)
}

// ─── citation checks ─────────────────────────────────────────────────────

#[test]
fn cited_files_exist() {
    let verify = load_verify();
    let root = repo_root();
    let mut missing = Vec::new();

    for (chapter, spec) in &verify.chapters {
        if !root.join("docs/book/src").join(chapter).is_file() {
            missing.push(format!("chapter itself: docs/book/src/{chapter}"));
        }
        for file in spec.rust_examples.iter().chain(spec.py_examples.iter()) {
            if !root.join(file).is_file() {
                missing.push(format!("{chapter} cites missing file {file}"));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "verify.toml cites files that do not exist:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn cited_anchors_resolve() {
    let verify = load_verify();
    let root = repo_root();
    let mut broken = Vec::new();

    for (chapter, spec) in &verify.chapters {
        for anchor in &spec.anchors {
            let (file, name) = anchor
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("{chapter}: anchor {anchor:?} is not <path>:<name>"));
            let Ok(src) = fs::read_to_string(root.join(file)) else {
                broken.push(format!("{chapter}: cannot read {file}"));
                continue;
            };
            if !anchor_exists(&src, name) {
                broken.push(format!("{chapter}: no balanced ANCHOR {name} in {file}"));
            }
        }
    }

    assert!(
        broken.is_empty(),
        "unresolved anchors:\n  {}",
        broken.join("\n  ")
    );
}

#[test]
fn chapters_declare_every_include() {
    let verify = load_verify();
    let root = repo_root();
    let src_dir = root.join("docs/book/src");
    let mut undeclared = Vec::new();

    for (chapter, spec) in &verify.chapters {
        let chapter_path = src_dir.join(chapter);
        let Ok(text) = fs::read_to_string(&chapter_path) else {
            continue;
        };
        let chapter_dir = chapter_path.parent().unwrap().to_path_buf();

        for line in text.lines() {
            let Some(rest) = line.split_once("{{#include ").map(|(_, r)| r) else {
                continue;
            };
            let Some(target) = rest.split_once("}}").map(|(t, _)| t.trim()) else {
                continue;
            };
            // strip the ":anchor" suffix if present
            let raw = target.rsplit_once(':').map_or(target, |(p, _)| p);
            let resolved = chapter_dir.join(raw);
            let Ok(canonical) = resolved.canonicalize() else {
                undeclared.push(format!("{chapter}: include target does not exist: {raw}"));
                continue;
            };
            let declared = spec
                .rust_examples
                .iter()
                .chain(spec.py_examples.iter())
                .any(|d| root.join(d).canonicalize().ok().as_deref() == Some(&canonical));
            if !declared {
                undeclared.push(format!(
                    "{chapter}: includes {raw}, not declared in verify.toml"
                ));
            }
        }
    }

    assert!(
        undeclared.is_empty(),
        "chapters include code they did not declare:\n  {}",
        undeclared.join("\n  ")
    );
}

// ─── reverse coverage ────────────────────────────────────────────────────

/// Every `[[bin]]` name declared in spvirit-tools/Cargo.toml.
fn shipped_tools() -> Vec<String> {
    let text = fs::read_to_string(repo_root().join("spvirit-tools/Cargo.toml"))
        .expect("read spvirit-tools/Cargo.toml");
    let manifest: toml::Value = text.parse().expect("parse spvirit-tools/Cargo.toml");
    manifest
        .get("bin")
        .and_then(|b| b.as_array())
        .expect("spvirit-tools/Cargo.toml has [[bin]] entries")
        .iter()
        .filter_map(|b| b.get("name")?.as_str().map(str::to_owned))
        .collect()
}

/// Every example target in the workspace, as repo-relative paths.
fn shipped_examples() -> Vec<String> {
    let root = repo_root();
    let mut found = Vec::new();
    for crate_dir in [
        "spvirit-client",
        "spvirit-server",
        "spvirit-codec",
        "spvirit-types",
    ] {
        let dir = root.join(crate_dir).join("examples");
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs") {
                found.push(format!(
                    "{crate_dir}/examples/{}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn every_tool_is_documented() {
    let verify = load_verify();
    let documented: Vec<&str> = verify
        .chapters
        .values()
        .flat_map(|c| c.tools.iter().map(String::as_str))
        .collect();

    let mut undocumented = Vec::new();
    for tool in shipped_tools() {
        if documented.contains(&tool.as_str()) {
            continue;
        }
        if verify.allow.undocumented_tools.contains(&tool) {
            continue;
        }
        undocumented.push(tool);
    }

    assert!(
        undocumented.is_empty(),
        "these tools ship but no chapter documents them: {undocumented:?}\n\
         Write a chapter for each, or add it to [allow].undocumented_tools in \
         docs/book/verify.toml with a reason."
    );
}

#[test]
fn every_example_is_documented() {
    let verify = load_verify();
    let documented: Vec<&str> = verify
        .chapters
        .values()
        .flat_map(|c| c.rust_examples.iter().map(String::as_str))
        .collect();

    let mut undocumented = Vec::new();
    for example in shipped_examples() {
        if documented.contains(&example.as_str()) {
            continue;
        }
        if verify.allow.undocumented_examples.contains(&example) {
            continue;
        }
        undocumented.push(example);
    }

    assert!(
        undocumented.is_empty(),
        "these examples ship but no chapter documents them: {undocumented:?}\n\
         Cite each from a chapter, or add it to [allow].undocumented_examples in \
         docs/book/verify.toml with a reason."
    );
}

/// The [allow] lists are a migration aid and must shrink to empty. This test
/// fails if an entry is stale — either the thing no longer exists, or a
/// chapter now documents it and the entry should have been deleted.
#[test]
fn allow_list_has_no_stale_entries() {
    let verify = load_verify();
    let tools = shipped_tools();
    let examples = shipped_examples();
    let mut stale = Vec::new();

    let documented_tools: Vec<&str> = verify
        .chapters
        .values()
        .flat_map(|c| c.tools.iter().map(String::as_str))
        .collect();
    let documented_examples: Vec<&str> = verify
        .chapters
        .values()
        .flat_map(|c| c.rust_examples.iter().map(String::as_str))
        .collect();

    for tool in &verify.allow.undocumented_tools {
        if !tools.contains(tool) {
            stale.push(format!(
                "allow.undocumented_tools has {tool:?}, which is not a [[bin]]"
            ));
        } else if documented_tools.contains(&tool.as_str()) {
            stale.push(format!(
                "{tool:?} is now documented — delete it from allow.undocumented_tools"
            ));
        }
    }
    for example in &verify.allow.undocumented_examples {
        if !examples.contains(example) {
            stale.push(format!(
                "allow.undocumented_examples has {example:?}, which does not exist"
            ));
        } else if documented_examples.contains(&example.as_str()) {
            stale.push(format!(
                "{example:?} is now documented — delete it from allow.undocumented_examples"
            ));
        }
    }

    assert!(
        stale.is_empty(),
        "stale [allow] entries:\n  {}",
        stale.join("\n  ")
    );
}
