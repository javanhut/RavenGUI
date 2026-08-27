//! Installed applications, and the icons that go with them.
//!
//! What a launcher and a dock both need and neither should implement twice:
//! find the `.desktop` files, work out which are meant to be shown, turn an
//! `Icon=` name into a file on disk, and turn an `Exec=` into an argv that is
//! safe to hand to `Command`.
//!
//! Deliberately free of Wayland, of any toolkit, and of any display: it is all
//! filesystem and string work, so it tests on any machine and in milliseconds.
//! That matters more than usual here, because the target distro ships almost no
//! GUI libraries — a crate that needed a session to test would be untestable on
//! the box it is built on.
//!
//! The XDG specs are implemented here rather than taken from a crate. See
//! [`entry`] for why that is a security decision rather than a taste one.

/// `$XDG_DATA_DIRS` when the environment does not set it, per the basedir
/// specification.
///
/// One constant because both the application search and the icon search fall
/// back to it, and two directories that are supposed to be the same list are
/// exactly the sort of thing that drifts apart.
pub(crate) const DEFAULT_DATA_DIRS: &str = "/usr/local/share:/usr/share";

pub mod entry;
pub mod icon;
pub mod pixmap;
pub mod search;

pub use entry::{Entry, Skipped, directories};
pub use icon::Icons;
pub use pixmap::{Pixmap, Pixmaps};
pub use search::{Frecency, Hit, Quality, search};
