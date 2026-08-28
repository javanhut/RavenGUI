//! Print the installed applications and where their icons resolve to.
//!
//! Run with `cargo run -p raven-desktop --example inventory`. Exists so the
//! lookup can be checked against a real machine rather than only fixtures.

use raven_desktop::{Icons, entry};

/// The application list, exactly as the launcher builds it.
///
/// Shares `directories` and `shadows` with the real scan rather than
/// re-deriving them, so an inventory run reports what the launcher would
/// actually show — an example that searched different directories, or listed
/// both copies of a shadowed application, would be worse than no example.
fn installed(current: &[String]) -> Vec<raven_desktop::Entry> {
    let mut apps = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for dir in entry::directories() {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            if entry::shadows(&mut seen, &path) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(app) = entry::parse(&text, &path, current) {
                apps.push(app);
            }
        }
    }
    apps.sort_by(|a, b| a.path.cmp(&b.path));
    apps
}

fn main() {
    let current: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let icons =
        Icons::discover(&std::env::var("RAVEN_ICON_THEME").unwrap_or_else(|_| "hicolor".into()));

    for dir in entry::directories() {
        println!("searching {}", dir.display());
    }

    let all = installed(&current);
    let mut without_icon = 0;

    for app in &all {
        let icon = app.icon.as_deref().and_then(|name| icons.find(name, 48, 1));
        if icon.is_none() {
            without_icon += 1;
        }
        if std::env::var("RAVEN_VERBOSE").is_ok() || app.name.contains("Raven") {
            println!(
                "{:<28} argv={:?}\n{:<28} icon={}",
                app.name,
                app.argv(&[]).unwrap_or_default(),
                "",
                icon.map_or_else(|| "<none>".into(), |p| p.display().to_string()),
            );
        }
    }
    println!(
        "\n{} applications, {without_icon} with no resolvable icon",
        all.len()
    );

    // Rank the real desktop against a few queries, so the ordering can be
    // judged against what is installed rather than against a fixture.
    let frecency = raven_desktop::Frecency::new();
    for query in ["te", "fi", "term", "raven", "web"] {
        let hits = raven_desktop::search(&all, query, &frecency, 1_700_000_000);
        let top: Vec<&str> = hits
            .iter()
            .take(3)
            .map(|h| all[h.index].name.as_str())
            .collect();
        println!("{query:>8} -> {top:?}");
    }
}
