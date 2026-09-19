//! What type a file is.
//!
//! The shared-mime-info database under `<data dir>/mime` is read when it is
//! there -- `globs2` for names, `aliases` and `subclasses` for how types
//! relate -- because it is what every other program on the system agrees
//! with. It is data, not a program: nothing is run. When it is missing, a
//! table of common extensions and a handful of content signatures compiled
//! into this binary still give the answer for everything a desktop routinely
//! opens, so a damaged or absent database degrades to "less precise", never to
//! "opens nothing".

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Directories.
pub const DIRECTORY: &str = "inode/directory";
/// Text of no more specific kind.
pub const TEXT: &str = "text/plain";
/// Bytes of no known kind.
pub const BINARY: &str = "application/octet-stream";

/// The loaded type database.
#[derive(Debug, Default)]
pub struct MimeDb {
    globs: Vec<Glob>,
    aliases: HashMap<String, String>,
    parents: HashMap<String, Vec<String>>,
}

#[derive(Debug)]
struct Glob {
    weight: u32,
    mime: String,
    pattern: String,
    case_sensitive: bool,
}

impl MimeDb {
    /// Load `<dir>/mime/{globs2,aliases,subclasses}` from each of `share_dirs`,
    /// most significant first. Missing files are skipped.
    pub fn load(share_dirs: &[PathBuf]) -> Self {
        let mut db = Self::default();
        for dir in share_dirs {
            let mime = dir.join("mime");
            if let Ok(text) = std::fs::read_to_string(mime.join("globs2")) {
                db.add_globs(&text);
            }
            if let Ok(text) = std::fs::read_to_string(mime.join("aliases")) {
                for (alias, canonical) in pairs(&text) {
                    db.aliases.entry(alias).or_insert(canonical);
                }
            }
            if let Ok(text) = std::fs::read_to_string(mime.join("subclasses")) {
                for (child, parent) in pairs(&text) {
                    let parents = db.parents.entry(child).or_default();
                    if !parents.contains(&parent) {
                        parents.push(parent);
                    }
                }
            }
        }
        db
    }

    fn add_globs(&mut self, text: &str) {
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            // weight:type:pattern[:flags[:…]]
            let mut fields = line.splitn(4, ':');
            let (Some(weight), Some(mime), Some(pattern)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            // `__NOGLOBS__` means a later directory removed this type's
            // globs; there is nothing to match.
            if pattern == "__NOGLOBS__" {
                continue;
            }
            let Ok(weight) = weight.parse() else {
                continue;
            };
            let flags = fields.next().unwrap_or_default();
            self.globs.push(Glob {
                weight,
                mime: mime.to_owned(),
                pattern: pattern.to_owned(),
                case_sensitive: flags.split(',').any(|f| f == "cs"),
            });
        }
    }

