//! The command line, turned into the list of requests the probe will send.
//! Kept free of Wayland so it can be tested without a compositor.

use std::time::Duration;

pub(crate) const USAGE: &str =
    "ime-probe — act as the input method for the focused text-input-v3 client

usage: ime-probe [--timeout MS] [--delay MS] STEP...

  --timeout MS   how long to wait for a focused, enabled text input (default 10000;
                 0 sends at once, without waiting for activation)
  --delay MS     pause inserted between steps                      (default 0)

steps:
  preedit TEXT [--cursor N|none]   composing text, caret after N characters (default: end)
  commit TEXT                      insert TEXT, ending any preedit
  clear                            empty the preedit
  delete BEFORE AFTER              delete_surrounding_text, in bytes
  wait MS                          sleep, holding the input method
  key [MODS+]NAME                  press and release a key on a virtual keyboard
                                   (MODS: ctrl, shift, alt, super)

example:
  ime-probe preedit nihon wait 500 preedit 日本 --cursor 1 wait 500 commit 日本";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    /// `set_preedit_string` then `commit`; begin and end are byte offsets,
    /// both -1 to hide the caret.
    Preedit {
        text: String,
        begin: i32,
        end: i32,
    },
    /// `commit_string` then `commit`.
    Commit(String),
    Delete {
        before: u32,
        after: u32,
    },
    Wait(Duration),
    /// Evdev key code and the xkb modifier mask held while it is pressed.
    Key {
        code: u32,
        mods: u32,
    },
}

