//! `xdg-settings`: which browser is the default, asked and set.
//!
//! Chromium-family browsers run `xdg-settings check default-web-browser
//! <their .desktop>` to decide whether to offer "Make default", and `set` when
//! it is clicked. Only the two properties anything asks about are provided;
//! the rest of xdg-settings (screensavers, …) belongs to desktops Raven is not.
//!
//! ```sh
//! xdg-settings get default-web-browser
//! xdg-settings check default-web-browser brave-browser.desktop   # yes / no
//! xdg-settings set default-web-browser brave-browser.desktop
//! xdg-settings get default-url-scheme-handler mailto
//! xdg-settings set default-url-scheme-handler mailto thunderbird.desktop
//! ```

use std::io::Write;

use raven_open::{BROWSER_TYPES, Env};

use crate::status;

const USAGE: &str = "\
usage: xdg-settings get default-web-browser
       xdg-settings check default-web-browser <app.desktop>
       xdg-settings set default-web-browser <app.desktop>
       xdg-settings get default-url-scheme-handler <scheme>
       xdg-settings check default-url-scheme-handler <scheme> <app.desktop>
       xdg-settings set default-url-scheme-handler <scheme> <app.desktop>
       xdg-settings --list";

const LIST: &str = "\
Known properties:
  default-url-scheme-handler    Default handler for URL scheme
  default-web-browser           Default web browser";

pub(crate) fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let usage = |err: &mut dyn Write| {
        let _ = writeln!(err, "{USAGE}");
        status::USAGE
    };

    let (verb, property, rest) = match args.as_slice() {
        ["--help" | "-h" | "--manual", ..] => {
            let _ = writeln!(out, "{USAGE}");
            return status::OK;
        }
        ["--version", ..] => return crate::version("xdg-settings", out),
        ["--list", ..] => {
            let _ = writeln!(out, "{LIST}");
            return status::OK;
        }
        [verb @ ("get" | "check" | "set"), property, rest @ ..] => (*verb, *property, rest),
        _ => return usage(err),
    };

    // Which types the property covers, and the value argument (if the verb
    // takes one).
    let (types, value): (Vec<String>, Option<&str>) = match (property, rest) {
        ("default-web-browser", []) if verb == "get" => (browser_types(), None),
        ("default-web-browser", [id]) if verb != "get" => (browser_types(), Some(*id)),
        ("default-url-scheme-handler", [scheme]) if verb == "get" => (scheme_type(scheme), None),
        ("default-url-scheme-handler", [scheme, id]) if verb != "get" => {
            (scheme_type(scheme), Some(*id))
        }
        ("default-web-browser" | "default-url-scheme-handler", _) => return usage(err),
        (other, _) => {
            let _ = writeln!(err, "xdg-settings: unknown property {other}");
            return status::USAGE;
        }
    };

    match (verb, value) {
        ("get", None) => {
            // What opens the first type is the answer; for the browser, what
            // opens http links. Nothing chosen and nothing installed prints
            // nothing, as xdg-settings does.
            if let Some(id) = raven_open::default_for(&types[0], env) {
                let _ = writeln!(out, "{id}");
            }
            status::OK
        }
        ("check", Some(id)) => {
            // "yes" only if links of every scheme it covers go to this
            // application: a browser that owns https but not http is not the
            // default. Only schemes are compared, as xdg-settings does -- a
            // browser whose entry forgot `application/xhtml+xml` would
            // otherwise be told "no" forever and ask to be made default on
            // every start.
            let all = types
                .iter()
                .filter(|t| t.starts_with("x-scheme-handler/"))
                .all(|t| raven_open::default_for(t, env).as_deref() == Some(id));
            let _ = writeln!(out, "{}", if all { "yes" } else { "no" });
            status::OK
        }
        ("set", Some(id)) => set(&types, id, env, err),
        _ => usage(err),
    }
}

fn browser_types() -> Vec<String> {
    BROWSER_TYPES.iter().map(|t| (*t).to_owned()).collect()
}

fn scheme_type(scheme: &str) -> Vec<String> {
    vec![format!("x-scheme-handler/{}", scheme.to_ascii_lowercase())]
}

