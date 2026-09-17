//! The Huginn compositor.
//!
//! Huginn is one of Odin's two ravens — *thought*. It flies out over the world
//! at dawn and reports back what it saw.
//!
//! Window-management behaviour lives in `huginn-core`, which has no Wayland or
//! GPU dependency and is tested on its own. This binary is the part that cannot
//! be tested without hardware: session setup, the event loop, protocol
//! handlers, and rendering.

#[cfg(target_os = "linux")]
mod anim;
#[cfg(target_os = "linux")]
mod appwatch;
mod audio;
#[cfg(target_os = "linux")]
mod backend;
#[cfg(target_os = "linux")]
mod bluetooth;
mod blur;
#[cfg(target_os = "linux")]
mod canvas;
mod configwatch;
#[cfg(target_os = "linux")]
mod decor;
#[cfg(target_os = "linux")]
mod desktop_config;
mod dmabuf;
#[cfg(target_os = "linux")]
mod dock;
mod fileindex;
mod focus;
mod frametime;
#[cfg(target_os = "linux")]
mod gesture;
#[cfg(target_os = "linux")]
mod launcher;
#[cfg(target_os = "linux")]
mod motion;
#[cfg(target_os = "linux")]
mod mouse;
#[cfg(target_os = "linux")]
mod notifications;
#[cfg(target_os = "linux")]
mod overlay;
mod overview;
mod pinned;
mod pins;
#[cfg(target_os = "linux")]
mod pointer;
#[cfg(target_os = "linux")]
mod popup;
#[cfg(target_os = "linux")]
mod record;
#[cfg(target_os = "linux")]
mod render;
#[cfg(target_os = "linux")]
mod screenshot;
#[cfg(target_os = "linux")]
mod settings;
#[cfg(target_os = "linux")]
mod shake;
#[cfg(target_os = "linux")]
mod shell_protocol;
#[cfg(target_os = "linux")]
mod sleep;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod switcher;
#[cfg(target_os = "linux")]
mod text;
#[cfg(target_os = "linux")]
mod theme;
#[cfg(target_os = "linux")]
mod userdirs;
#[cfg(target_os = "linux")]
mod wallpaper;
#[cfg(target_os = "linux")]
mod wheel;
#[cfg(target_os = "linux")]
mod window;
#[cfg(target_os = "linux")]
mod xwayland;

/// What `--help` prints.
///
/// Written out rather than derived from an argument parser: huginn takes one
/// flag, and a dependency that exists to format this paragraph would be a
/// larger thing than the paragraph.
const USAGE: &str = "\
huginn -- the RavenLinux Wayland compositor.

Usage: huginn [--backend <udev|winit>]

Options:
  --backend <udev|winit>
          Which backend to drive.
            udev   the real thing: DRM/KMS and libinput on a TTY.
            winit  the whole compositor nested in a window on an existing
                   desktop session, which is where development happens.
          Default: winit when WAYLAND_DISPLAY is set -- running inside a
          session almost always means development -- and udev otherwise.

  -h, --help
          Print this and exit.

  -V, --version
          Print the version and exit.

Environment:
  RUST_LOG          Log filter. Default: huginn=info,smithay=warn.
  WAYLAND_DISPLAY   When set, selects the winit backend by default.

huginn is normally started by raven-wayland-session, which execs it as
`huginn --backend udev`. Running it by hand from inside a session gives you
the nested backend: a compositor in a window, not a takeover of the screen.
";

/// Warn about flags that were passed and are not understood.
///
/// A warning rather than an error on purpose: this process is the desktop, and
/// raven-wayland-session `exec`s it as the last line of the script. Refusing to
/// start over an argument nobody reads would turn a typo into a machine with no
/// session to log into, which is far worse than the typo.
#[cfg(target_os = "linux")]
fn warn_about_unknown_flags(args: &[String]) {
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            // The one flag that takes a value; the value is not a flag.
            "--backend" => {
                rest.next();
            }
            other if other.starts_with('-') => {
                tracing::warn!(flag = other, "unrecognised flag, ignored; try --help");
            }
            other => tracing::warn!(argument = other, "unexpected argument, ignored"),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Answered before the logger is up: these two are questions asked at a
    // terminal, and the answer should be the only thing written there.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("huginn {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // Info by default. The session log is what an installed system keeps in
    // /var/log/raven/ravend.log, and at debug every window decoration and
    // every popup reposition went into it: on one desktop, half the lines
    // written were debug, and the warnings that mattered sat between them.
    // RUST_LOG still overrides, and `imlazy run` sets it to debug for
    // development; see lazy.toml.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "huginn=info,smithay=warn".into()),
        )
        .init();

    #[cfg(target_os = "linux")]
    {
        warn_about_unknown_flags(&args);
        let chosen = backend::Backend::detect(&args);
        tracing::info!(backend = ?chosen, "starting huginn");

        let result = match chosen {
            backend::Backend::Winit => backend::winit::run(),
            backend::Backend::Udev => backend::udev::run(),
        };

        if let Err(e) = result {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::error!(
            "huginn targets Linux only: it needs DRM/KMS, libinput and libseat, \
             which have no macOS equivalent. Build it on the RavenLinux host. \
             huginn-core, raven-config and raven-protocol build here — run \
             `cargo test -p huginn-core` for window-management work."
        );
        std::process::exit(1);
    }
}
