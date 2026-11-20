//! The same live-task check in the host process group and inside Bubblewrap's /proc.
//! Std-only: `build.rs` also compiles this into the sandbox observer without Cargo dependencies.
use std::{
    fs,
    io::{self, BufRead},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::Path,
};

fn state(path: &Path, group: Option<i32>) -> io::Result<Option<u8>> {
    let stat = fs::read(path.join("stat"))?;
    // comm is arbitrary bytes, including ')' and line breaks. Only its suffix
    // is kernel-generated ASCII: state, ppid, pgrp, ...
    let fields = stat
        .iter()
        .rposition(|b| *b == b')')
        .and_then(|i| stat.get(i + 2..));
    let mut fields = fields.unwrap_or_default().split(|&b| b == b' ');
    let (Some([state]), Some(pgrp)) = (fields.next(), fields.nth(1)) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed proc stat",
        ));
    };
    let pgrp: i32 = std::str::from_utf8(pgrp)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or(io::ErrorKind::InvalidData)?;
    Ok(group.is_none_or(|g| g == pgrp).then_some(*state))
}

// A vanishing PID/TID is expected. A missing stat/task entry for a still
// present process is an incomplete observation, not proof it exited.
fn vanished(path: &Path) -> bool {
    matches!(path.try_exists(), Ok(false))
}

fn live(path: &Path, group: Option<i32>) -> io::Result<bool> {
    match state(path, group)? {
        None => Ok(false),
        Some(b'X' | b'Z') => {
            // An exited leader can still have live threads.
            for thread in fs::read_dir(path.join("task"))? {
                let path = thread?.path();
                match state(&path, None) {
                    Ok(Some(s)) if s != b'X' && s != b'Z' => return Ok(true),
                    Err(e) if !vanished(&path) => return Err(e),
                    _ => (),
                }
            }
            Ok(false)
        }
        Some(_) => Ok(true),
    }
}

/// NStgid lists IDs from the procfs mount's PID namespace down to ours.
/// Comparing just the numbers from /proc/self and getpid can match by coincidence.
fn same_pid_namespace(proc: &Path, own: u32) -> io::Result<()> {
    let status = fs::read(proc.join("self/status"))?;
    let expected = own.to_string();
    let same = match status
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"NStgid:"))
    {
        Some(ids) => ids
            .split(u8::is_ascii_whitespace)
            .filter(|s| !s.is_empty())
            .eq([expected.as_bytes()]),
        // Older procfs implementations may lack NStgid. PID 1 identifies the
        // mounted namespace; compare kernel namespace objects, not PID spellings.
        None => {
            let init = fs::metadata(proc.join("1/ns/pid"))?;
            let me = fs::metadata(proc.join("self/ns/pid"))?;
            (init.dev(), init.ino()) == (me.dev(), me.ino())
        }
    };
    if same {
        Ok(())
    } else {
        Err(io::Error::other(
            "/proc does not describe the command PID namespace",
        ))
    }
}

/// Only the mount used for enumeration can prove a negative. Overmounted procfs
/// rows remain in mountinfo; neither their ordering nor their options matter.
fn unfiltered_proc_mount(mounts: impl BufRead, visible_id: u64) -> io::Result<bool> {
    let id = visible_id.to_string();
    for line in mounts.split(b'\n') {
        let line = line?;
        let mut fields = line.split(|&b| b == b' ');
        if fields.next() != Some(id.as_bytes()) {
            continue;
        }
        // mountinfo: id parent dev root mountpoint options [optional...] - type source superopts
        if fields.nth(2) != Some(&b"/"[..])
            || fields.next() != Some(&b"/proc"[..])
            || !fields.any(|s| s == b"-")
            || fields.next() != Some(&b"proc"[..])
        {
            return Ok(false);
        }
        let Some(options) = fields.nth(1) else {
            return Ok(false);
        };
        return Ok(!options.is_empty()
            && options.split(|&b| b == b',').all(|option| {
                option
                    .strip_prefix(b"hidepid=")
                    .is_none_or(|mode| matches!(mode, b"0" | b"off"))
            }));
    }
    Ok(false)
}

/// None = the dedicated namespace, after reaping the command. Its only
/// infrastructure processes are Bubblewrap's PID 1 and this observer.
pub fn has_live_member(group: Option<i32>) -> io::Result<bool> {
    scan(
        Path::new("/proc"),
        std::process::id(),
        group,
        |pid, group| {
            // Skip unrelated host processes before opening their stat files. The
            // stat read still verifies membership: a live process can switch groups.
            unsafe extern "C" {
                fn getpgid(pid: i32) -> i32;
            }
            // SAFETY: a query for a numeric PID; no pointers or side effects.
            let actual = unsafe { getpgid(pid) };
            actual < 0 || actual == group
        },
    )
}

