//! Open a file or URL with the application meant for it.
//!
//! Raven's answer to `xdg-open`, built into the system rather than borrowed
//! from a shell script that has to guess which desktop it is running on. It
//! reads the same files every other program does -- `.desktop` entries,
//! `mimeapps.list`, the shared-mime-info database -- so what opens a link
//! here is what GTK's "Open With" and the portal agree on, but it runs
//! nothing to find out: no `gio`, no `xdg-mime`, no helper of any kind.
//!
//! [`plan`] decides; the binary launches. Keeping them apart is what lets
//! Huginn's launcher or the file manager ask "what would open this?" without
//! opening it.

pub mod apps;
pub mod defaults;
pub mod env;
pub mod launch;
pub mod mime;
pub mod target;

use std::path::{Path, PathBuf};

pub use env::Env;
pub use target::{Target, TargetError};

/// Types that are never opened, whatever claims them. Handing an executable
/// to a handler is how clicking a download runs it; that is not "opening".
const REFUSED: &[&str] = &["application/x-executable", "application/x-sharedlib"];

/// What opening one thing will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The type the target was found to be: `x-scheme-handler/https`,
    /// `application/pdf`, `inode/directory`.
    pub mime: String,
    /// The desktop file ID of the application that will open it.
    pub app: String,
    /// The command that will be run.
    pub argv: Vec<String>,
    /// `(type, desktop file ID)` to record as the user's default once the
    /// launch has succeeded, when this was a choice nobody had made yet.
    pub remember: Option<(String, String)>,
}

/// Why nothing can be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The argument names nothing openable.
    Target(TargetError),
    /// No installed application opens this type.
    NoHandler(String),
    /// A program, which is run, not opened.
    Refused(PathBuf),
    /// A path that is not UTF-8, which no `.desktop` argument can carry.
    NotUtf8(PathBuf),
    /// The chosen application's `Exec` has nothing runnable in it.
    BadEntry(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Target(e) => e.fmt(f),
            Self::NoHandler(mime) => write!(f, "no application opens {mime}"),
            Self::Refused(path) => write!(
                f,
                "{}: is a program; run it instead of opening it",
                path.display()
            ),
            Self::NotUtf8(path) => write!(f, "{}: path is not valid UTF-8", path.display()),
            Self::BadEntry(id) => write!(f, "{id}: its Exec line has nothing to run"),
        }
    }
}

/// The types a web browser is the default for. `xdg-settings set
/// default-web-browser` sets all of them, so that a link, a saved page and a
/// page another program hands over all open in the same place.
pub const BROWSER_TYPES: &[&str] = &[
    "x-scheme-handler/http",
    "x-scheme-handler/https",
    "text/html",
    "application/xhtml+xml",
];

/// The desktop file ID of the application that opens `mime`: the same answer
/// [`plan`] would reach for a file of that type or a URL of that scheme.
pub fn default_for(mime: &str, env: &Env) -> Option<String> {
    let db = mime::MimeDb::load(&env.share_dirs());
    let lineage = db.lineage(mime);
    let lists = defaults::MimeApps::load(&env.mimeapps_lists());
    let apps = apps::Apps::scan(&env.app_dirs(), &env.desktops);
    defaults::choose(&lineage, &lists, &apps).map(|c| c.app.id.clone())
}

/// Whether `id` names an installed application that can be run -- the check
/// before anything is recorded as a default, so a typo cannot become one.
pub fn is_installed(id: &str, env: &Env) -> bool {
    apps::Apps::scan(&env.app_dirs(), &env.desktops)
        .get(id)
        .is_some()
}

