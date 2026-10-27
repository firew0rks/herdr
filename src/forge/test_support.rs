use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Writes `script` as an executable `gh` in a fresh `<home>/.local/bin` and
/// returns that directory.
#[cfg(unix)]
pub(super) fn fake_gh(script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let bin = std::env::temp_dir()
        .join(format!(
            "herdr-forge-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .join("home/.local/bin");
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The fake `gh` in `bin`, running with the system PATH so its script works.
#[cfg(unix)]
pub(super) fn gh_in(bin: &std::path::Path) -> super::gh::Gh {
    let system = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&system));
    super::gh::Gh {
        program: bin.join("gh"),
        path: std::env::join_paths(dirs).unwrap(),
    }
}
