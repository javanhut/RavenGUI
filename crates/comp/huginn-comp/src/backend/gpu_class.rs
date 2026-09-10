//! Is the GPU the compositor renders on integrated or discrete?
//!
//! The answer sets the default for the effects `desktop.toml` leaves unset:
//! blur is a full-screen offscreen pass plus two shader passes over it on
//! every frame a panel or glass window is up, which an integrated GPU sharing
//! its memory bus with the CPU feels and a card with its own memory does not.
//! Rather than make every owner of a thin laptop find the switch, the
//! compositor guesses from the hardware and lets the file overrule it
//! ([`crate::desktop_config::DesktopConfig::blur`]).
//!
//! # The heuristic
//!
//! Read from `/sys/class/drm/<card>/device`, which for a PCI GPU is the PCI
//! device's sysfs directory and for a SoC's display block is the platform
//! device's. In order:
//!
//! 1. No `vendor` file: not a PCI device, so a SoC (`vc4`, `panfrost`, `msm`,
//!    …). Integrated by construction.
//! 2. Driver `i915` or `xe`: Intel. Integrated when the device sits at PCI
//!    `0000:00:02.0`, where every Intel iGPU has lived since the chipset
//!    graphics days; an Arc card is behind a bridge at some other address.
//! 3. Driver `amdgpu` or `radeon`: integrated when there is no
//!    `mem_info_vram_total` or it reports under [`VRAM_CARVEOUT`]. An APU has
//!    no memory of its own and reports the firmware's carve-out of system RAM,
//!    which is a few hundred megabytes to a gigabyte; a card reports the
//!    memory soldered to it. (`radeon` never exposes the file, so an old
//!    Radeon card comes out integrated — blur off, which on a card of that
//!    age is the right answer anyway.)
//! 4. Driver `nvidia` or `nouveau`: discrete. NVIDIA's integrated parts are
//!    Tegra, which is not PCI and is caught by rule 1.
//! 5. Anything else — a virtual machine's `virtio_gpu` or `vmwgfx`, a driver
//!    this list has never met — is [`GpuClass::Unknown`], which keeps the
//!    compiled-in defaults.
//!
//! It is a guess, deliberately shallow, and wrong in the ways the list above
//! admits. That is fine: it only chooses a default, and a value in the file
//! always wins.

use std::path::{Path, PathBuf};

/// A GPU's class, as far as sysfs tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuClass {
    /// Shares system memory with the CPU: an iGPU, an APU, a SoC.
    Integrated,
    /// A card with memory of its own.
    Discrete,
    /// Nothing the heuristic recognises.
    Unknown,
}

impl GpuClass {
    /// Whether blur is on when `desktop.toml` does not say. Off only where the
    /// hardware is known to be integrated: an unknown GPU gets the look the
    /// desktop was designed with.
    pub(crate) const fn blur_by_default(self) -> bool {
        !matches!(self, Self::Integrated)
    }
}

/// Below this much dedicated VRAM an AMD device is an APU with a carve-out
/// rather than a card. The smallest card `amdgpu` drives has 2 GiB; the
/// largest carve-out firmware commonly configures is less.
const VRAM_CARVEOUT: u64 = 2 << 30;

/// Where an Intel iGPU is on the PCI bus, without exception so far.
const INTEL_IGPU_ADDRESS: &str = "0000:00:02.0";

/// What sysfs says about the GPU behind a DRM node, for the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GpuInfo {
    pub(crate) class: GpuClass,
    /// The kernel driver bound to the device, when one is.
    pub(crate) driver: Option<String>,
    /// `0x1234`, as sysfs writes it, for a PCI device.
    pub(crate) vendor: Option<String>,
}

/// Classify the GPU behind `/dev/dri/<node>`.
pub(crate) fn classify(node: &Path) -> GpuInfo {
    match node.file_name() {
        Some(name) => classify_sysfs(&Path::new("/sys/class/drm").join(name).join("device")),
        None => GpuInfo {
            class: GpuClass::Unknown,
            driver: None,
            vendor: None,
        },
    }
}

/// Classify from the device directory a DRM node's `device` link points at.
/// Split from [`classify`] so a fabricated tree can stand in for sysfs.
pub(crate) fn classify_sysfs(device: &Path) -> GpuInfo {
    if !device.is_dir() {
        return GpuInfo {
            class: GpuClass::Unknown,
            driver: None,
            vendor: None,
        };
    }
    let driver = std::fs::read_link(device.join("driver"))
        .ok()
        .and_then(|link| Some(link.file_name()?.to_string_lossy().into_owned()));
    let vendor = read_trimmed(&device.join("vendor"));
    let class = match (driver.as_deref(), vendor.as_deref()) {
        (_, None) => GpuClass::Integrated,
        (Some("i915" | "xe"), _) => {
            if pci_address(device).as_deref() == Some(INTEL_IGPU_ADDRESS) {
                GpuClass::Integrated
            } else {
                GpuClass::Discrete
            }
        }
        (Some("amdgpu" | "radeon"), _) => {
            let vram = read_trimmed(&device.join("mem_info_vram_total"))
                .and_then(|s| s.parse::<u64>().ok());
            match vram {
                Some(bytes) if bytes >= VRAM_CARVEOUT => GpuClass::Discrete,
                _ => GpuClass::Integrated,
            }
        }
        (Some("nvidia" | "nouveau"), _) => GpuClass::Discrete,
        _ => GpuClass::Unknown,
    };
    GpuInfo {
        class,
        driver,
        vendor,
    }
}

