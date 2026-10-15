//! Finds the command-line tools the app shells out to.
//!
//! Launched from Finder or the Dock, a `.app` gets launchd's bare `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin`), not the shell's, so Homebrew installs
//! such as `gh` are invisible. The usual Homebrew prefixes are searched too.

use std::env;
use std::path::{Path, PathBuf};

/// Homebrew on Apple Silicon, then on Intel.
const EXTRA_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// The full path to `name`, or `name` itself so the error that follows says
/// which tool is missing.
pub fn path(name: &str) -> PathBuf {
    let path_var = env::var_os("PATH").unwrap_or_default();
    let dirs = env::split_paths(&path_var).chain(EXTRA_DIRS.iter().map(PathBuf::from));
    find(name, dirs).unwrap_or_else(|| PathBuf::from(name))
}

fn find(name: &str, dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_an_executable_in_the_first_dir_that_has_it() {
        let dirs = [PathBuf::from("/nonexistent"), PathBuf::from("/bin")];

        assert_eq!(find("sh", dirs), Some(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn a_missing_tool_falls_back_to_its_bare_name() {
        assert_eq!(
            path("surely-not-installed-tool"),
            PathBuf::from("surely-not-installed-tool")
        );
    }
}
