//! Explicit Linux hardware capabilities for sandboxed Bash.
//!
//! Discovery is restricted to well-known device nodes and DRM render/card
//! nodes with a matching kernel-reported PCI vendor. No /dev directory is
//! mounted wholesale. Hardware mounts share the pinned-FD infrastructure
//! used for other opted-in host services.
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    host_services::{HostMount, HostMountKind, HostShell},
    setup::GpuVendor,
};

const AMD_VENDOR: &str = "0x1002";
const NVIDIA_VENDOR: &str = "0x10de";

#[derive(Debug, Default)]
struct Discovery {
    kvm: Option<PathBuf>,
    amd: Vec<PathBuf>,
    nvidia: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Available {
    pub kvm: bool,
    pub amd: bool,
    pub nvidia: bool,
}

#[derive(Debug)]
struct Layout<'a> {
    dev: &'a Path,
    drm_sys: &'a Path,
}

impl Layout<'_> {
    fn real() -> Layout<'static> {
        Layout {
            dev: Path::new("/dev"),
            drm_sys: Path::new("/sys/class/drm"),
        }
    }
}

fn character_device(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_char_device())
}

fn drm_node_name(name: &str) -> bool {
    [("renderD", 3), ("card", 1)]
        .iter()
        .any(|(prefix, min_digits)| {
            name.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.len() >= *min_digits && suffix.bytes().all(|c| c.is_ascii_digit())
            })
        })
}

fn nvidia_node_name(name: &str) -> bool {
    matches!(
        name,
        "nvidiactl" | "nvidia-uvm" | "nvidia-uvm-tools" | "nvidia-modeset"
    ) || name
        .strip_prefix("nvidia")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit()))
}

fn nvidia_cap_name(name: &str) -> bool {
    name.strip_prefix("nvidia-cap")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit()))
}

fn drm_vendor(layout: &Layout<'_>, name: &str) -> Option<GpuVendor> {
    let vendor = fs::read_to_string(layout.drm_sys.join(name).join("device/vendor")).ok()?;
    match vendor.trim().to_ascii_lowercase().as_str() {
        AMD_VENDOR => Some(GpuVendor::Amd),
        NVIDIA_VENDOR => Some(GpuVendor::Nvidia),
        _ => None,
    }
}

fn matching_nodes(dir: &Path, predicate: fn(&str) -> bool) -> Vec<PathBuf> {
    let mut nodes = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(predicate) && character_device(&entry.path()) {
                nodes.push(entry.path());
            }
        }
    }
    nodes.sort();
    nodes
}

fn discover_at(layout: &Layout<'_>) -> Discovery {
    let mut found = Discovery::default();
    let kvm = layout.dev.join("kvm");
    if character_device(&kvm) {
        found.kvm = Some(kvm);
    }

    for path in matching_nodes(&layout.dev.join("dri"), drm_node_name) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        match drm_vendor(layout, name) {
            Some(GpuVendor::Amd) => found.amd.push(path),
            Some(GpuVendor::Nvidia) => found.nvidia.push(path),
            None => {} // Unknown/Intel devices must not be granted.
        }
    }

    // KFD is a shared AMD device, exposed only when AMD GPU access is requested.
    let kfd = layout.dev.join("kfd");
    if !found.amd.is_empty() && character_device(&kfd) {
        found.amd.push(kfd);
    }

    // NVIDIA CUDA/NVML devices are identified by their tightly scoped kernel
    // device names. A DRM render node alone is enough for some Vulkan drivers.
    found
        .nvidia
        .extend(matching_nodes(layout.dev, nvidia_node_name));
    found.nvidia.extend(matching_nodes(
        &layout.dev.join("nvidia-caps"),
        nvidia_cap_name,
    ));
    found.amd.sort();
    found.amd.dedup();
    found.nvidia.sort();
    found.nvidia.dedup();
    found
}

pub fn available() -> Available {
    let found = discover_at(&Layout::real());
    Available {
        kvm: found.kvm.is_some(),
        amd: !found.amd.is_empty(),
        nvidia: !found.nvidia.is_empty(),
    }
}

