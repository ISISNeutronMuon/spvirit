//! Guards the version coupling between the two Python packages.
//!
//! `pip install spvirit` is meant to bring the command-line tools with it, so
//! `spvirit-py/pyproject.toml` depends on `spvirit-tools` by exact version.
//! Both packages take their version from the workspace, but that pin is a
//! literal string in a file whose own version is dynamic — nothing bumps it.
//! Left unchecked it would drift on the next release and resolve to a stale
//! tools wheel, or to nothing at all.
//!
//! This suite fails the build instead.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/spvirit-tools
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("spvirit-tools has a parent directory")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn parse(rel: &str) -> toml::Value {
    read(rel)
        .parse::<toml::Value>()
        .unwrap_or_else(|e| panic!("cannot parse {rel}: {e}"))
}

/// The single version every crate and both wheels inherit.
fn workspace_version() -> String {
    parse("Cargo.toml")
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .expect("root Cargo.toml declares [workspace.package] version")
        .to_string()
}

/// Every member inherits the workspace version rather than restating one.
///
/// A member that reintroduces `version = "..."` would drift silently: the
/// pin below would still match the workspace, while the wheel built from that
/// member carried a different number.
#[test]
fn members_inherit_the_workspace_version() {
    let root = parse("Cargo.toml");
    let members = root["workspace"]["members"]
        .as_array()
        .expect("[workspace] members is an array");

    for member in members {
        let name = member.as_str().expect("member entries are strings");
        let manifest = parse(&format!("{name}/Cargo.toml"));
        let version = &manifest["package"]["version"];

        assert!(
            version.get("workspace").and_then(|w| w.as_bool()) == Some(true),
            "{name}/Cargo.toml sets an explicit version ({version}); \
             use `version.workspace = true` so the workspace stays unified",
        );
    }
}

/// `spvirit-py`'s dependency on `spvirit-tools` pins the current version.
#[test]
fn python_bindings_pin_the_current_tools_version() {
    let expected = workspace_version();
    let pyproject = parse("spvirit-py/pyproject.toml");

    let deps = pyproject["project"]["dependencies"]
        .as_array()
        .expect("spvirit-py/pyproject.toml declares [project] dependencies");

    let pin = deps
        .iter()
        .filter_map(|d| d.as_str())
        .find(|d| {
            let name: String = d
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
                .collect();
            // PEP 503 normalisation: -, _ and . are equivalent separators.
            name.to_lowercase().replace(['_', '.'], "-") == "spvirit-tools"
        })
        .unwrap_or_else(|| {
            panic!(
                "spvirit-py/pyproject.toml has no spvirit-tools dependency; \
                 `pip install spvirit` would not install the command-line tools"
            )
        });

    let actual = pin
        .split_once("==")
        .unwrap_or_else(|| {
            panic!("spvirit-tools pin `{pin}` is not an exact `==` requirement")
        })
        .1
        .trim();

    assert_eq!(
        actual, expected,
        "spvirit-py/pyproject.toml pins spvirit-tools {actual}, but the \
         workspace is at {expected}; update the pin in spvirit-py/pyproject.toml",
    );
}

/// The tools wheel is built as binaries, not as an extension module.
///
/// Without `bindings = "bin"` maturin finds a library target and no pyo3
/// dependency, and the resulting wheel ships no executables at all — it
/// installs cleanly and provides nothing.
#[test]
fn tools_wheel_ships_binaries() {
    let pyproject = parse("spvirit-tools/pyproject.toml");

    assert_eq!(
        pyproject["tool"]["maturin"]["bindings"].as_str(),
        Some("bin"),
        "spvirit-tools/pyproject.toml must set [tool.maturin] bindings = \"bin\"",
    );

    assert_eq!(
        pyproject["project"]["name"].as_str(),
        Some("spvirit-tools"),
        "the wheel name must match the pin in spvirit-py/pyproject.toml",
    );
}
