//! The ALSA Master control can silence a card independently of PipeWire's
//! sink volume. Only touch the card behind the current default sink, and only
//! raise Master when it is zero or switch it on when it is off.

use std::ffi::CString;
use std::process::{Command, Stdio};

use alsa::Round;
use alsa::ctl::{Ctl, ElemId, ElemIface, ElemType, ElemValue};
use alsa::mixer::MilliBel;

use super::SINK;

const VOLUME: &str = "Master Playback Volume";
const SWITCH: &str = "Master Playback Switch";

fn default_alsa_card() -> Option<u32> {
    let output = Command::new("wpctl")
        .args(["inspect", SINK])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_card(&String::from_utf8_lossy(&output.stdout))
}

fn parse_card(inspect: &str) -> Option<u32> {
    inspect.lines().find_map(|line| {
        line.trim()
            .strip_prefix("alsa.card = ")?
            .trim_matches('"')
            .parse()
            .ok()
    })
}

fn control(ctl: &Ctl, name: &str, kind: ElemType) -> Option<(ElemId, ElemValue)> {
    let mut id = ElemId::new(ElemIface::Mixer);
    id.set_name(&CString::new(name).ok()?);
    let mut value = ElemValue::new(kind).ok()?;
    value.set_id(&id);
    ctl.elem_read(&mut value).ok()?;
    Some((id, value))
}

pub(super) fn silent() -> Option<bool> {
    let ctl = Ctl::new(&format!("hw:{}", default_alsa_card()?), false).ok()?;
    let volume =
        control(&ctl, VOLUME, ElemType::Integer).and_then(|(_, value)| value.get_integer(0));
    let switch =
        control(&ctl, SWITCH, ElemType::Boolean).and_then(|(_, value)| value.get_boolean(0));
    if volume.is_none() && switch.is_none() {
        None
    } else {
        Some(volume == Some(0) || switch == Some(false))
    }
}

pub(super) fn make_audible() {
    let Some(card) = default_alsa_card() else {
        return; // Bluetooth, virtual and other sinks have no ALSA Master.
    };
    let Ok(ctl) = Ctl::new(&format!("hw:{card}"), false) else {
        return;
    };

    if let Some((id, mut value)) = control(&ctl, VOLUME, ElemType::Integer)
        && value.get_integer(0) == Some(0)
    {
        // Cap recovery at -12 dB, even if the card supports positive gain.
        // PipeWire still controls the ordinary 0–100% slider above this.
        let target = ctl.get_db_range(&id).and_then(|(min, max)| {
            let db = MilliBel((-1200).clamp(min.0, max.0));
            ctl.convert_from_db(&id, db, Round::Floor)
        });
        match target {
            Ok(raw) if raw > 0 => {
                if value.set_integer(0, raw as i32).is_some() {
                    if let Err(error) = ctl.elem_write(&value) {
                        tracing::warn!(card, %error, "could not restore ALSA Master volume");
                    }
                }
            }
            Ok(_) => tracing::warn!(card, "ALSA Master recovery level was zero"),
            Err(error) => {
                tracing::warn!(card, %error, "could not calculate ALSA Master recovery level")
            }
        }
    }

    if let Some((_, mut value)) = control(&ctl, SWITCH, ElemType::Boolean)
        && value.get_boolean(0) == Some(false)
    {
        if value.set_boolean(0, true).is_some() {
            if let Err(error) = ctl.elem_write(&value) {
                tracing::warn!(card, %error, "could not unmute ALSA Master");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_card;

    #[test]
    fn card_comes_from_the_selected_sink_only() {
        assert_eq!(
            parse_card("alsa.card = \"2\"\napi.alsa.pcm.card = \"0\""),
            Some(2)
        );
        assert_eq!(parse_card("node.name = \"bluez_output.foo\""), None);
        assert_eq!(parse_card("api.alsa.pcm.card = \"0\""), None);
    }
}
