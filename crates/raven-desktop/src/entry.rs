//! Desktop entries: what is installed, and what to run.
//!
//! Implements the parts of the XDG Desktop Entry specification a launcher
//! actually needs. Hand-rolled rather than taken from a crate for one reason:
//! [`Entry::argv`] turns a file on disk into an argument vector, and every
//! `.desktop` file under `$XDG_DATA_HOME` is user-writable. That makes this the
//! security boundary of the whole launcher, and it is worth owning outright and
//! testing exhaustively rather than delegating to a dependency.
//!
//! The rule that follows from it: **`Exec` never reaches a shell.** It is
//! parsed to an argv here and handed to `Command` with an explicit program and
//! explicit arguments. There is no interpolation step a crafted entry could
//! escape from, because there is no shell to escape into.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One installed application, as a launcher cares about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `Name`. What the launcher shows.
    pub name: String,
    /// `Comment`, if any. A second line, or a tooltip.
    pub comment: Option<String>,
    /// `GenericName`: what the application *is*, as opposed to what it is
    /// called. "Web Browser" for Firefox. Searched alongside the name, because
    /// someone who does not remember the name still knows what they want.
    pub generic_name: Option<String>,
    /// `Icon`. A theme name like `raven-terminal`, or an absolute path.
    /// Resolved to a file by [`crate::icon`], not here.
    pub icon: Option<String>,
    /// `Exec`, still holding its field codes. Use [`Entry::argv`] to run it.
    pub exec: String,
    /// `Categories`, split on `;`. Used to group the launcher's list.
    pub categories: Vec<String>,
    /// `Keywords`, split on `;`. Searched alongside the name.
    pub keywords: Vec<String>,
    /// `Terminal=true`: must be run inside a terminal emulator.
    pub terminal: bool,
    /// `StartupWMClass`, which is how a dock matches a window back to the
    /// entry that launched it when the `app_id` does not match the file name.
    pub startup_wm_class: Option<String>,
    /// Where this came from. Kept for cache invalidation and for diagnostics.
    pub path: PathBuf,
}

/// Why an entry was skipped rather than listed.
///
/// Not an error type: every one of these is a normal thing to find in a
/// well-formed desktop directory. They are distinguished so a launcher can log
/// *why* an app it expected is missing, which is otherwise very hard to debug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// Not `Type=Application` — a Link or Directory entry.
    NotAnApplication,
    /// `NoDisplay=true`: installed, but deliberately not for menus.
    NoDisplay,
    /// `Hidden=true`, which the spec defines as "deleted by the user".
    Hidden,
    /// `OnlyShowIn`/`NotShowIn` excluded this desktop.
    WrongDesktop,
    /// `TryExec` names a binary that is not on `PATH`.
    TryExecMissing,
    /// No `Name`, or no `Exec` — nothing to show or nothing to run.
    Incomplete,
}

/// Parse one desktop file, or say why it should not be listed.
///
/// `current_desktop` is `$XDG_CURRENT_DESKTOP` split on `:`; pass an empty
/// slice when it is unset. Note that an unset value means `OnlyShowIn` entries
/// are excluded and `NotShowIn` entries are included — the session really
/// should set it, or entries gated to `OnlyShowIn=Raven` will vanish.
pub fn parse(text: &str, path: &Path, current_desktop: &[String]) -> Result<Entry, Skipped> {
    let fields = desktop_entry_group(text);

    if fields.get("Type").map(String::as_str) != Some("Application") {
        return Err(Skipped::NotAnApplication);
    }
    if is_true(fields.get("Hidden")) {
        return Err(Skipped::Hidden);
    }
    if is_true(fields.get("NoDisplay")) {
        return Err(Skipped::NoDisplay);
    }
    if !shows_in(&fields, current_desktop) {
        return Err(Skipped::WrongDesktop);
    }
    if let Some(try_exec) = fields.get("TryExec")
        && !is_executable(try_exec)
    {
        return Err(Skipped::TryExecMissing);
    }

    let (Some(name), Some(exec)) = (fields.get("Name"), fields.get("Exec")) else {
        return Err(Skipped::Incomplete);
    };
    if name.is_empty() || exec.is_empty() {
        return Err(Skipped::Incomplete);
    }

    Ok(Entry {
        name: name.clone(),
        comment: fields.get("Comment").cloned(),
        generic_name: fields.get("GenericName").cloned(),
        icon: fields.get("Icon").cloned(),
        exec: exec.clone(),
        categories: semicolon_list(fields.get("Categories")),
        keywords: semicolon_list(fields.get("Keywords")),
        terminal: is_true(fields.get("Terminal")),
        startup_wm_class: fields.get("StartupWMClass").cloned(),
        path: path.to_owned(),
    })
}