impl Op {
    pub(crate) fn needs_input_method(&self) -> bool {
        matches!(self, Op::Preedit { .. } | Op::Commit(_) | Op::Delete { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) timeout: Duration,
    pub(crate) ops: Vec<Op>,
}

impl Plan {
    pub(crate) fn needs_virtual_keyboard(&self) -> bool {
        self.ops.iter().any(|op| matches!(op, Op::Key { .. }))
    }
}

/// Byte offset of the caret after `chars` characters of `text`.
pub(crate) fn char_to_byte(text: &str, chars: usize) -> Result<i32, String> {
    let byte = if chars == text.chars().count() {
        text.len()
    } else {
        text.char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .ok_or_else(|| format!("cursor {chars} is past the end of {text:?}"))?
    };
    i32::try_from(byte).map_err(|_| String::from("preedit too long"))
}

/// Evdev codes, as the virtual keyboard sends them (no xkb offset of 8).
const KEYS: &[(&str, u32)] = &[
    ("escape", 1),
    ("esc", 1),
    ("minus", 12),
    ("-", 12),
    ("equal", 13),
    ("=", 13),
    ("backspace", 14),
    ("tab", 15),
    ("enter", 28),
    ("return", 28),
    ("space", 57),
    ("q", 16),
    ("w", 17),
    ("e", 18),
    ("r", 19),
    ("t", 20),
    ("y", 21),
    ("u", 22),
    ("i", 23),
    ("o", 24),
    ("p", 25),
    ("a", 30),
    ("s", 31),
    ("d", 32),
    ("f", 33),
    ("g", 34),
    ("h", 35),
    ("j", 36),
    ("k", 37),
    ("l", 38),
    ("z", 44),
    ("x", 45),
    ("c", 46),
    ("v", 47),
    ("b", 48),
    ("n", 49),
    ("m", 50),
    ("home", 102),
    ("up", 103),
    ("pageup", 104),
    ("left", 105),
    ("right", 106),
    ("end", 107),
    ("down", 108),
    ("pagedown", 109),
    ("insert", 110),
    ("delete", 111),
    ("f11", 87),
    ("f12", 88),
];

/// xkb modifier masks of the stock keymap: Shift, Control, Mod1, Mod4.
const MODS: &[(&str, u32)] = &[
    ("shift", 1),
    ("ctrl", 4),
    ("control", 4),
    ("alt", 8),
    ("super", 64),
    ("logo", 64),
];

fn key_code(name: &str) -> Option<u32> {
    if let Some(&(_, code)) = KEYS.iter().find(|(n, _)| *n == name) {
        return Some(code);
    }
    match name.as_bytes() {
        [d @ b'1'..=b'9'] => Some(u32::from(d - b'1') + 2),
        [b'0'] => Some(11),
        [b'f', rest @ ..] => match std::str::from_utf8(rest).ok()?.parse::<u32>().ok()? {
            n @ 1..=10 => Some(58 + n),
            _ => None,
        },
        _ => None,
    }
}

/// Evdev code and xkb modifier mask for `ctrl+shift+a`, `enter`, `5` ...
pub(crate) fn key_chord(chord: &str) -> Result<(u32, u32), String> {
    let lower = chord.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    let key = parts.pop().unwrap_or_default();
    let mut mods = 0;
    for m in parts {
        let &(_, mask) = MODS
            .iter()
            .find(|(n, _)| *n == m)
            .ok_or_else(|| format!("unknown modifier {m:?} in {chord:?}"))?;
        mods |= mask;
    }
    let code = key_code(key).ok_or_else(|| format!("unknown key {chord:?}"))?;
    Ok((code, mods))
}

fn millis(flag: &str, value: Option<&String>) -> Result<Duration, String> {
    value
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .ok_or_else(|| format!("{flag} needs a number of milliseconds"))
}

pub(crate) fn parse_args(args: &[String]) -> Result<Plan, String> {
    let mut timeout = Duration::from_secs(10);
    let mut delay = Duration::ZERO;
    let mut steps = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        let op = match arg.as_str() {
            "--timeout" => {
                timeout = millis("--timeout", it.next())?;
                continue;
            }
            "--delay" => {
                delay = millis("--delay", it.next())?;
                continue;
            }
            "preedit" => {
                let text = it.next().ok_or("preedit needs text")?.clone();
                let mut caret = Some(text.chars().count());
                if it.peek().is_some_and(|a| *a == "--cursor") {
                    it.next();
                    caret = match it.next().map(String::as_str) {
                        Some("none") => None,
                        Some("end") => Some(text.chars().count()),
                        Some(n) => Some(n.parse().map_err(|_| format!("bad --cursor {n:?}"))?),
                        None => return Err("--cursor needs a value".into()),
                    };
                }
                let at = match caret {
                    Some(c) => char_to_byte(&text, c)?,
                    None => -1,
                };
                Op::Preedit {
                    text,
                    begin: at,
                    end: at,
                }
            }
            "commit" => Op::Commit(it.next().ok_or("commit needs text")?.clone()),
            "clear" => Op::Preedit {
                text: String::new(),
                begin: 0,
                end: 0,
            },
            "delete" => {
                let mut n = || -> Result<u32, String> {
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| String::from("delete needs BEFORE and AFTER byte counts"))
                };
                Op::Delete {
                    before: n()?,
                    after: n()?,
                }
            }
            "wait" => Op::Wait(millis("wait", it.next())?),
            "key" => {
                let (code, mods) = key_chord(it.next().ok_or("key needs a key name")?)?;
                Op::Key { code, mods }
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown step {other:?}\n\n{USAGE}")),
        };
        steps.push(op);
    }
    if steps.is_empty() {
        return Err(USAGE.into());
    }
    let mut ops = Vec::with_capacity(steps.len() * 2);
    for op in steps {
        let gap = !delay.is_zero()
            && !matches!(op, Op::Wait(_))
            && ops.last().is_some_and(|prev| !matches!(prev, Op::Wait(_)));
        if gap {
            ops.push(Op::Wait(delay));
        }
        ops.push(op);
    }
    Ok(Plan { timeout, ops })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn cursor_counts_characters_but_sends_bytes() {
        assert_eq!(char_to_byte("nihon", 5), Ok(5));
        assert_eq!(char_to_byte("日本", 0), Ok(0));
        assert_eq!(char_to_byte("日本", 1), Ok(3));
        assert_eq!(char_to_byte("日本", 2), Ok(6));
        assert_eq!(char_to_byte("aé😀b", 3), Ok(7));
        assert!(char_to_byte("日本", 3).is_err());
    }

    #[test]
    fn preedit_then_commit() {
        let plan = parse_args(&args(&[
            "preedit", "nihon", "wait", "500", "preedit", "日本", "--cursor", "1", "commit", "日本",
        ]))
        .unwrap();
        assert_eq!(plan.timeout, ms(10_000));
        assert_eq!(
            plan.ops,
            vec![
                Op::Preedit {
                    text: "nihon".into(),
                    begin: 5,
                    end: 5
                },
                Op::Wait(ms(500)),
                Op::Preedit {
                    text: "日本".into(),
                    begin: 3,
                    end: 3
                },
                Op::Commit("日本".into()),
            ]
        );
        assert!(!plan.needs_virtual_keyboard());
    }

    #[test]
    fn hidden_caret_clear_and_delete() {
        let plan = parse_args(&args(&[
            "preedit", "abc", "--cursor", "none", "clear", "delete", "2", "0",
        ]))
        .unwrap();
        assert_eq!(
            plan.ops,
            vec![
                Op::Preedit {
                    text: "abc".into(),
                    begin: -1,
                    end: -1
                },
                Op::Preedit {
                    text: String::new(),
                    begin: 0,
                    end: 0
                },
                Op::Delete {
                    before: 2,
                    after: 0
                },
            ]
        );
    }

    #[test]
    fn delay_goes_between_steps_only() {
        let plan = parse_args(&args(&[
            "--delay",
            "100",
            "--timeout",
            "2000",
            "preedit",
            "a",
            "commit",
            "a",
            "wait",
            "50",
            "clear",
        ]))
        .unwrap();
        assert_eq!(plan.timeout, ms(2000));
        assert_eq!(
            plan.ops,
            vec![
                Op::Preedit {
                    text: "a".into(),
                    begin: 1,
                    end: 1
                },
                Op::Wait(ms(100)),
                Op::Commit("a".into()),
                Op::Wait(ms(50)),
                Op::Preedit {
                    text: String::new(),
                    begin: 0,
                    end: 0
                },
            ]
        );
    }

    #[test]
    fn keys() {
        assert_eq!(key_chord("a"), Ok((30, 0)));
        assert_eq!(key_chord("enter"), Ok((28, 0)));
        assert_eq!(key_chord("Escape"), Ok((1, 0)));
        assert_eq!(key_chord("1"), Ok((2, 0)));
        assert_eq!(key_chord("0"), Ok((11, 0)));
        assert_eq!(key_chord("ctrl+shift+space"), Ok((57, 4 | 1)));
        assert_eq!(key_chord("super+alt+f12"), Ok((88, 64 | 8)));
        assert!(key_chord("ctrl+").is_err());
        assert!(key_chord("hyper+a").is_err());
        assert!(key_chord("nosuchkey").is_err());
        let plan = parse_args(&args(&["key", "ctrl+c"])).unwrap();
        assert_eq!(plan.ops, vec![Op::Key { code: 46, mods: 4 }]);
        assert!(plan.needs_virtual_keyboard());
        assert!(!plan.ops[0].needs_input_method());
    }

    #[test]
    fn rejects_bad_command_lines() {
        for bad in [
            &[][..],
            &["preedit"],
            &["preedit", "ab", "--cursor", "3"],
            &["preedit", "ab", "--cursor", "x"],
            &["commit"],
            &["wait", "soon"],
            &["delete", "1"],
            &["--timeout"],
            &["--delay", "-1", "clear"],
            &["frobnicate"],
            &["key"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?} must be rejected");
        }
    }
}
