//! Private Windows files and directories: owner/SYSTEM/Administrators only.
//! Do not inherit a broad project-directory ACL for credentials or OAuth state.

use std::{
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr,
};

use anyhow::{Context, Result};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
        SetFileSecurityW, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

fn current_user_sid() -> Result<String> {
    let mut raw = ptr::null_mut();
    // SAFETY: current-process pseudohandle and a live output for the token.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error()).context("cannot open process token");
    }
    // SAFETY: OpenProcessToken returned an owned HANDLE.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    use std::os::windows::io::AsRawHandle;
    let mut needed = 0_u32;
    // SAFETY: first call queries the required size, returning no user data.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 || needed > 64 * 1024 {
        anyhow::bail!("cannot determine a bounded Windows token SID size");
    }
    // A usize buffer provides alignment for TOKEN_USER and its trailing SID.
    let mut buffer = vec![0_usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buffer is aligned and has at least the requested number of bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * size_of::<usize>()) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("cannot read Windows token SID");
    }
    // SAFETY: successful TokenUser returns a TOKEN_USER at the aligned start.
    let sid = unsafe { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let mut encoded = ptr::null_mut();
    // SAFETY: the SID is backed by the live buffer and encoded is writable.
    if unsafe { ConvertSidToStringSidW(sid, &mut encoded) } == 0 {
        return Err(io::Error::last_os_error()).context("cannot format Windows user SID");
    }
    let mut length = 0_usize;
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated allocated UTF-16 SID.
    unsafe {
        while *encoded.add(length) != 0 {
            length += 1;
        }
    }
    // SAFETY: length is the number of live UTF-16 words until the terminator.
    let value = String::from_utf16(unsafe { std::slice::from_raw_parts(encoded, length) });
    // SAFETY: the SID string was allocated using LocalAlloc.
    unsafe { LocalFree(encoded.cast()) };
    Ok(value.context("Windows user SID was invalid UTF-16")?)
}

pub(crate) fn protect(path: &Path, directory: bool) -> Result<()> {
    let sid = current_user_sid()?;
    // The protected DACL has no inherited ACEs; child files and directories
    // inherit the same narrow authority from a private directory.
    let inherit = if directory { "OICI" } else { "" };
    let sddl = format!("D:P(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)(A;{inherit};FA;;;{sid})");
    let wide = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let name = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut descriptor = ptr::null_mut();
    // SAFETY: SDDL is NUL-terminated and descriptor is a live output pointer.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("cannot build Windows private DACL");
    }
    // SAFETY: a valid security descriptor and NUL-terminated target pathname.
    let ok = unsafe {
        SetFileSecurityW(
            name.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    // SAFETY: descriptor was allocated by the Windows SDDL converter.
    unsafe { LocalFree(descriptor.cast()) };
    if ok == 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("cannot protect Windows private path {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{
        GetFileSecurityW, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
    };

    fn has_protected_acl(path: &Path) -> bool {
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut needed = 0;
        // SAFETY: first call requests the descriptor's required size.
        unsafe {
            GetFileSecurityW(
                wide.as_ptr(),
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        assert!(needed > 0 && needed <= 64 * 1024);
        let mut descriptor = vec![0_usize; (needed as usize).div_ceil(size_of::<usize>())];
        // SAFETY: aligned writable storage and a NUL-terminated pathname.
        assert_ne!(
            unsafe {
                GetFileSecurityW(
                    wide.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    descriptor.as_mut_ptr().cast(),
                    (descriptor.len() * size_of::<usize>()) as u32,
                    &mut needed,
                )
            },
            0
        );
        let mut control = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: successful GetFileSecurityW returned a descriptor.
        assert_ne!(
            unsafe {
                GetSecurityDescriptorControl(
                    descriptor.as_mut_ptr().cast(),
                    &mut control,
                    &mut revision,
                )
            },
            0
        );
        control & SE_DACL_PROTECTED != 0
    }

    #[test]
    fn private_state_and_directory_do_not_inherit_a_broad_acl() {
        let temp = tempfile::tempdir().unwrap();
        protect(temp.path(), true).unwrap();
        let file = temp.path().join("oauth.json");
        std::fs::write(&file, b"state").unwrap();
        protect(&file, false).unwrap();
        assert!(has_protected_acl(temp.path()));
        assert!(has_protected_acl(&file));
        assert_eq!(std::fs::read(file).unwrap(), b"state");
    }
}
