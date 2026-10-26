//! Start-up check for the packet-capture runtime (Npcap) on Windows.
//!
//! The scry binaries ship in the `spvirit-tools` wheel, but the capture
//! runtime does not: Npcap's licence forbids redistributing it. On Windows the
//! binaries delay-load `wpcap.dll` (see `build.rs`), so a machine without
//! Npcap still reaches `main`, and [`ensure_capture_runtime`] can explain what
//! is missing instead of Windows showing a "wpcap.dll was not found" dialog.
//!
//! Call it after argument parsing (so `--help` and `--version` always work)
//! and before the first pcap call.
//!
//! `SPVIRIT_NPCAP_DIR` (Windows only) replaces the search: when set, only
//! that directory is checked for `wpcap.dll`. It exists so tests can force
//! the missing-runtime path without uninstalling Npcap.

use std::path::{Path, PathBuf};

/// Environment variable that overrides the Windows Npcap search.
pub const NPCAP_DIR_ENV: &str = "SPVIRIT_NPCAP_DIR";

/// Where the Npcap runtime can be downloaded.
pub const NPCAP_URL: &str = "https://npcap.com/#download";

/// Outcome of the Windows search for `wpcap.dll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// `wpcap.dll` is in this directory (Npcap's own folder, or the
    /// override); it must be added to the DLL search path before use.
    Dir(PathBuf),
    /// `wpcap.dll` already loads from the normal DLL search path (Npcap's
    /// WinPcap-compatible mode, or a legacy WinPcap install).
    SearchPath,
    /// Not found; the directories that were checked.
    Missing { looked_in: Vec<PathBuf> },
}

/// Decide where `wpcap.dll` comes from.
///
/// Pure apart from the injected probes, so it is unit-testable:
/// - `override_dir`: the value of [`NPCAP_DIR_ENV`]; when present it is the
///   only place checked (no fallback to the search path).
/// - `system_root`: `%SystemRoot%`; Npcap installs into `System32\Npcap`.
/// - `exists`: file-existence probe.
/// - `on_search_path`: whether `wpcap.dll` loads from the default search path.
pub fn detect(
    override_dir: Option<&Path>,
    system_root: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
    on_search_path: impl FnOnce() -> bool,
) -> Detection {
    if let Some(dir) = override_dir {
        return if exists(&dir.join("wpcap.dll")) {
            Detection::Dir(dir.to_path_buf())
        } else {
            Detection::Missing {
                looked_in: vec![dir.to_path_buf()],
            }
        };
    }
    let mut looked_in = Vec::new();
    if let Some(root) = system_root {
        let npcap = root.join("System32").join("Npcap");
        if exists(&npcap.join("wpcap.dll")) {
            return Detection::Dir(npcap);
        }
        looked_in.push(npcap);
    }
    if on_search_path() {
        return Detection::SearchPath;
    }
    looked_in.push(PathBuf::from("the default DLL search path"));
    Detection::Missing { looked_in }
}

