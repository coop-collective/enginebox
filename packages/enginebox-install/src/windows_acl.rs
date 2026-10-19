//! Native ACL management accepts canonical verbatim Windows paths without a
//! PowerShell filesystem-provider translation. Every failure carries its API
//! operation, path and Windows error; no subprocess stderr is discarded.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::c_void,
    mem::size_of,
    os::windows::{ffi::OsStrExt, io::FromRawHandle, io::OwnedHandle},
    path::Path,
    ptr::{addr_of, null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE,
        Authorization::{
            EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT,
            SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
            TRUSTEE_W,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
        GetTokenInformation, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
        SE_DACL_PROTECTED, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::FILE_ALL_ACCESS,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // Allocations returned by ACL APIs must be released with LocalFree.
        unsafe { LocalFree(self.0) };
    }
}

fn status(code: u32, operation: &str, path: &Path) -> Result<()> {
    if code != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(code as i32))
            .with_context(|| format!("{operation} for {} (Windows error {code})", path.display()));
    }
    Ok(())
}
fn boolean(success: i32, operation: &str, path: &Path) -> Result<()> {
    if success == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("{operation} for {}", path.display()));
    }
    Ok(())
}

// usize storage provides the alignment needed by TOKEN_USER and owns its SID.
fn current_user(path: &Path) -> Result<Vec<usize>> {
    let mut handle = null_mut();
    // Only query access to this process's own token is requested.
    unsafe {
        boolean(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle),
            "OpenProcessToken(TOKEN_QUERY)",
            path,
        )?;
    }
    let _token = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut length = 0;
    unsafe { GetTokenInformation(handle, TokenUser, null_mut(), 0, &mut length) };
    ensure!(
        (size_of::<TOKEN_USER>() as u32..=65536).contains(&length),
        "GetTokenInformation(TokenUser) returned invalid size {length} for {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    let mut storage = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    unsafe {
        boolean(
            GetTokenInformation(
                handle,
                TokenUser,
                storage.as_mut_ptr().cast(),
                length,
                &mut length,
            ),
            "GetTokenInformation(TokenUser)",
            path,
        )?;
    }
    Ok(storage)
}
fn user_sid(storage: &[usize]) -> PSID {
    // The successful token query wrote TOKEN_USER into aligned, live storage.
    unsafe { (*storage.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}
fn wide_path(path: &Path) -> Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    ensure!(!wide.contains(&0), "NUL in Windows ACL path");
    wide.push(0);
    Ok(wide)
}

pub(crate) fn restrict_directory(path: &Path) -> Result<()> {
    let user = current_user(path)?;
    let sid = user_sid(&user);
    let path_wide = wide_path(path)?;
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.cast(),
        },
    };
    let mut dacl = null_mut();
    // A NULL old ACL creates a fresh ACL, rather than retaining explicit grants.
    unsafe {
        status(
            SetEntriesInAclW(1, &entry, null(), &mut dacl),
            "SetEntriesInAclW(current user only)",
            path,
        )?;
    }
    let _dacl = LocalAllocation(dacl.cast());
    ensure!(!dacl.is_null(), "SetEntriesInAclW returned a NULL DACL");
    unsafe {
        status(
            SetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                sid,
                null_mut(),
                dacl,
                null(),
            ),
            "SetNamedSecurityInfoW(protected owner-only DACL)",
            path,
        )?;
    }
    verify(path, &path_wide, sid)
}

fn verify(path: &Path, path_wide: &[u16], sid: PSID) -> Result<()> {
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    unsafe {
        status(
            GetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            ),
            "GetNamedSecurityInfoW(verify private directory)",
            path,
        )?;
    }
    let _descriptor = LocalAllocation(descriptor);
    ensure!(
        !descriptor.is_null() && !owner.is_null() && !dacl.is_null(),
        "missing private ACL for {}",
        path.display()
    );
    let mut control = 0;
    let mut revision = 0;
    unsafe {
        boolean(
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
            "GetSecurityDescriptorControl",
            path,
        )?;
        ensure!(
            control & SE_DACL_PROTECTED != 0,
            "private directory still inherits permissions: {}",
            path.display()
        );
        ensure!(
            EqualSid(owner, sid) != 0,
            "private directory owner differs from current user: {}",
            path.display()
        );
        ensure!(
            (*dacl).AceCount == 1,
            "private directory retained additional access rules: {}",
            path.display()
        );
        let mut raw_ace = null_mut();
        boolean(GetAce(dacl, 0, &mut raw_ace), "GetAce(private owner)", path)?;
        ensure!(!raw_ace.is_null(), "private ACL has no owner ACE");
        let ace = raw_ace.cast::<ACCESS_ALLOWED_ACE>();
        // ACCESS_ALLOWED_ACE_TYPE is 0 in the Win32 ACL representation.
        ensure!(
            (*ace).Header.AceType == 0
                && usize::from((*ace).Header.AceSize) >= size_of::<ACCESS_ALLOWED_ACE>(),
            "unexpected private ACL type for {}",
            path.display()
        );
        ensure!(
            (*ace).Mask == FILE_ALL_ACCESS
                && u32::from((*ace).Header.AceFlags) == SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            "private ACL permissions/inheritance mismatch for {}",
            path.display()
        );
        ensure!(
            EqualSid(addr_of!((*ace).SidStart).cast_mut().cast(), sid) != 0,
            "private ACL grants a different account access: {}",
            path.display()
        );
    }
    Ok(())
}
