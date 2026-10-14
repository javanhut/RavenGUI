//! Running the chosen application.
//!
//! The target is only ever an *argument*. Nothing here runs the file being
//! opened, and `Exec` never reaches a shell: [`raven_desktop::Entry::argv`]
//! splits it first and substitutes after, so no file name can inject a word.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::Env;
use crate::apps::App;

/// Raven's terminal: what a `Terminal=true` application runs in, and what
/// `raven-open --terminal` opens unless the desktop settings name another.
/// Every Raven program that wants a terminal gets it from here rather than
/// guessing at one.
pub const TERMINAL: &str = "raven-terminal";

/// The terminal chosen in the desktop settings -- `[general] terminal` in
/// `$XDG_CONFIG_HOME/raven/desktop.toml`, the same key Huginn's terminal
/// binding launches -- or [`TERMINAL`] when none is set.
pub fn chosen_terminal(env: &Env) -> String {
    let path = env.config_home.join("raven").join("desktop.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| general_terminal(&text))
        .unwrap_or_else(|| TERMINAL.to_owned())
}

/// `[general] terminal` from the text of `desktop.toml`, when it is set to
/// something. Read by hand because it is one string key, and a TOML parser
/// is not worth linking into every `xdg-open` call for it.
fn general_terminal(text: &str) -> Option<String> {
    let mut in_general = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_general = line == "[general]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !in_general || key.trim() != "terminal" {
            continue;
        }
        let value = value.trim();
        let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let rest = &value[1..];
        let value = rest[..rest.find(quote)?].trim();
        return (!value.is_empty()).then(|| value.to_owned());
    }
    None
}

/// The command that opens `terminal` with a shell in `dir`, and the
/// directory to start it in. Raven's terminal is told the folder; any other
/// terminal is started in it, which every terminal takes as where its shell
/// begins. `None` when `dir` is not UTF-8 and so cannot be passed on.
pub fn terminal_argv(terminal: &str, dir: &Path) -> Option<Vec<String>> {
    let is_raven = Path::new(terminal)
        .file_name()
        .is_some_and(|name| name == TERMINAL);
    let mut argv = vec![terminal.to_owned()];
    if is_raven {
        argv.push("--working-directory".to_owned());
        argv.push(dir.to_str()?.to_owned());
    }
    Some(argv)
}

/// The full argument vector that opens `target` with `app`, or `None` if the
/// entry's `Exec` has nothing runnable in it.
///
/// An `Exec` with no `%f %F %u %U` accepts no files, per the spec -- but
/// launching the application without the thing that was asked for opens
/// nothing, so the target is appended, as `xdg-open` does.
pub fn argv(app: &App, target: &str) -> Option<Vec<String>> {
    let targets = [target.to_owned()];
    let mut argv = app.entry.argv(&targets)?;
    let takes_target = app
        .entry
        .exec
        .split_whitespace()
        .any(|w| matches!(w, "%f" | "%F" | "%u" | "%U"));
    if !takes_target {
        argv.push(target.to_owned());
    }
    if app.entry.terminal {
        // xterm's convention: the program follows `-e`.
        let mut wrapped = vec![TERMINAL.to_owned(), "-e".to_owned()];
        wrapped.extend(argv);
        argv = wrapped;
    }
    Some(argv)
}

/// Start `argv` detached from the caller: no shared stdio, and its own
/// process group, so a Ctrl-C in the terminal that ran `raven-open` does not
/// reach the browser it started. Returns once the program has been
/// executed; it is not waited for.
pub fn spawn(argv: &[String]) -> std::io::Result<()> {
    spawn_from(argv, None)
}

/// [`spawn`], starting the program in `dir`.
pub fn spawn_in(argv: &[String], dir: &Path) -> std::io::Result<()> {
    spawn_from(argv, Some(dir))
}

fn spawn_from(argv: &[String], dir: Option<&Path>) -> std::io::Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    let mut command = Command::new(program);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        // Not waited for: this process exits next, and the child is
        // reparented rather than left a zombie.
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn app(body: &str) -> App {
        let text = format!("[Desktop Entry]\nType=Application\nName=n\n{body}");
        App {
            id: "n.desktop".into(),
            entry: raven_desktop::parse_handler(&text, Path::new("/n.desktop"), &[]).unwrap(),
            installed: (0, 0),
        }
    }

    #[test]
    fn the_target_fills_the_field_code() {
        assert_eq!(
            argv(&app("Exec=brave --new-window %U\n"), "https://x.test/a b").unwrap(),
            ["brave", "--new-window", "https://x.test/a b"]
        );
    }

    #[test]
    fn a_target_is_appended_when_exec_takes_none() {
        assert_eq!(
            argv(&app("Exec=viewer\n"), "/tmp/a.png").unwrap(),
            ["viewer", "/tmp/a.png"]
        );
    }

    #[test]
    fn a_hostile_file_name_stays_one_argument() {
        let name = "/tmp/x\"; rm -rf ~; \".txt";
        assert_eq!(argv(&app("Exec=ed %f\n"), name).unwrap(), ["ed", name]);
    }

    #[test]
    fn terminal_applications_run_inside_raven_terminal() {
        assert_eq!(
            argv(&app("Exec=nvim %F\nTerminal=true\n"), "/tmp/a.txt").unwrap(),
            ["raven-terminal", "-e", "nvim", "/tmp/a.txt"]
        );
    }

    #[test]
    fn ravens_terminal_is_told_the_folder() {
        let dir = Path::new("/home/me/My Files");
        assert_eq!(
            terminal_argv(TERMINAL, dir).unwrap(),
            ["raven-terminal", "--working-directory", "/home/me/My Files"]
        );
        assert_eq!(
            terminal_argv("/usr/local/bin/raven-terminal", dir).unwrap()[1],
            "--working-directory"
        );
        // Another terminal is started in the folder instead (spawn_in).
        assert_eq!(terminal_argv("kitty", dir).unwrap(), ["kitty"]);
    }

    #[test]
    fn the_desktop_settings_choose_the_terminal() {
        let text = "[appearance]\nterminal = \"no\"\n\n[general]\nlock = 1\nterminal = \"kitty\" # mine\n";
        assert_eq!(general_terminal(text).as_deref(), Some("kitty"));
        assert_eq!(general_terminal("[general]\nterminal = ''\n"), None);
        assert_eq!(general_terminal("[general]\nclock_24h = true\n"), None);
        assert_eq!(general_terminal("[other]\nterminal = \"x\"\n"), None);
    }

    #[test]
    fn spawning_a_missing_program_is_an_error() {
        assert!(spawn(&["/nonexistent/raven-open-test".into()]).is_err());
        assert!(spawn(&[]).is_err());
    }
}