/// The warning printed when Npcap is missing on Windows.
pub fn npcap_missing_warning(tool: &str, looked_in: &[PathBuf]) -> String {
    let places = looked_in
        .iter()
        .map(|p| format!("  - {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "warning: {tool}: the Npcap runtime (wpcap.dll) was not found.\n\
         Looked in:\n{places}\n\
         {tool} captures packets and needs Npcap installed: {NPCAP_URL}\n\
         (install it with \"WinPcap API-compatible mode\" or the default options).\n\
         The other spvirit-tools (spget, spmonitor, spgateway, ...) work without it."
    )
}

/// Check that the capture runtime is usable, preparing the DLL search path on
/// Windows. On failure returns the warning to print; the caller exits.
pub fn ensure_capture_runtime(tool: &str) -> Result<(), String> {
    imp::ensure(tool)
}

/// Exit status when the capture runtime is missing.
pub const EXIT_NO_CAPTURE_RUNTIME: i32 = 3;

/// [`ensure_capture_runtime`], printing the warning to stderr and exiting
/// with [`EXIT_NO_CAPTURE_RUNTIME`] when the runtime is missing.
pub fn exit_unless_capture_runtime(tool: &str) {
    if let Err(warning) = ensure_capture_runtime(tool) {
        eprintln!("{warning}");
        std::process::exit(EXIT_NO_CAPTURE_RUNTIME);
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetDllDirectoryW(path: *const u16) -> i32;
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn loads_from_search_path() -> bool {
        let name = wide(OsStr::new("wpcap.dll"));
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the
        // call. The handle is deliberately kept: the delay-load helper will
        // load the same module moments later.
        !unsafe { LoadLibraryW(name.as_ptr()) }.is_null()
    }

    pub(super) fn ensure(tool: &str) -> Result<(), String> {
        let override_dir = std::env::var_os(NPCAP_DIR_ENV).map(PathBuf::from);
        let system_root = std::env::var_os("SystemRoot").map(PathBuf::from);
        match detect(
            override_dir.as_deref(),
            system_root.as_deref(),
            |p| p.is_file(),
            loads_from_search_path,
        ) {
            Detection::Dir(dir) => {
                let w = wide(dir.as_os_str());
                // SAFETY: `w` is NUL-terminated and outlives the call.
                if unsafe { SetDllDirectoryW(w.as_ptr()) } == 0 {
                    return Err(format!(
                        "warning: {tool}: could not add {} to the DLL search path",
                        dir.display()
                    ));
                }
                Ok(())
            }
            Detection::SearchPath => Ok(()),
            Detection::Missing { looked_in } => Err(npcap_missing_warning(tool, &looked_in)),
        }
    }
}

// Elsewhere libpcap is an ordinary load-time dependency (in the Linux wheel
// it is bundled, under a hashed file name, beside the binary), so if `main`
// runs the library is already loaded and there is nothing to check. A dlopen
// probe by name would wrongly fail on the bundled copy; a missing system
// libpcap is reported by the dynamic loader itself ("error while loading
// shared libraries: libpcap.so...").
#[cfg(not(windows))]
mod imp {
    pub(super) fn ensure(_tool: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(r"C:\Windows")
    }

    fn npcap_dir() -> PathBuf {
        root().join("System32").join("Npcap")
    }

    #[test]
    fn npcap_folder_is_preferred_over_the_search_path() {
        let want = npcap_dir().join("wpcap.dll");
        let got = detect(None, Some(&root()), |p| p == want, || {
            panic!("search path must not be probed when the Npcap folder has wpcap.dll")
        });
        assert_eq!(got, Detection::Dir(npcap_dir()));
    }

    #[test]
    fn falls_back_to_the_default_search_path() {
        let got = detect(None, Some(&root()), |_| false, || true);
        assert_eq!(got, Detection::SearchPath);
    }

    #[test]
    fn missing_everywhere_lists_where_it_looked() {
        let got = detect(None, Some(&root()), |_| false, || false);
        let Detection::Missing { looked_in } = got else {
            panic!("expected Missing, got {got:?}");
        };
        assert_eq!(looked_in[0], npcap_dir());
        let text = npcap_missing_warning("spvirit-scry", &looked_in);
        assert!(text.contains("Npcap runtime (wpcap.dll) was not found"), "{text}");
        assert!(text.contains(NPCAP_URL), "{text}");
        assert!(text.contains("other spvirit-tools"), "{text}");
        assert!(text.contains(&npcap_dir().display().to_string()), "{text}");
    }

    #[test]
    fn override_is_the_only_place_checked() {
        let dir = PathBuf::from(r"D:\nowhere");
        let got = detect(Some(&dir), Some(&root()), |_| false, || {
            panic!("override must not fall back to the search path")
        });
        assert_eq!(got, Detection::Missing { looked_in: vec![dir.clone()] });

        let want = dir.join("wpcap.dll");
        let got = detect(Some(&dir), Some(&root()), |p| p == want, || unreachable!());
        assert_eq!(got, Detection::Dir(dir));
    }

}