fn scan(
    proc: &Path,
    own: u32,
    group: Option<i32>,
    candidate: impl Fn(i32, i32) -> bool,
) -> io::Result<bool> {
    // Use one opened root for the scan and its visibility evidence, even on a
    // stacked /proc. Only obtain its mount ID if a negative needs corroboration.
    let root = fs::File::open(proc)?;
    let root_path = format!("/proc/self/fd/{}", root.as_raw_fd());
    let proc = Path::new(&root_path);
    same_pid_namespace(proc, own)?;
    let mut incomplete = None;
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        // A direct group's waited leader is terminal but still reserves its PGID.
        if match group {
            Some(group) => pid == group || !candidate(pid, group),
            None => pid == 1 || pid as u32 == own,
        } {
            continue;
        }
        let path = entry.path();
        match live(&path, group) {
            Ok(true) => return Ok(true),
            Err(e) if !vanished(&path) => incomplete = Some(e),
            _ => (),
        }
    }
    if let Some(error) = incomplete {
        return Err(error);
    }
    let mounts = io::BufReader::new(fs::File::open(proc.join("self/mountinfo"))?);
    if unfiltered_proc_mount(mounts, crate::linux_procfs::mount_id(&root)?)? {
        Ok(false)
    } else {
        Err(io::Error::other("incomplete procfs process visibility"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_negative_scan_requires_the_right_namespace_and_full_pid_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let proc = dir.path();
        fs::create_dir(proc.join("self")).unwrap();
        let id = crate::linux_procfs::mount_id(&fs::File::open(proc).unwrap()).unwrap();
        let check = |group| scan(proc, 7, group, |_, _| true);
        for scope in [None, Some(42)] {
            fs::write(
                proc.join("self/mountinfo"),
                format!("{id} 7 0:1 / /proc rw - proc proc rw\n"),
            )
            .unwrap();
            for ids in ["", "NStgid: 9", "NStgid: 7 7"] {
                fs::write(proc.join("self/status"), ids).unwrap();
                assert!(check(scope).is_err());
            }
            fs::write(proc.join("self/status"), "NStgid:\t7\n").unwrap();
            assert!(!check(scope).unwrap());
            for mounts in [
                "",
                "{id} 7 0:1 / /proc rw - proc proc rw,hidepid=2",
                "{id} 7 0:1 / /proc rw - proc proc hidepid=ptraceable,rw",
            ] {
                fs::write(
                    proc.join("self/mountinfo"),
                    mounts.replace("{id}", &id.to_string()),
                )
                .unwrap();
                assert!(check(scope).is_err());
            }
            let member = proc.join("88");
            fs::create_dir(&member).unwrap();
            fs::write(member.join("stat"), b"88 (member) S 1 42 42").unwrap();
            assert!(
                check(scope).unwrap(),
                "positive evidence survives filtering"
            );
            fs::remove_file(member.join("stat")).unwrap();
            fs::remove_dir(member).unwrap();
        }
    }

    #[test]
    fn stacked_procfs_uses_the_open_mount_not_row_order() {
        let hidden = "91 7 0:8 / /proc rw - proc proc rw,hidepid=invisible";
        let clear = "14 91 0:9 / /proc rw shared:2 - proc proc rw";
        for rows in [format!("{hidden}\n{clear}"), format!("{clear}\n{hidden}")] {
            assert!(unfiltered_proc_mount(rows.as_bytes(), 14).unwrap());
            assert!(!unfiltered_proc_mount(rows.as_bytes(), 91).unwrap());
            assert!(!unfiltered_proc_mount(rows.as_bytes(), 99).unwrap());
        }
    }

    #[test]
    fn missing_nstgid_requires_matching_namespace_objects() {
        let dir = tempfile::tempdir().unwrap();
        let proc = dir.path();
        fs::create_dir_all(proc.join("self/ns")).unwrap();
        fs::create_dir_all(proc.join("1/ns")).unwrap();
        fs::write(proc.join("self/status"), "Name: test\n").unwrap();
        fs::write(proc.join("self/ns/pid"), "").unwrap();
        assert!(same_pid_namespace(proc, 7).is_err());
        fs::write(proc.join("1/ns/pid"), "").unwrap();
        assert!(same_pid_namespace(proc, 7).is_err());
        fs::remove_file(proc.join("1/ns/pid")).unwrap();
        fs::hard_link(proc.join("self/ns/pid"), proc.join("1/ns/pid")).unwrap();
        assert!(same_pid_namespace(proc, 7).is_ok());
        fs::write(proc.join("self/status"), "NStgid: 17 7\n").unwrap();
        assert!(same_pid_namespace(proc, 7).is_err());
    }

    #[test]
    fn live_threads_count_but_zombies_and_other_groups_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let pid = temp.path();
        let task = pid.join("task/43");
        fs::create_dir_all(&task).unwrap();
        // comm can contain parentheses, newlines and non-UTF-8 bytes.
        fs::write(pid.join("stat"), b"42 (x)\xff\n) Z 1 47 47").unwrap();
        fs::write(task.join("stat"), b"43 (worker) S 1 47 47").unwrap();
        assert!(live(pid, Some(47)).unwrap());
        assert!(!live(pid, Some(46)).unwrap());
        fs::write(task.join("stat"), b"43 (worker) Z 1 47 47").unwrap();
        assert!(!live(pid, None).unwrap());
        fs::write(pid.join("stat"), b"truncated").unwrap();
        assert!(live(pid, None).is_err());
    }
}
