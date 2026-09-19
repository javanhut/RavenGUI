//! `xdg-mime`: what type a file is, and what opens a type.
//!
//! ```sh
//! xdg-mime query filetype report.pdf              # application/pdf
//! xdg-mime query default application/pdf          # com.ravenviewer.Raven.desktop
//! xdg-mime default eagleeye.desktop image/png image/jpeg
//! ```
//!
//! Answers come from the same code `raven-open` uses to decide, so "what
//! would open this" and what then opens it cannot disagree. Installing new
//! MIME definitions (`xdg-mime install`) is the package manager's job on
//! Raven, which refreshes the database after a package brings one.

use std::io::Write;
use std::path::Path;

use raven_open::Env;
use raven_open::mime::MimeDb;

use crate::status;

const USAGE: &str = "\
usage: xdg-mime query filetype <file>
       xdg-mime query default <mimetype>
       xdg-mime default <app.desktop> <mimetype> [mimetype…]";

pub(crate) fn run(
    args: &[String],
    env: &Env,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--help" | "-h" | "--manual", ..] => {
            let _ = writeln!(out, "{USAGE}");
            status::OK
        }
        ["--version", ..] => crate::version("xdg-mime", out),
        ["query", "filetype", file] => {
            let path = cwd.join(file);
            if !path.exists() {
                let _ = writeln!(
                    err,
                    "xdg-mime: {}: no such file or directory",
                    path.display()
                );
                return status::MISSING;
            }
            let _ = writeln!(out, "{}", MimeDb::load(&env.share_dirs()).type_of(&path));
            status::OK
        }
        ["query", "default", mime] => {
            if let Some(id) = raven_open::default_for(mime, env) {
                let _ = writeln!(out, "{id}");
            }
            status::OK
        }
        ["default", id, mimes @ ..] if !mimes.is_empty() => set(id, mimes, env, err),
        ["install" | "uninstall", ..] => {
            let _ = writeln!(
                err,
                "xdg-mime: installing MIME types is done by rvn on Raven, not by xdg-mime"
            );
            status::NO_TOOL
        }
        _ => {
            let _ = writeln!(err, "{USAGE}");
            status::USAGE
        }
    }
}

fn set(id: &str, mimes: &[&str], env: &Env, err: &mut dyn Write) -> u8 {
    if !id.ends_with(".desktop") {
        let _ = writeln!(
            err,
            "xdg-mime: {id}: not a desktop file ID (expected name.desktop)"
        );
        return status::USAGE;
    }
    if let Some(bad) = mimes.iter().find(|m| !m.contains('/')) {
        let _ = writeln!(
            err,
            "xdg-mime: {bad}: not a MIME type (expected type/subtype)"
        );
        return status::USAGE;
    }
    if !raven_open::is_installed(id, env) {
        let _ = writeln!(err, "xdg-mime: {id}: no such application is installed");
        return status::MISSING;
    }
    for mime in mimes {
        if let Err(e) = raven_open::defaults::set_default(&env.user_mimeapps(), mime, id) {
            let _ = writeln!(err, "xdg-mime: could not set {id} for {mime}: {e}");
            return status::FAILED;
        }
    }
    status::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    fn mime(env: &Env, cwd: &Path, words: &[&str]) -> (u8, String, String) {
        capture(|o, e| super::run(&args(words), env, cwd, o, e))
    }

    #[test]
    fn filetype_reads_names_and_content() {
        let (root, env) = system("filetype");
        std::fs::write(root.join("a.pdf"), "x").unwrap();
        std::fs::write(root.join("noext"), b"\x89PNG\r\n\x1a\n").unwrap();
        assert_eq!(
            mime(&env, &root, &["query", "filetype", "a.pdf"]).1,
            "application/pdf\n"
        );
        assert_eq!(
            mime(&env, &root, &["query", "filetype", "noext"]).1,
            "image/png\n"
        );
        assert_eq!(
            mime(&env, &root, &["query", "filetype", "."]).1,
            "inode/directory\n"
        );
        assert_eq!(
            mime(&env, &root, &["query", "filetype", "gone"]).0,
            status::MISSING
        );
    }

    #[test]
    fn default_then_query_round_trips() {
        let (root, env) = system("default");
        install(&env, "viewer.desktop", "image/png;");
        install(&env, "other.desktop", "image/png;");
        let (code, _, err) = mime(
            &env,
            &root,
            &["default", "other.desktop", "image/png", "image/jpeg"],
        );
        assert_eq!(code, status::OK, "{err}");
        assert_eq!(
            mime(&env, &root, &["query", "default", "image/png"]).1,
            "other.desktop\n"
        );
        assert_eq!(
            mime(&env, &root, &["query", "default", "image/jpeg"]).1,
            "other.desktop\n"
        );
    }

    #[test]
    fn default_refuses_what_it_cannot_honour() {
        let (root, env) = system("refuse");
        install(&env, "v.desktop", "image/png;");
        assert_eq!(
            mime(&env, &root, &["default", "ghost.desktop", "image/png"]).0,
            status::MISSING
        );
        assert_eq!(
            mime(&env, &root, &["default", "v.desktop", "png"]).0,
            status::USAGE
        );
        assert_eq!(
            mime(&env, &root, &["default", "v.desktop"]).0,
            status::USAGE
        );
        assert!(!env.user_mimeapps().exists());
    }

    #[test]
    fn install_is_not_provided() {
        let (root, env) = system("install");
        assert_eq!(mime(&env, &root, &["install", "x.xml"]).0, status::NO_TOOL);
    }
}
