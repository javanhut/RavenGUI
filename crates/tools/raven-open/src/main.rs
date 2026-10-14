//! raven-open: open a file or URL with its default application.
//!
//! ```sh
//! raven-open https://example.com     # the default browser
//! raven-open ~/Downloads/report.pdf  # the default PDF viewer
//! raven-open .                       # the file manager
//! raven-open --dry-run notes.md      # say what would run, run nothing
//! raven-open --terminal ~/src        # Raven's terminal, in that folder
//! ```
//!
//! One binary, installed under the names other programs call: `xdg-open`,
//! `xdg-settings` and `xdg-mime`. Which one it is being is decided by the name
//! it was run as, and each answers with the exit codes and output of the
//! xdg-utils tool it stands in for, so a browser asking "am I the default?"
//! reads the reply it expects.

mod xdg_mime;
mod xdg_settings;

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use raven_open::{Env, OpenError, TargetError};

/// xdg-utils' exit statuses, shared by every tool this binary stands in for.
mod status {
    pub(crate) const OK: u8 = 0;
    pub(crate) const USAGE: u8 = 1;
    pub(crate) const MISSING: u8 = 2;
    pub(crate) const NO_TOOL: u8 = 3;
    pub(crate) const FAILED: u8 = 4;
}

const OPEN_USAGE: &str = "\
usage: raven-open [--dry-run] <file | URL>
       raven-open [--dry-run] --terminal [folder]

Opens a file, folder or URL with the application chosen for it.

  -t, --terminal  open Raven's terminal in the folder (default: here)
  -n, --dry-run   print what would be run, and run nothing
  -h, --help      this text
      --version   the version

Exit status: 0 opened, 1 usage, 2 no such file, 3 nothing opens it,
4 the application could not be started.";

fn main() -> ExitCode {
    let mut args = std::env::args();
    let name = args
        .next()
        .as_deref()
        .and_then(|a| Path::new(a).file_name()?.to_str().map(str::to_owned))
        .unwrap_or_else(|| "raven-open".to_owned());
    let args: Vec<String> = args.collect();

    let env = Env::from_process();
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr().lock());

    let code = match name.as_str() {
        "raven-open" | "xdg-open" => open(&name, &args, &env, &cwd, &mut out, &mut err),
        "xdg-settings" => xdg_settings::run(&args, &env, &mut out, &mut err),
        "xdg-mime" => xdg_mime::run(&args, &env, &cwd, &mut out, &mut err),
        other => {
            let _ = writeln!(err, "{other}: not provided on Raven");
            status::NO_TOOL
        }
    };
    ExitCode::from(code)
}

/// `--version`, the same shape for every name.
fn version(name: &str, out: &mut dyn Write) -> u8 {
    let _ = writeln!(out, "{name} (raven-open) {}", env!("CARGO_PKG_VERSION"));
    status::OK
}

fn open(
    name: &str,
    args: &[String],
    env: &Env,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let mut dry_run = false;
    let mut terminal = false;
    let mut targets = Vec::new();
    let mut options_done = false;
    for arg in args {
        match arg.as_str() {
            _ if options_done => targets.push(arg),
            "--" => options_done = true,
            "-h" | "--help" | "--manual" => {
                let _ = writeln!(out, "{OPEN_USAGE}");
                return status::OK;
            }
            "--version" => return version(name, out),
            "-n" | "--dry-run" => dry_run = true,
            "-t" | "--terminal" => terminal = true,
            flag if flag.starts_with('-') && flag.len() > 1 => {
                let _ = writeln!(err, "{name}: unknown option {flag}\n\n{OPEN_USAGE}");
                return status::USAGE;
            }
            _ => targets.push(arg),
        }
    }
    if terminal {
        return open_terminal(name, &targets, env, cwd, dry_run, out, err);
    }
    // One thing at a time, as xdg-open takes: several would each need a
    // choice, and a partial failure has no exit status that means anything.
    let [target] = targets.as_slice() else {
        let _ = writeln!(err, "{OPEN_USAGE}");
        return status::USAGE;
    };

    let plan = match raven_open::plan(target, cwd, env) {
        Ok(plan) => plan,
        Err(e) => {
            let _ = writeln!(err, "{name}: {e}");
            return match e {
                OpenError::Target(TargetError::Missing(_) | TargetError::Empty) => status::MISSING,
                OpenError::Target(TargetError::RemoteFile(_)) | OpenError::NoHandler(_) => {
                    status::NO_TOOL
                }
                OpenError::Refused(_) | OpenError::NotUtf8(_) | OpenError::BadEntry(_) => {
                    status::FAILED
                }
            };
        }
    };

    if dry_run {
        let _ = writeln!(out, "type:  {}", plan.mime);
        let _ = writeln!(out, "app:   {}", plan.app);
        let _ = writeln!(out, "run:   {}", plan.argv.join(" "));
        if let Some((mime, id)) = &plan.remember {
            let _ = writeln!(out, "would remember {id} as the default for {mime}");
        }
        return status::OK;
    }

    if let Err(e) = raven_open::launch::spawn(&plan.argv) {
        let _ = writeln!(err, "{name}: could not start {}: {e}", plan.argv[0]);
        return status::FAILED;
    }
    // Only a choice that actually launched is kept: remembering an
    // application that failed to start would make the failure permanent.
    if let Some((mime, id)) = &plan.remember
        && let Err(e) = raven_open::defaults::set_default(&env.user_mimeapps(), mime, id)
    {
        let _ = writeln!(
            err,
            "{name}: opened, but could not remember {id} for {mime}: {e}"
        );
    }
    status::OK
}

