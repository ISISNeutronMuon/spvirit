//! Build script for spvirit-tools.
//!
//! Everything here belongs to the optional `scry` feature (the packet-capture
//! tools folded in from spvirit-scry): Npcap SDK discovery on Windows and the
//! vergen git metadata its version header prints. Without that feature the
//! script does nothing, so a default build needs neither Npcap nor git.

#[cfg(feature = "scry")]
use vergen_git2::{Emitter, Git2Builder};

#[cfg(all(windows, feature = "scry"))]
fn find_npcap_sdk() -> String {
    use std::path::PathBuf;

    // 1. Explicit override via environment variable
    if let Ok(dir) = std::env::var("NPCAP_SDK") {
        let p = PathBuf::from(&dir);
        let lib = if cfg!(target_pointer_width = "64") {
            p.join("Lib").join("x64")
        } else {
            p.join("Lib")
        };
        if lib.join("wpcap.lib").exists() {
            return lib.to_string_lossy().into_owned();
        }
        panic!(
            "\n\n\
            ══════════════════════════════════════════════════════════════\n\
            NPCAP_SDK is set to \"{}\" but wpcap.lib was not found at\n\
            {}\n\n\
            Make sure NPCAP_SDK points to the extracted Npcap SDK root\n\
            (the folder that contains a Lib/ subdirectory).\n\
            ══════════════════════════════════════════════════════════════\n",
            dir,
            lib.display()
        );
    }

    // 2. Common install locations
    let arch_dir = if cfg!(target_pointer_width = "64") {
        "Lib/x64"
    } else {
        "Lib"
    };

    let candidates: Vec<PathBuf> = {
        let mut v = Vec::new();

        // Paths relative to the workspace
        v.push(PathBuf::from("npcap-sdk").join(arch_dir));

        // System-wide locations
        for root in &["C:/npcap", "C:/npcap-sdk", "C:/Program Files/Npcap"] {
            let base = PathBuf::from(root);
            // Direct: C:\npcap\Lib\x64
            v.push(base.join(arch_dir));
            // Versioned subdirs: C:\npcap\npcap-sdk-1.13\Lib\x64
            if let Ok(entries) = std::fs::read_dir(&base) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("npcap-sdk") {
                        v.push(entry.path().join(arch_dir));
                    }
                }
            }
        }

        // User-local: %LOCALAPPDATA%\npcap-sdk
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            v.push(PathBuf::from(local).join("npcap-sdk").join(arch_dir));
        }

        v
    };

    for dir in &candidates {
        if dir.join("wpcap.lib").exists() {
            eprintln!("cargo:warning=Found Npcap SDK at {}", dir.display());
            return dir.to_string_lossy().into_owned();
        }
    }

    let searched = candidates
        .iter()
        .map(|p| format!("  - {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    panic!(
        "\n\n\
        ══════════════════════════════════════════════════════════════\n\
        Could not find the Npcap SDK (wpcap.lib).\n\n\
        Searched:\n{}\n\n\
        To fix this, either:\n\
        1. Set the NPCAP_SDK environment variable to the SDK root, e.g.\n\
           set NPCAP_SDK=C:\\npcap\\npcap-sdk-1.13\n\
        2. Place the extracted SDK in one of the paths listed above\n\
        3. Download from https://npcap.com/#download (SDK zip)\n\
        ══════════════════════════════════════════════════════════════\n",
        searched
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Without `scry` there is nothing to do; say so, or cargo would re-run
    // this script whenever any file in the package changes.
    if std::env::var_os("CARGO_FEATURE_SCRY").is_none() {
        println!("cargo:rerun-if-changed=build.rs");
        return Ok(());
    }
    #[cfg(feature = "scry")]
    scry_main()?;
    Ok(())
}

#[cfg(feature = "scry")]
fn scry_main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        let lib_dir = find_npcap_sdk();
        println!("cargo:rustc-link-search=native={}", lib_dir);
    }

    // Provide a simple build timestamp to satisfy env usage in code.
    let build_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    println!("cargo:rustc-env=VERGEN_BUILD_TIMESTAMP={}", build_timestamp);

    // Configure Git metadata emission.
    // When installed from a crate tarball (cargo install) there is no .git
    // directory, so vergen-git2 will fail. Fall back to placeholder values
    // so the build still succeeds.
    let git_ok = (|| -> Result<(), Box<dyn std::error::Error>> {
        let git2 = Git2Builder::default()
            .commit_timestamp(true)
            .describe(true, false, None)
            .sha(true)
            .branch(true)
            .build()?;
        Emitter::default().add_instructions(&git2)?.emit()?;
        Ok(())
    })();

    if git_ok.is_err() {
        println!("cargo:rustc-env=VERGEN_GIT_COMMIT_TIMESTAMP=unknown");
        println!("cargo:rustc-env=VERGEN_GIT_DESCRIBE=unknown");
        println!("cargo:rustc-env=VERGEN_GIT_SHA=unknown");
        println!("cargo:rustc-env=VERGEN_GIT_BRANCH=unknown");
    }

    Ok(())
}
