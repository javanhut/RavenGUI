//! Where the compositor puts the files it makes, and what it calls them.
//!
//! Screenshots go under the user's pictures directory and recordings under
//! their videos directory, each as the user-dirs specification names it: the
//! exported variable if there is one, otherwise the value in `user-dirs.dirs`,
//! otherwise the usual folder in the home directory. Shared so the two cannot
//! disagree about how that lookup goes, or about how a file made this second
//! is named.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A user directory — `XDG_PICTURES_DIR`, `XDG_VIDEOS_DIR` — resolved.
///
/// `fallback` is the folder under the home directory used when neither the
/// environment nor `user-dirs.dirs` names one. `None` only when there is no
/// `HOME` and nothing to anchor a relative path to, which is the one case
/// there is nowhere sensible to put a file.
pub(crate) fn user_dir(key: &str, fallback: &str) -> Option<PathBuf> {
    let user_dirs = read_user_dirs();
    resolve(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os(key).as_deref(),
        user_dirs.as_deref(),
        key,
        fallback,
    )
}

/// The directory, from the environment and the `user-dirs.dirs` contents.
/// Split out with the inputs passed in so the policy is testable without
/// touching the real environment.
fn resolve(
    home: Option<&OsStr>,
    exported: Option<&OsStr>,
    user_dirs: Option<&str>,
    key: &str,
    fallback: &str,
) -> Option<PathBuf> {
    if let Some(dir) = exported.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = home.filter(|value| !value.is_empty())?;
    let home = Path::new(home);
    if let Some(relative) = user_dirs.and_then(|contents| parse_user_dirs(contents, key)) {
        return Some(expand_home(&relative, home));
    }
    Some(home.join(fallback))
}

/// The contents of `user-dirs.dirs`, from `$XDG_CONFIG_HOME` or `~/.config`.
fn read_user_dirs() -> Option<String> {
    let path = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(config) => PathBuf::from(config).join("user-dirs.dirs"),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config/user-dirs.dirs"),
    };
    std::fs::read_to_string(path).ok()
}

/// The `KEY="..."` value from a `user-dirs.dirs` file, unquoted.
///
/// The format is shell assignments; the value is double-quoted and usually
/// begins with `$HOME`. Comments and other keys are skipped.
fn parse_user_dirs(contents: &str, key: &str) -> Option<String> {
    for line in contents.lines() {
        let Some(rest) = line
            .trim()
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
        else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        if value.is_empty() {
            return None;
        }
        return Some(value.to_owned());
    }
    None
}

/// Expand a leading `$HOME` (or `~`) in a user-dirs value against `home`.
fn expand_home(value: &str, home: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix("$HOME/") {
        home.join(rest)
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if value == "$HOME" || value == "~" {
        home.to_path_buf()
    } else {
        PathBuf::from(value)
    }
}

/// A path for `stem.extension` in `dir` that is not already taken.
///
/// A second file in the same second gets a `-2` suffix rather than
/// overwriting the first.
pub(crate) fn unique_path(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return first;
    }
    for n in 2.. {
        let candidate = dir.join(format!("{stem}-{n}.{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the integers do not run out")
}

/// A `<prefix>-YYYY-MM-DD-HHMMSS` stem for a file made now, in UTC.
///
/// UTC rather than local time because turning a Unix timestamp into local time
/// needs the zone database and a C library call, and a compositor that forbids
/// unsafe is not going to reach for `localtime` to name a file. The name is for
/// telling two files apart, which UTC does exactly as well.
pub(crate) fn timestamp(prefix: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = civil_utc(secs);
    format!("{prefix}-{y:04}-{mo:02}-{d:02}-{h:02}{mi:02}{s:02}")
}

/// Broken-down UTC time from a Unix timestamp: `(year, month, day, hour, min,
/// sec)`. Howard Hinnant's `civil_from_days`, which is exact and needs no zone
/// data.
fn civil_utc(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PICTURES: &str = "XDG_PICTURES_DIR";

    fn pictures(
        home: Option<&str>,
        exported: Option<&str>,
        user_dirs: Option<&str>,
    ) -> Option<PathBuf> {
        resolve(
            home.map(OsStr::new),
            exported.map(OsStr::new),
            user_dirs,
            PICTURES,
            "Pictures",
        )
    }

    #[test]
    fn an_exported_dir_wins() {
        let base = pictures(
            Some("/home/person"),
            Some("/photos"),
            Some("XDG_PICTURES_DIR=\"$HOME/Pictures\"\n"),
        );
        assert_eq!(base, Some(PathBuf::from("/photos")));
    }

    #[test]
    fn an_empty_exported_dir_is_ignored() {
        // A shell that exported the variable blank has said nothing, not "put it
        // at the filesystem root".
        let base = pictures(Some("/home/person"), Some(""), None);
        assert_eq!(base, Some(PathBuf::from("/home/person/Pictures")));
    }

    #[test]
    fn user_dirs_is_consulted_and_home_expanded() {
        let base = pictures(
            Some("/home/person"),
            None,
            Some(
                "# generated\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\nXDG_PICTURES_DIR=\"$HOME/Bilder\"\n",
            ),
        );
        assert_eq!(base, Some(PathBuf::from("/home/person/Bilder")));
    }

    #[test]
    fn each_key_reads_its_own_line() {
        let contents = "XDG_PICTURES_DIR=\"$HOME/Bilder\"\nXDG_VIDEOS_DIR=\"$HOME/Filme\"\n";
        let videos = resolve(
            Some(OsStr::new("/home/person")),
            None,
            Some(contents),
            "XDG_VIDEOS_DIR",
            "Videos",
        );
        assert_eq!(videos, Some(PathBuf::from("/home/person/Filme")));
        // A key that is a prefix of another line's key does not match it.
        assert_eq!(
            parse_user_dirs("XDG_VIDEOS_DIRS=\"/x\"\n", "XDG_VIDEOS_DIR"),
            None
        );
    }

    #[test]
    fn without_user_dirs_it_falls_back_to_the_named_folder() {
        let base = pictures(Some("/home/person"), None, None);
        assert_eq!(base, Some(PathBuf::from("/home/person/Pictures")));
        let videos = resolve(
            Some(OsStr::new("/home/person")),
            None,
            None,
            "XDG_VIDEOS_DIR",
            "Videos",
        );
        assert_eq!(videos, Some(PathBuf::from("/home/person/Videos")));
    }

    #[test]
    fn with_no_home_there_is_nowhere_to_put_it() {
        assert_eq!(pictures(None, None, None), None);
        // ...unless an absolute dir was exported, which needs no home.
        assert_eq!(
            pictures(None, Some("/shots"), None),
            Some(PathBuf::from("/shots"))
        );
    }

    #[test]
    fn an_absolute_user_dirs_value_is_left_alone() {
        assert_eq!(
            expand_home("/mnt/pics", Path::new("/home/person")),
            PathBuf::from("/mnt/pics")
        );
    }

    #[test]
    fn known_timestamps_break_down_correctly() {
        // 2023-11-14T22:13:20Z
        assert_eq!(civil_utc(1_700_000_000), (2023, 11, 14, 22, 13, 20));
        // The epoch itself.
        assert_eq!(civil_utc(0), (1970, 1, 1, 0, 0, 0));
        // A leap day: 2024-02-29T12:00:00Z.
        assert_eq!(civil_utc(1_709_208_000), (2024, 2, 29, 12, 0, 0));
    }
}