/// Pin the explicitly requested hardware and GPU runtime resources at dotlink
/// startup. Missing requested hardware fails loudly; missing optional driver
/// search directories are allowed because non-NixOS distributions can provide
/// those libraries through the ordinary system runtime mounts.
pub fn add_to_shell(shell: &mut HostShell, kvm: bool, vendors: &[GpuVendor]) -> Result<()> {
    let found = discover_at(&Layout::real());
    add_selected(shell, kvm, vendors, &found, &DriverDirectories::real())
}

#[derive(Debug)]
struct DriverDirectories<'a> {
    graphics: &'a [&'a str],
    sys: &'a Path,
    nvidia_proc: &'a Path,
}

impl DriverDirectories<'_> {
    fn real() -> DriverDirectories<'static> {
        DriverDirectories {
            graphics: &[
                "/run/opengl-driver",
                "/run/opengl-driver-32",
                "/etc/vulkan/icd.d",
                "/etc/OpenCL/vendors",
            ],
            sys: Path::new("/sys"),
            nvidia_proc: Path::new("/proc/driver/nvidia"),
        }
    }
}

fn add_selected(
    shell: &mut HostShell,
    kvm: bool,
    vendors: &[GpuVendor],
    discovered: &Discovery,
    driver_dirs: &DriverDirectories<'_>,
) -> Result<()> {
    let vendors = vendors.iter().copied().collect::<BTreeSet<_>>();
    if kvm {
        let node = discovered.kvm.as_ref()
            .context("--allow-kvm requested, but /dev/kvm is not available; check KVM support and device permissions")?;
        shell.mounts.push(HostMount::new(
            node.clone(),
            PathBuf::from("/dev/kvm"),
            HostMountKind::CharacterDevice,
        )?);
        shell.kvm = true;
    }

    if vendors.is_empty() {
        return Ok(());
    }
    for vendor in &vendors {
        let nodes = match vendor {
            GpuVendor::Amd => &discovered.amd,
            GpuVendor::Nvidia => &discovered.nvidia,
        };
        if nodes.is_empty() {
            bail!(
                "--allow-gpu={} requested, but no matching GPU devices were found; check driver loading and /dev access",
                vendor.cli_name()
            );
        }
        for path in nodes {
            let dest = path.to_path_buf();
            shell.mounts.push(HostMount::new(
                path.clone(),
                dest,
                HostMountKind::CharacterDevice,
            )?);
        }
    }

    // Read-only sysfs is required by ROCm/KFD topology discovery and Vulkan
    // ICD / PCI enumeration. It never gives access to /dev device ioctls.
    if driver_dirs.sys.is_dir() {
        shell.mounts.push(HostMount::new(
            driver_dirs.sys.to_path_buf(),
            PathBuf::from("/sys"),
            HostMountKind::Directory,
        )?);
    }
    for source in driver_dirs.graphics {
        let source = Path::new(source);
        if source.is_dir() {
            shell.mounts.push(HostMount::new(
                source.to_path_buf(),
                source.to_path_buf(),
                HostMountKind::Directory,
            )?);
        }
    }
    if vendors.contains(&GpuVendor::Nvidia) && driver_dirs.nvidia_proc.is_dir() {
        shell.mounts.push(HostMount::new(
            driver_dirs.nvidia_proc.to_path_buf(),
            PathBuf::from("/proc/driver/nvidia"),
            HostMountKind::Directory,
        )?);
    }

    // NixOS GPU driver libraries are typically provided here, not in the
    // nix-shell package closure. Avoid setting this on machines that do not
    // expose the driver location; other distributions use their own runtime.
    if Path::new("/run/opengl-driver/lib").is_dir() {
        shell
            .environment
            .push(("LD_LIBRARY_PATH".into(), "/run/opengl-driver/lib".into()));
    }
    shell.gpu = vendors.into_iter().collect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn only_vendor_verified_drm_devices_are_discovered() {
        let temp = tempfile::tempdir().unwrap();
        let dev = temp.path().join("dev");
        let sys = temp.path().join("sys/class/drm");
        fs::create_dir_all(dev.join("dri")).unwrap();
        let names = [
            ("renderD128", AMD_VENDOR),
            ("renderD129", NVIDIA_VENDOR),
            ("renderD130", "0x8086"),
            ("card0", NVIDIA_VENDOR),
            ("card1", AMD_VENDOR),
        ];
        for (name, vendor) in names {
            fs::create_dir_all(sys.join(name).join("device")).unwrap();
            fs::write(sys.join(name).join("device/vendor"), vendor).unwrap();
            symlink("/dev/null", dev.join("dri").join(name)).unwrap();
        }
        symlink("/dev/null", dev.join("kvm")).unwrap();
        symlink("/dev/null", dev.join("kfd")).unwrap();
        symlink("/dev/null", dev.join("nvidiactl")).unwrap();
        symlink("/dev/null", dev.join("nvidia0")).unwrap();
        symlink("/dev/null", dev.join("nvidia-forbidden")).unwrap();
        fs::create_dir_all(dev.join("nvidia-caps")).unwrap();
        symlink("/dev/null", dev.join("nvidia-caps/nvidia-cap2")).unwrap();
        let found = discover_at(&Layout {
            dev: &dev,
            drm_sys: &sys,
        });
        assert!(found.kvm.is_some());
        assert_eq!(found.amd.len(), 3);
        assert_eq!(found.nvidia.len(), 5);
        assert!(
            !found
                .nvidia
                .iter()
                .any(|path| path.to_string_lossy().contains("forbidden"))
        );
        assert!(
            !found
                .amd
                .iter()
                .chain(found.nvidia.iter())
                .any(|path| path.to_string_lossy().contains("130"))
        );
        assert!(drm_node_name("renderD128"));
        assert!(!drm_node_name("renderDabc"));
        assert!(!nvidia_node_name("nvidia-secret"));
    }

    #[test]
    fn kfd_alone_is_not_an_amd_gpu() {
        let temp = tempfile::tempdir().unwrap();
        let dev = temp.path().join("dev");
        let sys = temp.path().join("sys/class/drm");
        fs::create_dir_all(&dev).unwrap();
        symlink("/dev/null", dev.join("kfd")).unwrap();
        let found = discover_at(&Layout {
            dev: &dev,
            drm_sys: &sys,
        });
        assert!(found.amd.is_empty());
    }

    #[test]
    fn unavailable_requested_hardware_is_rejected() {
        let mut shell = HostShell::default();
        let empty = Discovery::default();
        let dirs = DriverDirectories {
            graphics: &[],
            sys: Path::new("/nonexistent"),
            nvidia_proc: Path::new("/nonexistent"),
        };
        assert!(
            add_selected(&mut shell, true, &[], &empty, &dirs)
                .unwrap_err()
                .to_string()
                .contains("/dev/kvm")
        );
        assert!(
            add_selected(&mut shell, false, &[GpuVendor::Amd], &empty, &dirs)
                .unwrap_err()
                .to_string()
                .contains("amd")
        );
        assert!(shell.mounts.is_empty());
    }

    #[test]
    fn discovered_devices_are_pinned_and_vendor_selective() {
        let mut shell = HostShell::default();
        // Harmless character devices stand in for GPU nodes; unit tests
        // verify that only explicitly selected devices are mounted.
        let found = Discovery {
            kvm: Some(PathBuf::from("/dev/null")),
            amd: vec![PathBuf::from("/dev/null")],
            nvidia: vec![PathBuf::from("/dev/zero")],
        };
        let dirs = DriverDirectories {
            graphics: &[],
            sys: Path::new("/nonexistent"),
            nvidia_proc: Path::new("/nonexistent"),
        };
        add_selected(&mut shell, true, &[GpuVendor::Amd], &found, &dirs).unwrap();
        assert!(shell.kvm);
        assert_eq!(shell.gpu, vec![GpuVendor::Amd]);
        assert_eq!(shell.mounts.len(), 2);
        assert!(
            shell
                .mounts
                .iter()
                .all(|mount| mount.kind == HostMountKind::CharacterDevice)
        );
        assert!(
            !shell
                .mounts
                .iter()
                .any(|mount| mount.source == Path::new("/dev/zero"))
        );
    }
}