fn set(types: &[String], id: &str, env: &Env, err: &mut dyn Write) -> u8 {
    if !id.ends_with(".desktop") {
        let _ = writeln!(
            err,
            "xdg-settings: {id}: not a desktop file ID (expected name.desktop)"
        );
        return status::USAGE;
    }
    if !raven_open::is_installed(id, env) {
        let _ = writeln!(err, "xdg-settings: {id}: no such application is installed");
        return status::MISSING;
    }
    for mime in types {
        if let Err(e) = raven_open::defaults::set_default(&env.user_mimeapps(), mime, id) {
            let _ = writeln!(err, "xdg-settings: could not set {id} for {mime}: {e}");
            return status::FAILED;
        }
    }
    status::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    const WEB: &str = "x-scheme-handler/http;x-scheme-handler/https;text/html;";

    fn settings(env: &Env, words: &[&str]) -> (u8, String, String) {
        capture(|o, e| super::run(&args(words), env, o, e))
    }

    #[test]
    fn the_only_browser_is_the_default_without_anyone_setting_it() {
        let (_root, env) = system("only");
        install(&env, "brave-browser.desktop", WEB);
        assert_eq!(
            settings(&env, &["get", "default-web-browser"]),
            (status::OK, "brave-browser.desktop\n".into(), String::new())
        );
        // So the browser does not nag to be made default when it already is.
        assert_eq!(
            settings(
                &env,
                &["check", "default-web-browser", "brave-browser.desktop"]
            )
            .1,
            "yes\n"
        );
    }

    #[test]
    fn set_moves_every_browser_type_and_check_agrees() {
        let (_root, env) = system("set");
        install(&env, "first.desktop", WEB);
        install(&env, "second.desktop", WEB);
        assert_eq!(
            settings(&env, &["check", "default-web-browser", "second.desktop"]).1,
            "no\n"
        );

        assert_eq!(
            settings(&env, &["set", "default-web-browser", "second.desktop"]).0,
            status::OK
        );
        assert_eq!(
            settings(&env, &["check", "default-web-browser", "second.desktop"]).1,
            "yes\n"
        );
        assert_eq!(
            settings(&env, &["check", "default-web-browser", "first.desktop"]).1,
            "no\n"
        );
        for mime in BROWSER_TYPES {
            assert_eq!(
                raven_open::default_for(mime, &env).as_deref(),
                Some("second.desktop"),
                "{mime}"
            );
        }
    }

    #[test]
    fn a_browser_missing_a_page_type_is_still_the_default() {
        let (_root, env) = system("xhtml");
        install(
            &env,
            "b.desktop",
            "x-scheme-handler/http;x-scheme-handler/https;",
        );
        assert_eq!(
            settings(&env, &["check", "default-web-browser", "b.desktop"]).1,
            "yes\n"
        );
    }

    #[test]
    fn a_scheme_handler_is_set_and_read_alone() {
        let (_root, env) = system("scheme");
        install(&env, "mail.desktop", "x-scheme-handler/mailto;");
        let get = ["get", "default-url-scheme-handler", "mailto"];
        assert_eq!(settings(&env, &get).1, "mail.desktop\n");
        assert_eq!(
            settings(
                &env,
                &[
                    "set",
                    "default-url-scheme-handler",
                    "MAILTO",
                    "mail.desktop"
                ]
            )
            .0,
            status::OK
        );
        assert_eq!(
            settings(
                &env,
                &[
                    "check",
                    "default-url-scheme-handler",
                    "mailto",
                    "mail.desktop"
                ]
            )
            .1,
            "yes\n"
        );
    }

    #[test]
    fn setting_something_not_installed_is_refused_and_writes_nothing() {
        let (_root, env) = system("missing");
        let (code, _, err) = settings(&env, &["set", "default-web-browser", "ghost.desktop"]);
        assert_eq!(code, status::MISSING, "{err}");
        assert_eq!(
            settings(&env, &["set", "default-web-browser", "not-a-desktop-id"]).0,
            status::USAGE
        );
        assert!(!env.user_mimeapps().exists());
    }

    #[test]
    fn nothing_installed_gets_an_empty_answer() {
        let (_root, env) = system("empty");
        assert_eq!(
            settings(&env, &["get", "default-web-browser"]),
            (status::OK, String::new(), String::new())
        );
    }

    #[test]
    fn malformed_calls_are_usage_errors() {
        let (_root, env) = system("usage");
        for words in [
            &[][..],
            &["get"],
            &["frobnicate", "default-web-browser"],
            &["get", "default-web-browser", "extra"],
            &["set", "default-web-browser"],
            &["get", "screensaver"],
        ] {
            assert_eq!(settings(&env, words).0, status::USAGE, "{words:?}");
        }
        assert_eq!(settings(&env, &["--list"]).0, status::OK);
    }
}
