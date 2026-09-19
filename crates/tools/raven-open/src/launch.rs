//! Running the chosen application.
//!
//! The target is only ever an *argument*. Nothing here runs the file being
//! opened, and `Exec` never reaches a shell: [`raven_desktop::Entry::argv`]
//! splits it first and substitutes after, so no file name can inject a word.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::apps::App;

/// The terminal a `Terminal=true` application runs in. Raven's own, which
/// takes the program after `-e` like xterm.
const TERMINAL: &[&str] = &["raven-terminal", "-e"];

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
        let mut wrapped: Vec<String> = TERMINAL.iter().map(|s| (*s).to_owned()).collect();
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
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    Command::new(program)
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
    fn spawning_a_missing_program_is_an_error() {
        assert!(spawn(&["/nonexistent/raven-open-test".into()]).is_err());
        assert!(spawn(&[]).is_err());
    }
}