    /// The type of the file or directory at `path`.
    pub fn type_of(&self, path: &Path) -> String {
        let Ok(meta) = std::fs::metadata(path) else {
            return BINARY.to_owned();
        };
        if meta.is_dir() {
            return DIRECTORY.to_owned();
        }
        if !meta.is_file() {
            // Sockets, pipes and devices have no application to open them.
            return BINARY.to_owned();
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let named = self.by_name(&name).map(|m| self.canonical(&m));
        if let Some(mime) = &named
            && !is_binary_media(mime)
        {
            return mime.clone();
        }
        let mut head = Vec::with_capacity(512);
        if let Ok(file) = std::fs::File::open(path) {
            let _ = file.take(512).read_to_end(&mut head);
        }
        match named {
            // A name claiming a binary media format over content that is
            // plain text is a name that collides: `go.mod` is not a tracker
            // module, whatever `*.mod` says. None of these formats is ever
            // valid text, so the content can safely overrule the name.
            Some(_) if !head.is_empty() && sniff(&head) == TEXT => TEXT.to_owned(),
            Some(mime) => mime,
            None => sniff(&head).to_owned(),
        }
    }

    /// The type a file name implies, if any: the highest-weighted matching
    /// glob, the longest pattern on a tie (`*.tar.gz` over `*.gz`), then the
    /// built-in extension table.
    pub fn by_name(&self, name: &str) -> Option<String> {
        let lower = name.to_lowercase();
        let best = self
            .globs
            .iter()
            .filter(|g| {
                if g.case_sensitive {
                    glob_match(&g.pattern, name)
                } else {
                    glob_match(&g.pattern.to_lowercase(), &lower)
                }
            })
            .max_by_key(|g| (g.weight, g.pattern.len()));
        if let Some(glob) = best {
            return Some(glob.mime.clone());
        }
        let (_, ext) = lower.rsplit_once('.')?;
        BUILTIN_EXTENSIONS
            .iter()
            .find(|(e, _)| *e == ext)
            .map(|(_, mime)| (*mime).to_owned())
    }

    /// The canonical name of `mime`, if it is an alias.
    pub fn canonical(&self, mime: &str) -> String {
        self.aliases
            .get(mime)
            .cloned()
            .unwrap_or_else(|| mime.to_owned())
    }

    /// `mime` and then every type it is a kind of, nearest first:
    /// `text/x-python` → `text/x-script`… → `text/plain`.
    ///
    /// Every text type ends at `text/plain` even without a database, so a
    /// source file always has at least the editor. Nothing ends at
    /// `application/octet-stream`, although the spec makes it every type's
    /// ancestor: a picture that falls through to a hex editor has not been
    /// opened in any useful sense.
    pub fn lineage(&self, mime: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([self.canonical(mime)]);
        while let Some(t) = queue.pop_front() {
            if !seen.insert(t.clone()) {
                continue;
            }
            for parent in self.parents.get(&t).into_iter().flatten() {
                let parent = self.canonical(parent);
                if !NEVER_INHERITED.contains(&parent.as_str()) {
                    queue.push_back(parent);
                }
            }
            for (child, parent) in BUILTIN_PARENTS {
                if *child == t {
                    queue.push_back((*parent).to_owned());
                }
            }
            if t.starts_with("text/") && t != TEXT {
                queue.push_back(TEXT.to_owned());
            }
            out.push(t);
        }
        // text/plain last, whatever order the parents were found in: it is
        // the least specific answer on offer.
        if let Some(i) = out.iter().position(|t| t == TEXT)
            && i + 1 != out.len()
        {
            let text = out.remove(i);
            out.push(text);
        }
        out
    }
}

/// Types a file is never opened *as* merely because it descends from them.
///
/// shared-mime-info makes scripts (`text/x-python`, …) subclasses of
/// `application/x-executable`, which is true of their content and dangerous
/// as a fallback: whatever registers for executables runs them. Opening a
/// script edits it. Only a file that *is* this type is looked up as it.
const NEVER_INHERITED: &[&str] = &["application/x-executable", "application/x-sharedlib"];

/// Audio, video and raster image types: binary formats a text file can never
/// be. SVG is the exception -- it is XML, and text.
fn is_binary_media(mime: &str) -> bool {
    (mime.starts_with("audio/") || mime.starts_with("video/") || mime.starts_with("image/"))
        && !mime.starts_with("image/svg")
}

/// Whitespace-separated pairs, one per line.
fn pairs(text: &str) -> impl Iterator<Item = (String, String)> + '_ {
    text.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        Some((words.next()?.to_owned(), words.next()?.to_owned()))
    })
}

/// A shell glob: `*`, `?` and `[…]` (with `!` negation and ranges).
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    matches(&p, &n)
}

