//! Linux mount identity shared by launcher provenance and process observation.
use std::{fs, io, os::fd::AsRawFd};

/// Kernel mount ID of this open description (including bind/stacked mounts).
pub(crate) fn mount_id(file: &fs::File) -> io::Result<u64> {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|id| id.trim().parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid descriptor mount ID"))
}
