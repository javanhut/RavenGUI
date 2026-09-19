//! Where the configuration and data live, read from the environment once.
//!
//! Everything else in the crate takes an [`Env`] rather than reading variables
//! itself, so tests describe a whole system with explicit paths instead of
//! setting process-global variables that would race every other test.

use std::ffi::OsStr;
use std::path::PathBuf;

/// `$XDG_DATA_DIRS` when unset, per the basedir specification.
const DEFAULT_DATA_DIRS: &str = "/usr/local/share:/usr/share";
/// `$XDG_CONFIG_DIRS` when unset.
const DEFAULT_CONFIG_DIRS: &str = "/etc/xdg";

/// The XDG base directories and desktop names that decide what opens what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    /// `$XDG_CONFIG_HOME`: where the user's own choices are written.
    pub config_home: PathBuf,
    /// `$XDG_CONFIG_DIRS`, most significant first.
    pub config_dirs: Vec<PathBuf>,
    /// `$XDG_DATA_HOME`.
    pub data_home: PathBuf,
    /// `$XDG_DATA_DIRS`, most significant first.
    pub data_dirs: Vec<PathBuf>,
    /// `$XDG_CURRENT_DESKTOP` split on `:`, as written (`Huginn`, `Raven`).
    pub desktops: Vec<String>,
}

impl Env {
    /// The running process's environment.
    ///
    /// An empty variable counts as unset: the basedir specification says so,
    /// and a session exporting `XDG_DATA_DIRS=` would otherwise find nothing
    /// installed at all.
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let home = var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            config_home: var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config")),
            config_dirs: split(var("XDG_CONFIG_DIRS").as_deref(), DEFAULT_CONFIG_DIRS),
            data_home: var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share")),
            data_dirs: split(var("XDG_DATA_DIRS").as_deref(), DEFAULT_DATA_DIRS),
            desktops: std::env::var("XDG_CURRENT_DESKTOP")
                .unwrap_or_default()
                .split(':')
                .filter(|d| !d.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }

    /// Data directories, most significant first: the user's, then the
    /// system's, each once.
    pub fn share_dirs(&self) -> Vec<PathBuf> {
        dedup(std::iter::once(self.data_home.clone()).chain(self.data_dirs.iter().cloned()))
    }

    /// Where `.desktop` files are installed, most significant first.
    pub fn app_dirs(&self) -> Vec<PathBuf> {
        self.share_dirs()
            .into_iter()
            .map(|d| d.join("applications"))
            .collect()
    }

    /// Every `mimeapps.list` that may hold a choice, most significant first,
    /// in the order the MIME Applications Associations specification gives:
    /// user config, system config, user data, system data -- and within each
    /// directory the desktop-specific `raven-mimeapps.list` before the plain
    /// one.
    pub fn mimeapps_lists(&self) -> Vec<PathBuf> {
        let dirs = std::iter::once(self.config_home.clone())
            .chain(self.config_dirs.iter().cloned())
            .chain(self.app_dirs());
        let names: Vec<String> = self
            .desktops
            .iter()
            .map(|d| format!("{}-mimeapps.list", d.to_lowercase()))
            .chain(std::iter::once("mimeapps.list".to_owned()))
            .collect();
        dedup(
            dirs.flat_map(|dir| names.iter().map(move |n| dir.join(n)))
                .collect::<Vec<_>>(),
        )
    }

    /// The file a choice made on this machine is written to.
    pub fn user_mimeapps(&self) -> PathBuf {
        self.config_home.join("mimeapps.list")
    }
}

fn split(value: Option<&OsStr>, default: &str) -> Vec<PathBuf> {
    let value = value.unwrap_or_else(|| OsStr::new(default));
    std::env::split_paths(value)
        .filter(|p| p.is_absolute())
        .collect()
}

fn dedup(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// An [`Env`] rooted entirely under `root`, for tests.
#[cfg(test)]
pub(crate) fn under(root: &std::path::Path, desktops: &[&str]) -> Env {
    Env {
        config_home: root.join("home/.config"),
        config_dirs: vec![root.join("etc/xdg")],
        data_home: root.join("home/.local/share"),
        data_dirs: vec![root.join("usr/local/share"), root.join("usr/share")],
        desktops: desktops.iter().map(|d| (*d).to_owned()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn mimeapps_lists_follow_the_spec_order() {
        let env = under(Path::new("/r"), &["Huginn", "Raven"]);
        let lists = env.mimeapps_lists();
        let first: Vec<_> = lists.iter().take(4).map(|p| p.to_str().unwrap()).collect();
        assert_eq!(
            first,
            [
                "/r/home/.config/huginn-mimeapps.list",
                "/r/home/.config/raven-mimeapps.list",
                "/r/home/.config/mimeapps.list",
                "/r/etc/xdg/huginn-mimeapps.list",
            ]
        );
        assert_eq!(
            lists.last().unwrap(),
            Path::new("/r/usr/share/applications/mimeapps.list")
        );
    }

    #[test]
    fn a_directory_listed_twice_is_searched_once() {
        let mut env = under(Path::new("/r"), &[]);
        env.data_dirs.push(PathBuf::from("/r/usr/share"));
        assert_eq!(env.app_dirs().len(), 3);
    }

    #[test]
    fn relative_entries_in_a_path_list_are_ignored() {
        // The basedir spec: relative paths are invalid and must be ignored.
        assert_eq!(
            split(Some(OsStr::new("relative:/abs")), DEFAULT_DATA_DIRS),
            [PathBuf::from("/abs")]
        );
    }
}