impl Entry {
    /// The argument vector to spawn, with field codes resolved.
    ///
    /// Returns `None` when `Exec` contains nothing runnable — an entry that is
    /// only field codes, or that is malformed enough to leave no program name.
    ///
    /// Field codes are handled per the spec: `%f`/`%u` take one item, `%F`/`%U`
    /// expand to all of them, and `%d %D %n %N %v %m` are deprecated and
    /// dropped. `%i` expands to *two* arguments (`--icon`, the icon name) or to
    /// none — it is the one code whose arity is not one, which is exactly the
    /// case naive implementations get wrong. `%%` is a literal percent.
    ///
    /// Codes are substituted **after** the argv has been split, so a filename
    /// containing a space stays one argument and a filename containing a quote
    /// or a semicolon can never introduce another. That ordering is the whole
    /// safety property; reversing it is the classic launcher injection bug.
    pub fn argv(&self, targets: &[String]) -> Option<Vec<String>> {
        let words = split_exec(&self.exec)?;
        let mut argv = Vec::with_capacity(words.len());

        for word in words {
            match field_code(&word) {
                Some('f' | 'u') => argv.extend(targets.first().cloned()),
                Some('F' | 'U') => argv.extend_from_slice(targets),
                // Two arguments, or none. Never one.
                Some('i') => {
                    if let Some(icon) = &self.icon {
                        argv.push("--icon".to_owned());
                        argv.push(icon.clone());
                    }
                }
                Some('c') => argv.push(self.name.clone()),
                Some('k') => argv.push(self.path.to_string_lossy().into_owned()),
                // Deprecated codes expand to nothing at all.
                Some('d' | 'D' | 'n' | 'N' | 'v' | 'm') => {}
                // Not a standalone code: either a plain word, or one with a
                // literal %% in it that unescaping has already dealt with.
                _ => argv.push(unescape_percent(&word)),
            }
        }

        // A program name is the minimum. An Exec of only field codes with
        // nothing to substitute leaves an empty argv, which would otherwise
        // reach Command::new("") and fail with a confusing ENOENT.
        (!argv.is_empty()).then_some(argv)
    }
}

/// The `[Desktop Entry]` group's key/value pairs.
///
/// Only that group: a desktop file may carry any number of
/// `[Desktop Action Foo]` groups afterwards, and reading their keys as if they
/// were the entry's own would let an action's `Exec` masquerade as the app's.
fn desktop_entry_group(text: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let mut inside = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = group == "Desktop Entry";
            continue;
        }
        if !inside {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // Localized keys look like `Name[de]`. The launcher shows the C locale
        // for now; taking `Name[de]` as `Name` would pick whichever
        // translation happened to be last in the file.
        if key.contains('[') {
            continue;
        }
        fields
            .entry(key.to_owned())
            .or_insert_with(|| value.trim().to_owned());
    }
    fields
}

/// The spec's booleans are exactly `true` and `false`, lowercase.
fn is_true(value: Option<&String>) -> bool {
    value.map(String::as_str) == Some("true")
}

