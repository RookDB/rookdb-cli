//! Shared helpers for CLI integration tests.
//!
//! [`Workspace`] creates an isolated directory per test and removes it when
//! the guard drops — including on panic — so test runs never litter the
//! crate directory with `database*` workspaces.

use std::path::PathBuf;

pub struct Workspace {
    path: PathBuf,
}

impl Workspace {
    /// Create `<crate>/database_<pid>_<name>` fresh.
    pub fn new(name: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "database_p{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create workspace");
        Workspace { path }
    }

}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl std::ops::Deref for Workspace {
    type Target = str;
    fn deref(&self) -> &str {
        self.path.to_str().expect("workspace path is utf-8")
    }
}

impl AsRef<std::path::Path> for Workspace {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}
