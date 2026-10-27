use std::{
    env, fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

// systemd LoadCredential passes the credential path, never its plaintext value.
// Keep the same interface useful outside systemd as an explicit file override.
const MAX_CREDENTIAL_BYTES: u64 = crate::bounded::RUNTIME_KEY_MAX_BYTES;

/// A credential and both pathname forms that must be hidden from MCP tools.
///
/// `stable_target` is canonicalized and then checked against the descriptor
/// that supplied `secret`. This makes the secret and protected target one
/// result of one validated open instead of two raceable pathname lookups.
pub struct CredentialSource {
    secret: String,
    lexical_target: PathBuf,
    stable_target: PathBuf,
}

impl CredentialSource {
    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn extend_protected_paths(&self, paths: &mut Vec<PathBuf>) {
        paths.push(self.lexical_target.clone());
        paths.push(self.stable_target.clone());
    }
}

/// Read an optional credential from one validated regular-file descriptor.
pub fn read(path: &Path, label: &'static str) -> Result<Option<CredentialSource>> {
    let Some(mut file) = crate::bounded::open_regular_file(path, label)? else {
        return Ok(None);
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    if metadata.len() > MAX_CREDENTIAL_BYTES {
        bail!(
            "{label} {} must be a regular file of at most 16 KiB",
            path.display()
        );
    }

    let mut contents = String::new();
    (&mut file)
        .take(MAX_CREDENTIAL_BYTES.saturating_add(1))
        .read_to_string(&mut contents)
        .with_context(|| format!("failed to read {label} {}", path.display()))?;
    if contents.len() as u64 > MAX_CREDENTIAL_BYTES {
        bail!("{label} {} exceeds 16 KiB", path.display());
    }
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        bail!("{label} {} is empty", path.display());
    }

    // Resolve only after reading, while the descriptor is still live, and
    // prove that the resolved target identifies that descriptor. A symlink or
    // rename race therefore fails closed rather than protecting a different
    // file from the one whose bytes were consumed.
    let stable_target = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve {label} {}", path.display()))?;
    let Some(stable_file) = crate::bounded::open_regular_file(&stable_target, label)? else {
        bail!(
            "{label} changed while it was being read: {}",
            path.display()
        );
    };
    if !same_file_identity(&file, &stable_file)? {
        bail!(
            "{label} changed while it was being read: {}",
            path.display()
        );
    }

    Ok(Some(CredentialSource {
        secret: trimmed.to_owned(),
        lexical_target: path.to_path_buf(),
        stable_target,
    }))
}

pub fn from_env(variable: &str) -> Result<Option<CredentialSource>> {
    let Some(path) = env::var_os(variable) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        bail!("{variable} must point to an absolute credential-file path");
    }
    read(&path, "credential file")?.map_or_else(
        || {
            Err(anyhow::anyhow!(
                "credential file {} does not exist",
                path.display()
            ))
        },
        |source| Ok(Some(source)),
    )
}

/// Lexical environment paths are always masked without opening them. This
/// preserves dormant-secret behavior; an active source adds its verified
/// stable target with `CredentialSource::extend_protected_paths`.
pub fn protected_paths() -> Vec<PathBuf> {
    ["DOTLINK_API_KEY_FILE", "NGROK_AUTHTOKEN_FILE"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .collect()
}

#[cfg(unix)]
fn same_file_identity(left: &fs::File, right: &fs::File) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(windows)]
fn same_file_identity(left: &fs::File, right: &fs::File) -> Result<bool> {
    use std::{io, os::windows::io::AsRawHandle as _};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    fn identity(file: &fs::File) -> io::Result<(u32, u64)> {
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: the file owns a live handle and `info` is a correctly sized
        // writable output buffer for the duration of this call.
        let result = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful call initializes the complete output struct.
        let info = unsafe { info.assume_init() };
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Ok((info.dwVolumeSerialNumber, index))
    }

    Ok(identity(left)? == identity(right)?)
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &fs::File, right: &fs::File) -> Result<bool> {
    // Keep unsupported targets functional. Unix and Windows, the platforms on
    // which dotlink exposes native filesystem tools, use true descriptor IDs.
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_file_requires_nonempty_small_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("token");
        fs::write(&path, "  fixture-key\n").unwrap();
        assert_eq!(
            read(&path, "credential file").unwrap().unwrap().secret(),
            "fixture-key"
        );

        fs::write(&path, "   \n").unwrap();
        assert!(read(&path, "credential file").is_err());

        fs::write(&path, vec![b'x'; 16 * 1024 + 1]).unwrap();
        assert!(read(&path, "credential file").is_err());
        assert!(read(temp.path(), "credential file").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_protects_both_lexical_alias_and_descriptor_verified_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("real-token");
        let alias = temp.path().join("token-link");
        fs::write(&target, "fixture-key\n").unwrap();
        symlink(&target, &alias).unwrap();

        let source = read(&alias, "credential file").unwrap().unwrap();
        let mut protected = Vec::new();
        source.extend_protected_paths(&mut protected);
        assert_eq!(source.secret(), "fixture-key");
        assert!(protected.contains(&alias));
        assert!(protected.contains(&target.canonicalize().unwrap()));
    }
}