/// `a;b;c;` — the trailing separator is part of the format, not a stray.
fn semicolon_list(value: Option<&String>) -> Vec<String> {
    value
        .map(|v| {
            v.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `OnlyShowIn`/`NotShowIn` admit this desktop.
fn shows_in(fields: &HashMap<String, String>, current: &[String]) -> bool {
    let matches = |key: &str| {
        semicolon_list(fields.get(key))
            .iter()
            .any(|listed| current.iter().any(|c| c == listed))
    };
    if fields.contains_key("OnlyShowIn") {
        return matches("OnlyShowIn");
    }
    if fields.contains_key("NotShowIn") {
        return !matches("NotShowIn");
    }
    true
}

/// Whether `name` is runnable: an executable path, or a name on `PATH`.
fn is_executable(name: &str) -> bool {
    let executable = |p: &Path| p.is_file();
    if name.contains('/') {
        return executable(Path::new(name));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| executable(&dir.join(name)))
}

/// Split `Exec` into words, honouring the spec's quoting.
///
/// Double quotes group, and inside them a backslash escapes `"`, `` ` ``, `$`
/// and `\` itself. Returns `None` for an unterminated quote — a malformed
/// value is refused rather than guessed at, because guessing here means
/// guessing at what to execute.
fn split_exec(exec: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut has_word = false;
    let mut quoted = false;
    let mut chars = exec.chars();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                // A quote starts a word even when what it encloses is empty,
                // so `foo "" bar` really does pass an empty argument.
                has_word = true;
            }
            '\\' if quoted => match chars.next() {
                Some(escaped @ ('"' | '`' | '$' | '\\')) => word.push(escaped),
                Some(other) => {
                    word.push('\\');
                    word.push(other);
                }
                None => return None,
            },
            c if c.is_whitespace() && !quoted => {
                if has_word {
                    words.push(std::mem::take(&mut word));
                    has_word = false;
                }
            }
            c => {
                word.push(c);
                has_word = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if has_word {
        words.push(word);
    }
    (!words.is_empty()).then_some(words)
}

/// The field code, if `word` is exactly one and nothing else.
///
/// A code only counts standing alone. `%f` is a code; `foo%f` is a filename
/// that happens to contain a percent, and substituting inside it would let an
/// entry build an argument out of a caller-supplied path.
fn field_code(word: &str) -> Option<char> {
    let mut chars = word.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some('%'), Some(code), None) if code != '%' => Some(code),
        _ => None,
    }
}

/// `%%` means a literal `%`.
fn unescape_percent(word: &str) -> String {
    word.replace("%%", "%")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(exec: &str) -> Entry {
        Entry {
            name: "Raven Terminal".to_owned(),
            comment: None,
            generic_name: None,
            icon: Some("raven-terminal".to_owned()),
            exec: exec.to_owned(),
            categories: Vec::new(),
            keywords: Vec::new(),
            terminal: false,
            startup_wm_class: None,
            path: PathBuf::from("/usr/share/applications/raven-terminal.desktop"),
        }
    }

    const REAL: &str = "\
[Desktop Entry]
Version=1.0
Name=Raven Terminal
Comment=GPU-accelerated terminal emulator
Exec=/usr/local/bin/raven-terminal-launcher
Icon=raven-terminal
Terminal=false
Type=Application
Categories=System;TerminalEmulator;Utility;
Keywords=terminal;console;shell;command;prompt;
StartupNotify=true
StartupWMClass=raven-terminal
";

    #[test]
    fn the_real_raven_terminal_entry_parses() {
        // The exact file installed at /usr/share/applications on this machine.
        let e = parse(REAL, Path::new("/x.desktop"), &[]).expect("parses");
        assert_eq!(e.name, "Raven Terminal");
        assert_eq!(e.icon.as_deref(), Some("raven-terminal"));
        assert_eq!(e.startup_wm_class.as_deref(), Some("raven-terminal"));
        assert_eq!(e.categories, ["System", "TerminalEmulator", "Utility"]);
        assert_eq!(e.keywords.len(), 5);
        assert!(!e.terminal);
        assert_eq!(
            e.argv(&[]).expect("runnable"),
            ["/usr/local/bin/raven-terminal-launcher"]
        );
    }

    #[test]
    fn entries_not_meant_for_a_menu_are_skipped_with_a_reason() {
        let cases = [
            ("Type=Link\nName=n\nExec=e\n", Skipped::NotAnApplication),
            ("Type=Application\nNoDisplay=true\nName=n\nExec=e\n", Skipped::NoDisplay),
            ("Type=Application\nHidden=true\nName=n\nExec=e\n", Skipped::Hidden),
            ("Type=Application\nName=n\n", Skipped::Incomplete),
            ("Type=Application\nExec=e\n", Skipped::Incomplete),
        ];
        for (body, expected) in cases {
            let text = format!("[Desktop Entry]\n{body}");
            assert_eq!(
                parse(&text, Path::new("/x.desktop"), &[]),
                Err(expected),
                "wrong verdict for {body:?}"
            );
        }
    }

    #[test]
    fn only_show_in_respects_the_current_desktop() {
        let text = "[Desktop Entry]\nType=Application\nName=n\nExec=e\nOnlyShowIn=Raven;GNOME;\n";
        let raven = ["Raven".to_owned()];
        assert!(parse(text, Path::new("/x.desktop"), &raven).is_ok());
        assert_eq!(
            parse(text, Path::new("/x.desktop"), &["KDE".to_owned()]),
            Err(Skipped::WrongDesktop)
        );
        // Unset XDG_CURRENT_DESKTOP excludes OnlyShowIn entries. This is the
        // spec's behaviour and the reason the session must set it.
        assert_eq!(
            parse(text, Path::new("/x.desktop"), &[]),
            Err(Skipped::WrongDesktop)
        );
    }

    #[test]
    fn not_show_in_is_the_other_way_round() {
        let text = "[Desktop Entry]\nType=Application\nName=n\nExec=e\nNotShowIn=KDE;\n";
        assert!(parse(text, Path::new("/x.desktop"), &["Raven".to_owned()]).is_ok());
        assert_eq!(
            parse(text, Path::new("/x.desktop"), &["KDE".to_owned()]),
            Err(Skipped::WrongDesktop)
        );
    }

    #[test]
    fn only_the_desktop_entry_group_is_read() {
        // An action's Exec must never be mistaken for the application's: it
        // would silently change what the launcher runs.
        let text = "\
[Desktop Entry]
Type=Application
Name=Real
Exec=/bin/real

[Desktop Action New]
Name=Impostor
Exec=/bin/impostor
";
        let e = parse(text, Path::new("/x.desktop"), &[]).expect("parses");
        assert_eq!(e.name, "Real");
        assert_eq!(e.argv(&[]).expect("runnable"), ["/bin/real"]);
    }

    #[test]
    fn a_localized_key_does_not_overwrite_the_plain_one() {
        let text = "[Desktop Entry]\nType=Application\nName=Terminal\nName[de]=Konsole\nExec=e\n";
        assert_eq!(parse(text, Path::new("/x.desktop"), &[]).unwrap().name, "Terminal");
    }

    #[test]
    fn field_codes_taking_one_item_take_exactly_one() {
        let targets = ["/a.txt".to_owned(), "/b.txt".to_owned()];
        assert_eq!(entry("app %f").argv(&targets).unwrap(), ["app", "/a.txt"]);
        assert_eq!(entry("app %u").argv(&targets).unwrap(), ["app", "/a.txt"]);
    }

    #[test]
    fn field_codes_taking_all_items_expand_in_place() {
        let targets = ["/a.txt".to_owned(), "/b.txt".to_owned()];
        assert_eq!(entry("app %F").argv(&targets).unwrap(), ["app", "/a.txt", "/b.txt"]);
        // And expand to nothing at all when there is nothing to open, rather
        // than to an empty string argument the program would try to open.
        assert_eq!(entry("app %F").argv(&[]).unwrap(), ["app"]);
        assert_eq!(entry("app %f").argv(&[]).unwrap(), ["app"]);
    }

    #[test]
    fn percent_i_expands_to_two_arguments_or_none() {
        // The one code whose arity is not one. Treating it as a single
        // argument is the classic bug: it yields `--icon` with no value, or a
        // bare icon name the program reads as a filename.
        assert_eq!(
            entry("app %i").argv(&[]).unwrap(),
            ["app", "--icon", "raven-terminal"]
        );
        let mut no_icon = entry("app %i");
        no_icon.icon = None;
        assert_eq!(no_icon.argv(&[]).unwrap(), ["app"]);
    }

    #[test]
    fn deprecated_codes_vanish_rather_than_becoming_arguments() {
        for code in ["%d", "%D", "%n", "%N", "%v", "%m"] {
            let e = entry(&format!("app {code} tail"));
            assert_eq!(e.argv(&[]).unwrap(), ["app", "tail"], "{code} survived");
        }
    }

    #[test]
    fn a_field_code_only_counts_when_it_stands_alone() {
        // `foo%f` is a filename with a percent in it. Substituting inside it
        // would let an entry assemble an argument out of a caller path.
        let targets = ["/evil".to_owned()];
        assert_eq!(entry("app foo%f").argv(&targets).unwrap(), ["app", "foo%f"]);
    }

    #[test]
    fn double_percent_is_a_literal_percent() {
        assert_eq!(entry("app 100%%").argv(&[]).unwrap(), ["app", "100%"]);
    }

    #[test]
    fn quoted_paths_with_spaces_stay_one_argument() {
        // The case a popular crate gets wrong: this must not become three.
        let e = entry(r#""/opt/My App/bin/run" --flag"#);
        assert_eq!(e.argv(&[]).unwrap(), ["/opt/My App/bin/run", "--flag"]);
    }

    #[test]
    fn a_quoted_command_line_is_not_split_inside_its_quotes() {
        let e = entry(r#"sh -c "foo bar""#);
        assert_eq!(e.argv(&[]).unwrap(), ["sh", "-c", "foo bar"]);
    }

    #[test]
    fn backslash_escapes_apply_only_inside_quotes() {
        let e = entry(r#"app "a\"b""#);
        assert_eq!(e.argv(&[]).unwrap(), ["app", r#"a"b"#]);
    }

    #[test]
    fn an_unterminated_quote_is_refused_rather_than_guessed_at() {
        // Guessing here means guessing at what to execute.
        assert_eq!(entry(r#"app "unterminated"#).argv(&[]), None);
    }

    #[test]
    fn a_target_can_never_introduce_another_argument() {
        // The property that makes this safe: codes are substituted AFTER the
        // split, so no content of a target is ever re-parsed as syntax.
        let nasty = [r#"; rm -rf / "$(whoami)" 'x' \ %F"#.to_owned()];
        let argv = entry("app %f").argv(&nasty).unwrap();
        assert_eq!(argv.len(), 2, "target became more than one argument: {argv:?}");
        assert_eq!(argv[1], nasty[0], "target was altered on the way through");
    }

    #[test]
    fn an_exec_with_nothing_runnable_left_is_none() {
        // Would otherwise reach Command::new("") and fail confusingly.
        assert_eq!(entry("%F").argv(&[]), None);
        assert_eq!(entry("   ").argv(&[]), None);
    }

    #[test]
    fn try_exec_hides_an_app_whose_binary_is_gone() {
        let missing = "[Desktop Entry]\nType=Application\nName=n\nExec=e\nTryExec=/nonexistent/xyzzy\n";
        assert_eq!(
            parse(missing, Path::new("/x.desktop"), &[]),
            Err(Skipped::TryExecMissing)
        );
        let present = "[Desktop Entry]\nType=Application\nName=n\nExec=e\nTryExec=/bin/sh\n";
        assert!(parse(present, Path::new("/x.desktop"), &[]).is_ok());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let text = "# a comment\n\n[Desktop Entry]\n# another\nType=Application\nName=n\nExec=e\n";
        assert!(parse(text, Path::new("/x.desktop"), &[]).is_ok());
    }
}
