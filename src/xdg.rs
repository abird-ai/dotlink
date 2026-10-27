//! XDG base-directory resolution shared by configuration, state, cache and
//! discovery code. An override is used only when it is non-empty and absolute;
//! a relative or empty value falls back to the user's home directory.

use std::{env, ffi::OsString, path::PathBuf};

use anyhow::{Result, anyhow};

fn absolute_override(value: Option<OsString>) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty() && path.is_absolute())
}

fn home_path(value: Option<OsString>) -> Result<PathBuf> {
    let home = value
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("HOME is not set"))?;
    if !home.is_absolute() {
        return Err(anyhow!("HOME must be an absolute path: {}", home.display()));
    }
    Ok(home)
}

pub fn home_dir() -> Result<PathBuf> {
    home_path(env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")))
}

fn from_env(variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(path) = absolute_override(env::var_os(variable)) {
        return Ok(path);
    }
    Ok(home_dir()?.join(fallback))
}

pub fn config_home() -> Result<PathBuf> {
    from_env("XDG_CONFIG_HOME", ".config")
}

pub fn cache_home() -> Result<PathBuf> {
    from_env("XDG_CACHE_HOME", ".cache")
}

pub fn state_home() -> Result<PathBuf> {
    if let Some(path) = absolute_override(env::var_os("XDG_STATE_HOME")) {
        return Ok(path);
    }
    #[cfg(windows)]
    if let Some(path) = absolute_override(env::var_os("LOCALAPPDATA")) {
        return Ok(path);
    }
    Ok(home_dir()?.join(".local/state"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// One absolute path this host actually accepts. The literal POSIX examples
    /// in these tests are not absolute on Windows, where only a drive-qualified
    /// path is.
    fn absolute(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn relative_and_empty_overrides_are_ignored() {
        assert_eq!(absolute_override(None), None);
        assert_eq!(absolute_override(Some(OsString::new())), None);
        assert_eq!(
            absolute_override(Some(OsString::from("relative-config"))),
            None
        );
        let absolute = absolute("xdg");
        assert_eq!(
            absolute_override(Some(absolute.clone().into_os_string())),
            Some(absolute)
        );
    }

    #[test]
    fn home_must_exist_and_be_absolute() {
        assert!(home_path(None).is_err());
        assert!(home_path(Some(OsString::new())).is_err());
        assert!(home_path(Some(OsString::from("relative-home"))).is_err());
        let home = absolute("user");
        assert_eq!(
            home_path(Some(home.clone().into_os_string())).unwrap(),
            Path::new(&home)
        );
    }
}
