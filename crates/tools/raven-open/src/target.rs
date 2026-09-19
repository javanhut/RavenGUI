//! What was asked to be opened: a URL, or a file on this machine.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// A classified argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A URL for some other program to fetch: `https://…`, `mailto:…`.
    /// Opened by whatever handles `x-scheme-handler/<scheme>`.
    Url {
        /// The URL exactly as given.
        url: String,
        /// Its scheme, lowercased.
        scheme: String,
    },
    /// A file or directory that exists, as an absolute path.
    File(PathBuf),
}

/// Why an argument names nothing that can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// An empty argument.
    Empty,
    /// A path, or a `file://` URL, naming nothing on disk.
    Missing(PathBuf),
    /// A `file://` URL for another machine, which this one cannot open.
    RemoteFile(String),
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("nothing to open"),
            Self::Missing(path) => write!(f, "{}: no such file or directory", path.display()),
            Self::RemoteFile(url) => write!(f, "{url}: a file on another host"),
        }
    }
}

/// Classify one argument, resolving relative paths against `cwd`.
///
/// A file that exists wins over a URL reading of the same text, so a file
/// called `notes:today` opens as the file -- which is what `xdg-open` does
/// too. Anything else with a scheme is a URL; anything without one is a path
/// that is missing.
pub fn classify(arg: &str, cwd: &Path) -> Result<Target, TargetError> {
    if arg.is_empty() {
        return Err(TargetError::Empty);
    }

    if let Some(scheme) = scheme(arg)
        && scheme == "file"
    {
        let path = file_url_path(arg)?;
        return if path.exists() {
            Ok(Target::File(path))
        } else {
            Err(TargetError::Missing(path))
        };
    }

    let path = cwd.join(arg);
    if path.exists() {
        return Ok(Target::File(path));
    }
    match scheme(arg) {
        Some(scheme) => Ok(Target::Url {
            url: arg.to_owned(),
            scheme,
        }),
        None => Err(TargetError::Missing(path)),
    }
}

/// The scheme of `arg`, lowercased, if it starts with one: a letter, then
/// letters, digits, `+`, `-` or `.`, then `:` (RFC 3986).
fn scheme(arg: &str) -> Option<String> {
    let (scheme, _) = arg.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then(|| scheme.to_ascii_lowercase())
}

/// The local path a `file:` URL names, percent-decoded.
///
/// `file:///p` and `file://localhost/p` are this machine; any other host is
/// not, and is refused rather than quietly opened as a local path.
fn file_url_path(url: &str) -> Result<PathBuf, TargetError> {
    let rest = &url["file:".len()..];
    let path = match rest.strip_prefix("//") {
        Some(authority_and_path) => {
            let (host, path) = match authority_and_path.find('/') {
                Some(i) => authority_and_path.split_at(i),
                None => (authority_and_path, ""),
            };
            if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                return Err(TargetError::RemoteFile(url.to_owned()));
            }
            path
        }
        // `file:/p`, which some programs write.
        None => rest,
    };
    // A query or fragment is not part of a file's name.
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if path.is_empty() {
        return Err(TargetError::Missing(PathBuf::from("/")));
    }
    Ok(PathBuf::from(OsString::from_vec(percent_decode(path))))
}

/// `%XX` escapes to bytes. A malformed escape is kept literally, as browsers
/// do, rather than failing the whole URL.
fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("raven-open-target-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_web_link_is_a_url_with_its_scheme() {
        assert_eq!(
            classify("HTTPS://example.com/a?b", Path::new("/")),
            Ok(Target::Url {
                url: "HTTPS://example.com/a?b".into(),
                scheme: "https".into()
            })
        );
        assert!(matches!(
            classify("mailto:someone@example.com", Path::new("/")),
            Ok(Target::Url { scheme, .. }) if scheme == "mailto"
        ));
    }

    #[test]
    fn a_relative_path_resolves_against_the_working_directory() {
        let dir = tmp("rel");
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        assert_eq!(
            classify("notes.txt", &dir),
            Ok(Target::File(dir.join("notes.txt")))
        );
    }

    #[test]
    fn an_existing_file_wins_over_a_url_reading() {
        let dir = tmp("colon");
        std::fs::write(dir.join("notes:today"), "x").unwrap();
        assert_eq!(
            classify("notes:today", &dir),
            Ok(Target::File(dir.join("notes:today")))
        );
    }

    #[test]
    fn a_missing_path_is_reported_as_missing() {
        assert_eq!(
            classify("no-such-file", Path::new("/nonexistent")),
            Err(TargetError::Missing(PathBuf::from(
                "/nonexistent/no-such-file"
            )))
        );
        assert_eq!(classify("", Path::new("/")), Err(TargetError::Empty));
    }

    #[test]
    fn file_urls_are_decoded_to_local_paths() {
        let dir = tmp("fileurl");
        std::fs::write(dir.join("a b.txt"), "x").unwrap();
        let url = format!("file://{}/a%20b.txt", dir.display());
        assert_eq!(
            classify(&url, Path::new("/")),
            Ok(Target::File(dir.join("a b.txt")))
        );
        let url = format!("file://localhost{}/a%20b.txt#frag", dir.display());
        assert_eq!(
            classify(&url, Path::new("/")),
            Ok(Target::File(dir.join("a b.txt")))
        );
    }

    #[test]
    fn a_file_url_for_another_host_is_refused() {
        assert_eq!(
            classify("file://server/share/x", Path::new("/")),
            Err(TargetError::RemoteFile("file://server/share/x".into()))
        );
    }

    #[test]
    fn a_malformed_escape_is_kept_literally() {
        assert_eq!(percent_decode("100%zz%41"), b"100%zzA");
    }

    #[test]
    fn scheme_rules_follow_rfc_3986() {
        assert_eq!(scheme("git+ssh://h/r").as_deref(), Some("git+ssh"));
        assert_eq!(scheme("1http://x"), None);
        assert_eq!(scheme("no scheme: here"), None);
        assert_eq!(scheme("plain"), None);
    }
}