/// `--terminal [folder]`: the desktop's terminal with a shell in `folder`,
/// or in the current directory when none is given. A file stands for the
/// folder it is in, so "a terminal here" works on whatever was selected.
fn open_terminal(
    name: &str,
    targets: &[&String],
    env: &Env,
    cwd: &Path,
    dry_run: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let dir = match targets {
        [] => cwd.to_path_buf(),
        [one] => cwd.join(one),
        _ => {
            let _ = writeln!(err, "{OPEN_USAGE}");
            return status::USAGE;
        }
    };
    let dir = match std::fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => dir,
        Ok(_) => match dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => dir,
        },
        Err(e) => {
            let _ = writeln!(err, "{name}: {}: {e}", dir.display());
            return status::MISSING;
        }
    };
    let terminal = raven_open::launch::chosen_terminal(env);
    let Some(argv) = raven_open::launch::terminal_argv(&terminal, &dir) else {
        let _ = writeln!(err, "{name}: {}: path is not valid UTF-8", dir.display());
        return status::FAILED;
    };
    if dry_run {
        let _ = writeln!(out, "run:   {}", argv.join(" "));
        let _ = writeln!(out, "in:    {}", dir.display());
        return status::OK;
    }
    if let Err(e) = raven_open::launch::spawn_in(&argv, &dir) {
        let _ = writeln!(err, "{name}: could not start {}: {e}", argv[0]);
        return status::FAILED;
    }
    status::OK
}

/// A system for the command tests: an [`Env`] under a fresh directory, and a
/// way to install applications into it.
#[cfg(test)]
mod testing {
    use std::path::PathBuf;

    use raven_open::Env;

    pub(crate) fn system(tag: &str) -> (PathBuf, Env) {
        let root =
            std::env::temp_dir().join(format!("raven-open-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let env = Env {
            config_home: root.join("home/.config"),
            config_dirs: vec![root.join("etc/xdg")],
            data_home: root.join("home/.local/share"),
            data_dirs: vec![root.join("usr/share")],
            desktops: vec!["Raven".into()],
        };
        std::fs::create_dir_all(root.join("usr/share/applications")).unwrap();
        (root, env)
    }

    pub(crate) fn install(env: &Env, id: &str, mimes: &str) {
        let path = env.data_dirs[0].join("applications").join(id);
        std::fs::write(
            path,
            format!(
                "[Desktop Entry]\nType=Application\nName={id}\nExec=run %u\nMimeType={mimes}\n"
            ),
        )
        .unwrap();
    }

    /// Run a command, returning `(status, stdout, stderr)`.
    pub(crate) fn capture(
        f: impl FnOnce(&mut dyn std::io::Write, &mut dyn std::io::Write) -> u8,
    ) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = f(&mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    pub(crate) fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn open_dry_run_reports_the_plan_and_changes_nothing() {
        let (root, env) = system("open");
        install(&env, "b.desktop", "x-scheme-handler/https;");
        let (code, out, _) = capture(|o, e| {
            open(
                "xdg-open",
                &args(&["-n", "https://x.test"]),
                &env,
                &root,
                o,
                e,
            )
        });
        assert_eq!(code, status::OK);
        assert!(out.contains("app:   b.desktop"), "{out}");
        assert!(!env.user_mimeapps().exists());
    }

    #[test]
    fn open_exit_codes_match_xdg_open() {
        let (root, env) = system("codes");
        let code = |a: &[&str]| capture(|o, e| open("xdg-open", &args(a), &env, &root, o, e)).0;
        assert_eq!(code(&[]), status::USAGE);
        assert_eq!(code(&["a", "b"]), status::USAGE);
        assert_eq!(code(&["--bogus", "x"]), status::USAGE);
        assert_eq!(code(&["missing.txt"]), status::MISSING);
        assert_eq!(code(&["gopher://x"]), status::NO_TOOL);
    }

    #[test]
    fn terminal_opens_in_the_named_folder_or_the_one_holding_a_file() {
        let (root, env) = system("terminal");
        let dir = root.join("My Project");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.md"), "x").unwrap();
        let run = |a: &[&str]| capture(|o, e| open("raven-open", &args(a), &env, &root, o, e));

        let (code, out, _) = run(&["-n", "--terminal", "My Project"]);
        assert_eq!(code, status::OK);
        let expected = format!("run:   raven-terminal --working-directory {}", dir.display());
        assert_eq!(out.lines().next(), Some(expected.as_str()));

        let (_, out, _) = run(&["-n", "-t", "My Project/notes.md"]);
        assert_eq!(out.lines().next(), Some(expected.as_str()));

        let (_, out, _) = run(&["-n", "-t"]);
        assert!(out.contains(&root.display().to_string()), "{out}");

        assert_eq!(run(&["-n", "-t", "nowhere"]).0, status::MISSING);
        assert_eq!(run(&["-t", "a", "b"]).0, status::USAGE);

        // The terminal chosen in the desktop settings is started in the folder.
        std::fs::create_dir_all(env.config_home.join("raven")).unwrap();
        std::fs::write(
            env.config_home.join("raven/desktop.toml"),
            "[general]\nterminal = \"kitty\"\n",
        )
        .unwrap();
        let (_, out, _) = run(&["-n", "-t", "My Project"]);
        assert_eq!(out, format!("run:   kitty\nin:    {}\n", dir.display()));
    }
}