fn matches(p: &[char], n: &[char]) -> bool {
    match p.first() {
        None => n.is_empty(),
        // Collapse runs of `*` and try every split; names are short.
        Some('*') => {
            let rest = &p[1..];
            (0..=n.len()).any(|i| matches(rest, &n[i..]))
        }
        Some('?') => !n.is_empty() && matches(&p[1..], &n[1..]),
        Some('[') => {
            let Some(close) = p.iter().skip(1).position(|&c| c == ']').map(|i| i + 1) else {
                // An unclosed bracket is a literal `[`.
                return n.first() == Some(&'[') && matches(&p[1..], &n[1..]);
            };
            let Some(&c) = n.first() else { return false };
            let mut set = &p[1..close];
            let negate = set.first() == Some(&'!');
            if negate {
                set = &set[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == '-' {
                    hit |= (set[i]..=set[i + 2]).contains(&c);
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            hit != negate && matches(&p[close + 1..], &n[1..])
        }
        Some(&c) => n.first() == Some(&c) && matches(&p[1..], &n[1..]),
    }
}

/// The type of a file's first bytes, for a name that says nothing.
pub fn sniff(head: &[u8]) -> &'static str {
    for (offset, magic, mime) in BUILTIN_MAGIC {
        if head.get(*offset..offset + magic.len()) == Some(*magic) {
            return mime;
        }
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return "image/webp";
    }
    if head.starts_with(b"#!") {
        let line = head.split(|&b| b == b'\n').next().unwrap_or_default();
        let line = String::from_utf8_lossy(line);
        return if line.contains("python") {
            "text/x-python"
        } else {
            "application/x-shellscript"
        };
    }
    // Text is what decodes and has no NULs. An empty file is text: it is
    // most likely about to be written, and an editor is what does that.
    let text = !head.contains(&0)
        && match std::str::from_utf8(head) {
            Ok(_) => true,
            // A multi-byte character cut off by the 512-byte read is fine.
            Err(e) => e.error_len().is_none(),
        };
    if text { TEXT } else { BINARY }
}

/// Extensions every desktop opens, for when the database is absent.
const BUILTIN_EXTENSIONS: &[(&str, &str)] = &[
    ("html", "text/html"),
    ("htm", "text/html"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("bmp", "image/bmp"),
    ("ico", "image/vnd.microsoft.icon"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("avif", "image/avif"),
    ("heic", "image/heic"),
    ("jxl", "image/jxl"),
    ("mp4", "video/mp4"),
    ("m4v", "video/x-m4v"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("avi", "video/x-msvideo"),
    ("mp3", "audio/mpeg"),
    ("flac", "audio/flac"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/opus"),
    ("wav", "audio/x-wav"),
    ("m4a", "audio/mp4"),
    ("zip", "application/zip"),
    ("tar", "application/x-tar"),
    ("gz", "application/gzip"),
    ("xz", "application/x-xz"),
    ("zst", "application/zstd"),
    ("json", "application/json"),
    ("xml", "application/xml"),
    ("toml", "application/toml"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("sh", "application/x-shellscript"),
    ("py", "text/x-python"),
    ("rs", "text/rust"),
    ("go", "text/x-go"),
    ("c", "text/x-csrc"),
    ("h", "text/x-chdr"),
    ("desktop", "application/x-desktop"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
];

/// Types that are text underneath without saying `text/`, for when the
/// database's `subclasses` is absent.
const BUILTIN_PARENTS: &[(&str, &str)] = &[
    ("application/json", TEXT),
    ("application/xml", TEXT),
    ("application/toml", TEXT),
    ("application/yaml", TEXT),
    ("application/x-shellscript", TEXT),
    ("application/x-desktop", TEXT),
    ("image/svg+xml", "application/xml"),
];

/// `(offset, bytes, type)` signatures, checked in order.
const BUILTIN_MAGIC: &[(usize, &[u8], &str)] = &[
    (0, b"%PDF-", "application/pdf"),
    (0, b"\x89PNG\r\n\x1a\n", "image/png"),
    (0, b"\xff\xd8\xff", "image/jpeg"),
    (0, b"GIF87a", "image/gif"),
    (0, b"GIF89a", "image/gif"),
    (0, b"\x7fELF", "application/x-executable"),
    (0, b"PK\x03\x04", "application/zip"),
    (0, b"\x1f\x8b", "application/gzip"),
    (0, b"\xfd7zXZ\x00", "application/x-xz"),
    (0, b"(\xb5/\xfd", "application/zstd"),
    (0, b"OggS", "audio/ogg"),
    (0, b"fLaC", "audio/flac"),
    (0, b"ID3", "audio/mpeg"),
    (0, b"\x1aE\xdf\xa3", "video/x-matroska"),
    (4, b"ftyp", "video/mp4"),
    (0, b"<!DOCTYPE html", "text/html"),
    (0, b"<!doctype html", "text/html"),
    (0, b"<html", "text/html"),
    (0, b"<?xml", "application/xml"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn db(globs: &str, aliases: &str, subclasses: &str) -> MimeDb {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("raven-open-mime-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("mime")).unwrap();
        std::fs::write(dir.join("mime/globs2"), globs).unwrap();
        std::fs::write(dir.join("mime/aliases"), aliases).unwrap();
        std::fs::write(dir.join("mime/subclasses"), subclasses).unwrap();
        MimeDb::load(&[dir])
    }

    #[test]
    fn the_heaviest_then_longest_glob_wins() {
        let db = db(
            "# comment\n50:application/gzip:*.gz\n50:application/x-compressed-tar:*.tar.gz\n\
             80:text/html:*.html\n40:text/plain:*.html\n",
            "",
            "",
        );
        assert_eq!(
            db.by_name("a.tar.gz").as_deref(),
            Some("application/x-compressed-tar")
        );
        assert_eq!(db.by_name("b.gz").as_deref(), Some("application/gzip"));
        assert_eq!(db.by_name("INDEX.HTML").as_deref(), Some("text/html"));
    }

    #[test]
    fn case_sensitive_globs_only_match_their_case() {
        let db = db(
            "50:text/x-genie:*.gs:cs\n50:application/x-core:core:cs\n",
            "",
            "",
        );
        assert_eq!(db.by_name("x.gs").as_deref(), Some("text/x-genie"));
        assert_eq!(db.by_name("x.GS"), None);
        assert_eq!(db.by_name("core").as_deref(), Some("application/x-core"));
    }

    #[test]
    fn without_a_database_the_builtin_table_answers() {
        let db = MimeDb::default();
        assert_eq!(db.by_name("Report.PDF").as_deref(), Some("application/pdf"));
        assert_eq!(db.by_name("clip.mkv").as_deref(), Some("video/x-matroska"));
        assert_eq!(db.by_name("README"), None);
    }

    #[test]
    fn aliases_resolve_to_the_canonical_type() {
        let db = db("", "application/acrobat application/pdf\n", "");
        assert_eq!(db.canonical("application/acrobat"), "application/pdf");
        assert_eq!(db.canonical("application/pdf"), "application/pdf");
    }

    #[test]
    fn lineage_walks_parents_and_ends_at_text_but_never_at_executable() {
        let db = db(
            "",
            "",
            "text/x-python3 text/x-python\ntext/x-python application/x-executable\n\
             text/x-python text/plain\n",
        );
        assert_eq!(
            db.lineage("text/x-python3"),
            ["text/x-python3", "text/x-python", "text/plain"]
        );
    }

    #[test]
    fn builtin_parents_make_config_formats_text() {
        let db = MimeDb::default();
        assert_eq!(
            db.lineage("application/json"),
            ["application/json", "text/plain"]
        );
        assert_eq!(
            db.lineage("image/svg+xml"),
            ["image/svg+xml", "application/xml", "text/plain"]
        );
        assert_eq!(db.lineage("image/png"), ["image/png"]);
    }

    #[test]
    fn globs_support_classes_and_wildcards() {
        assert!(glob_match("*.[ch]", "main.c"));
        assert!(!glob_match("*.[!ch]", "main.c"));
        assert!(glob_match("*.[a-z]z", "x.gz"));
        assert!(glob_match("Makefile.*", "Makefile.am"));
        assert!(glob_match("?akefile", "Makefile"));
        assert!(!glob_match("*.txt", "txt"));
    }

    #[test]
    fn content_is_sniffed_when_the_name_says_nothing() {
        assert_eq!(sniff(b"%PDF-1.7\n"), "application/pdf");
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), "image/png");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(sniff(b"\0\0\0\x18ftypmp42"), "video/mp4");
        assert_eq!(sniff(b"#!/usr/bin/env python3\n"), "text/x-python");
        assert_eq!(sniff(b"#!/bin/sh\necho\n"), "application/x-shellscript");
        assert_eq!(sniff(b"plain words\n"), TEXT);
        assert_eq!(sniff(b""), TEXT);
        assert_eq!(sniff(b"\x00\x01\x02"), BINARY);
        // "é" cut in half by the read limit is still text.
        assert_eq!(sniff(b"caf\xc3"), TEXT);
    }

    #[test]
    fn a_text_file_named_like_media_is_text() {
        let dir =
            std::env::temp_dir().join(format!("raven-open-mime-clash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("go.mod"), "module example.com/x\n\ngo 1.22\n").unwrap();
        std::fs::write(dir.join("song.mod"), b"M.K.\x00\x01\x02\xff").unwrap();
        std::fs::write(dir.join("empty.mp3"), b"").unwrap();
        let db = db("50:audio/x-mod:*.mod\n", "", "");
        assert_eq!(db.type_of(&dir.join("go.mod")), TEXT);
        assert_eq!(db.type_of(&dir.join("song.mod")), "audio/x-mod");
        // Nothing to go on but the name, so the name stands.
        assert_eq!(db.type_of(&dir.join("empty.mp3")), "audio/mpeg");
    }

    #[test]
    fn directories_and_files_on_disk() {
        let dir = std::env::temp_dir().join(format!("raven-open-mime-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("noext"), b"%PDF-1.4").unwrap();
        let db = MimeDb::default();
        assert_eq!(db.type_of(&dir), DIRECTORY);
        assert_eq!(db.type_of(&dir.join("noext")), "application/pdf");
    }
}