/// The PCI address a device directory is named after, once the `device`
/// symlink's `../../..` are resolved.
fn pci_address(device: &Path) -> Option<String> {
    let real: PathBuf = std::fs::canonicalize(device).ok()?;
    Some(real.file_name()?.to_string_lossy().into_owned())
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway sysfs, removed when the test ends. `device` is the
    /// directory `classify_sysfs` reads, named after a PCI address the way
    /// the real one is.
    struct Sysfs {
        root: PathBuf,
        device: PathBuf,
    }

    impl Sysfs {
        fn pci(name: &str, address: &str, driver: Option<&str>, vendor: &str) -> Self {
            let this = Self::bare(name, address);
            std::fs::write(this.device.join("vendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(this.device.join("class"), "0x030000\n").unwrap();
            if let Some(driver) = driver {
                this.bind(driver);
            }
            this
        }

        /// A device directory with nothing in it yet.
        fn bare(name: &str, address: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("raven-gpu-class-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let device = root.join("devices").join(address);
            std::fs::create_dir_all(&device).unwrap();
            std::fs::create_dir_all(root.join("bus/pci/drivers")).unwrap();
            Self { root, device }
        }

        fn bind(&self, driver: &str) {
            let target = self.root.join("bus/pci/drivers").join(driver);
            std::fs::create_dir_all(&target).unwrap();
            std::os::unix::fs::symlink(target, self.device.join("driver")).unwrap();
        }

        fn vram(&self, bytes: u64) {
            std::fs::write(
                self.device.join("mem_info_vram_total"),
                format!("{bytes}\n"),
            )
            .unwrap();
        }

        fn class(&self) -> GpuClass {
            classify_sysfs(&self.device).class
        }
    }

    impl Drop for Sysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn intel_at_the_igpu_slot_is_integrated() {
        let fs = Sysfs::pci("i915", "0000:00:02.0", Some("i915"), "0x8086");
        let info = classify_sysfs(&fs.device);
        assert_eq!(info.class, GpuClass::Integrated);
        assert_eq!(info.driver.as_deref(), Some("i915"));
        assert_eq!(info.vendor.as_deref(), Some("0x8086"));
        assert!(!info.class.blur_by_default());

        let xe = Sysfs::pci("xe", "0000:00:02.0", Some("xe"), "0x8086");
        assert_eq!(xe.class(), GpuClass::Integrated);
    }

    #[test]
    fn intel_behind_a_bridge_is_a_card() {
        let arc = Sysfs::pci("arc", "0000:03:00.0", Some("xe"), "0x8086");
        assert_eq!(arc.class(), GpuClass::Discrete);
    }

    #[test]
    fn amd_is_classed_by_vram() {
        let apu = Sysfs::pci("apu", "0000:05:00.0", Some("amdgpu"), "0x1002");
        apu.vram(512 << 20);
        assert_eq!(apu.class(), GpuClass::Integrated);

        let card = Sysfs::pci("card", "0000:03:00.0", Some("amdgpu"), "0x1002");
        card.vram(8 << 30);
        assert_eq!(card.class(), GpuClass::Discrete);

        // The boundary itself is a card: the smallest amdgpu drives.
        let small = Sysfs::pci("small", "0000:03:00.0", Some("amdgpu"), "0x1002");
        small.vram(VRAM_CARVEOUT);
        assert_eq!(small.class(), GpuClass::Discrete);

        // No file at all — `radeon`, or a kernel without the attribute.
        let old = Sysfs::pci("radeon", "0000:01:00.0", Some("radeon"), "0x1002");
        assert_eq!(old.class(), GpuClass::Integrated);
    }

    #[test]
    fn nvidia_is_discrete_under_either_driver() {
        let nouveau = Sysfs::pci("nouveau", "0000:01:00.0", Some("nouveau"), "0x10de");
        assert_eq!(nouveau.class(), GpuClass::Discrete);
        let nvidia = Sysfs::pci("nvidia", "0000:01:00.0", Some("nvidia"), "0x10de");
        assert_eq!(nvidia.class(), GpuClass::Discrete);
        assert!(nvidia.class().blur_by_default());
    }

    #[test]
    fn a_soc_has_no_pci_vendor_and_is_integrated() {
        let soc = Sysfs::bare("soc", "fec00000.display");
        soc.bind("vc4");
        let info = classify_sysfs(&soc.device);
        assert_eq!(info.class, GpuClass::Integrated);
        assert_eq!(info.driver.as_deref(), Some("vc4"));
        assert_eq!(info.vendor, None);
    }

    #[test]
    fn a_stranger_is_unknown_and_keeps_the_defaults() {
        let vm = Sysfs::pci("virtio", "0000:00:01.0", Some("virtio_gpu"), "0x1af4");
        assert_eq!(vm.class(), GpuClass::Unknown);
        assert!(vm.class().blur_by_default());

        // A PCI device no driver has bound.
        let unbound = Sysfs::pci("unbound", "0000:01:00.0", None, "0x8086");
        assert_eq!(unbound.class(), GpuClass::Unknown);

        // No such directory: the node has no sysfs entry at all.
        let info = classify_sysfs(Path::new("/nonexistent/raven-gpu-class"));
        assert_eq!(info.class, GpuClass::Unknown);
    }

    /// This is what the machine the desktop is developed on looks like; the
    /// test only asserts when it is that machine.
    #[test]
    fn the_real_sysfs_parses() {
        let Ok(nodes) = std::fs::read_dir("/sys/class/drm") else {
            return;
        };
        for node in nodes.flatten() {
            let name = node.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let info = classify(&Path::new("/dev/dri").join(&*name));
            if info.driver.as_deref() == Some("i915")
                && pci_address(&node.path().join("device")).as_deref() == Some(INTEL_IGPU_ADDRESS)
            {
                assert_eq!(info.class, GpuClass::Integrated, "{name}");
            }
        }
    }
}
