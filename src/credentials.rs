use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

// systemd LoadCredential passes the credential path, never its plaintext value.
// Keep the same interface useful outside systemd as an explicit file override.
const MAX_CREDENTIAL_BYTES: u64 = 16 * 1024;

pub fn read_file(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect credential file {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_CREDENTIAL_BYTES {
        bail!(
            "credential file {} must be a regular file of at most 16 KiB",
            path.display()
        );
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read credential file {}", path.display()))?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        bail!("credential file {} is empty", path.display());
    }
    Ok(trimmed.to_owned())
}

pub fn from_env(variable: &str) -> Result<Option<String>> {
    let Some(path) = env::var_os(variable) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        bail!("{variable} must point to an absolute credential-file path");
    }
    Ok(Some(read_file(&path)?))
}

pub fn protected_paths() -> Vec<PathBuf> {
    ["DOTLINK_API_KEY_FILE", "NGROK_AUTHTOKEN_FILE"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_file_requires_nonempty_small_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("token");
        fs::write(&path, "  fixture-key\n").unwrap();
        assert_eq!(read_file(&path).unwrap(), "fixture-key");

        fs::write(&path, "   \n").unwrap();
        assert!(read_file(&path).is_err());

        fs::write(&path, vec![b'x'; 16 * 1024 + 1]).unwrap();
        assert!(read_file(&path).is_err());
        assert!(read_file(temp.path()).is_err());
    }
}