/// Decide what opening `arg` will do, without doing it.
pub fn plan(arg: &str, cwd: &Path, env: &Env) -> Result<Plan, OpenError> {
    let target = target::classify(arg, cwd).map_err(OpenError::Target)?;

    let db = mime::MimeDb::load(&env.share_dirs());
    let (mime, lineage, argument) = match &target {
        Target::Url { url, scheme } => {
            let mime = format!("x-scheme-handler/{scheme}");
            (mime.clone(), vec![mime], url.clone())
        }
        Target::File(path) => {
            let mime = db.type_of(path);
            if REFUSED.contains(&mime.as_str()) {
                return Err(OpenError::Refused(path.clone()));
            }
            let argument = path
                .to_str()
                .ok_or_else(|| OpenError::NotUtf8(path.clone()))?
                .to_owned();
            (mime.clone(), db.lineage(&mime), argument)
        }
    };

    let lists = defaults::MimeApps::load(&env.mimeapps_lists());
    let apps = apps::Apps::scan(&env.app_dirs(), &env.desktops);
    let choice = defaults::choose(&lineage, &lists, &apps)
        .ok_or_else(|| OpenError::NoHandler(mime.clone()))?;

    let argv = launch::argv(choice.app, &argument)
        .ok_or_else(|| OpenError::BadEntry(choice.app.id.clone()))?;
    Ok(Plan {
        remember: choice
            .chosen()
            .then(|| (choice.mime.to_owned(), choice.app.id.clone())),
        mime,
        app: choice.app.id.clone(),
        argv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system(tag: &str) -> (PathBuf, Env) {
        let root =
            std::env::temp_dir().join(format!("raven-open-plan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let env = env::under(&root, &["Huginn", "Raven"]);
        std::fs::create_dir_all(env.app_dirs().last().unwrap()).unwrap();
        (root, env)
    }

    fn install(env: &Env, id: &str, body: &str) {
        let dir = env.app_dirs().last().unwrap().clone();
        std::fs::write(
            dir.join(id),
            format!("[Desktop Entry]\nType=Application\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn a_link_opens_in_the_only_browser_and_is_remembered() {
        let (_root, env) = system("link");
        install(
            &env,
            "brave-browser.desktop",
            "Name=Brave\nExec=brave %U\nMimeType=x-scheme-handler/https;\n",
        );
        let plan = plan("https://example.com", Path::new("/"), &env).unwrap();
        assert_eq!(plan.app, "brave-browser.desktop");
        assert_eq!(plan.argv, ["brave", "https://example.com"]);
        assert_eq!(
            plan.remember,
            Some((
                "x-scheme-handler/https".into(),
                "brave-browser.desktop".into()
            ))
        );
    }

    #[test]
    fn once_remembered_a_later_browser_does_not_take_over() {
        let (_root, env) = system("sticky");
        install(
            &env,
            "first.desktop",
            "Name=F\nExec=first %u\nMimeType=x-scheme-handler/https;\n",
        );
        let (mime, id) = plan("https://x.test", Path::new("/"), &env)
            .unwrap()
            .remember
            .unwrap();
        defaults::set_default(&env.user_mimeapps(), &mime, &id).unwrap();

        install(
            &env,
            "second.desktop",
            "Name=S\nExec=second %u\nMimeType=x-scheme-handler/https;\n",
        );
        let plan = plan("https://x.test", Path::new("/"), &env).unwrap();
        assert_eq!((plan.app.as_str(), plan.remember), ("first.desktop", None));
    }

    #[test]
    fn a_directory_opens_in_the_system_file_manager() {
        let (root, env) = system("dir");
        install(
            &env,
            "files.desktop",
            "Name=Files\nExec=files %U\nMimeType=inode/directory;\n",
        );
        std::fs::write(
            env.app_dirs().last().unwrap().join("mimeapps.list"),
            "[Default Applications]\ninode/directory=files.desktop\n",
        )
        .unwrap();
        let plan = plan(root.to_str().unwrap(), Path::new("/"), &env).unwrap();
        assert_eq!(
            (plan.mime.as_str(), plan.app.as_str()),
            (mime::DIRECTORY, "files.desktop")
        );
        assert_eq!(plan.remember, None);
    }

    #[test]
    fn a_program_is_refused_even_with_a_handler() {
        let (root, env) = system("elf");
        install(
            &env,
            "runner.desktop",
            "Name=R\nExec=runner %f\nMimeType=application/x-executable;\n",
        );
        let path = root.join("tool");
        std::fs::write(&path, b"\x7fELF\x02\x01\x01").unwrap();
        assert_eq!(
            plan(path.to_str().unwrap(), Path::new("/"), &env),
            Err(OpenError::Refused(path))
        );
    }

    #[test]
    fn nothing_installed_for_a_type_says_so() {
        let (_root, env) = system("none");
        assert_eq!(
            plan("magnet:?xt=urn:x", Path::new("/"), &env),
            Err(OpenError::NoHandler("x-scheme-handler/magnet".into()))
        );
    }
}
