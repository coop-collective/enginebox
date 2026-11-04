//! Windows primitives for agent slots and the editor account (contract §3):
//! user-scope DPAPI for the installer's account passwords, SID lookup, fork
//! directory ACLs, the private agent desktop, and `CreateProcessWithLogonW`
//! into a limited Job Object. Compiled only on Windows; the behaviour is
//! qualified on the engine machine (`assets/qualify-editor-account.ps1`).
use super::slots::{Credential, Limits};
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::{OsStr, c_void},
    fs::File,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
    sync::Mutex,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GENERIC_ALL, HANDLE,
        HANDLE_FLAG_INHERIT, LocalFree, SetHandleInformation,
    },
    Security::{
        ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
            GetSecurityInfo, NO_MULTIPLE_TRUSTEE, ProgressInvokeNever, SE_FILE_OBJECT,
            SE_WINDOW_OBJECT, SET_ACCESS, SetEntriesInAclW, SetSecurityInfo, TREE_SEC_INFO_RESET,
            TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W, TreeSetNamedSecurityInfoW,
        },
        Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
        DACL_SECURITY_INFORMATION, GetTokenInformation, InitializeSecurityDescriptor,
        LookupAccountNameW, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SID_NAME_USE, SUB_CONTAINERS_AND_OBJECTS_INHERIT, SetSecurityDescriptorDacl, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        GetFileInformationByHandle, READ_CONTROL,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
            JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
            JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
            JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
            JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
            JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_UI_RESTRICTIONS,
            JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicUIRestrictions, JobObjectCpuRateControlInformation,
            JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::CreatePipe,
        StationsAndDesktops::{CreateDesktopW, OpenWindowStationW},
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
            CreateProcessWithLogonW, GetCurrentProcess, GetExitCodeProcess, LOGON_WITH_PROFILE,
            OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
            STARTUPINFOW, TerminateProcess,
        },
    },
};

/// The node's private desktop for agent runs (on `WinSta0`).
pub const AGENT_DESKTOP: &str = "eb-agents";
/// `OWNER RIGHTS`: an ACE for it replaces the owner's implicit
/// `READ_CONTROL | WRITE_DAC`, so a slot cannot re-ACL files it created.
const OWNER_RIGHTS: &str = "S-1-3-4";
const DPAPI_ENTROPY: &[u8] = b"enginebox-accounts-v1";
const STILL_ACTIVE: u32 = 259;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}
fn last_error(what: &str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::last_os_error()).context(what.to_owned())
}
fn status(code: u32, what: &str) -> Result<()> {
    if code != ERROR_SUCCESS {
        return Err(
            anyhow::Error::new(std::io::Error::from_raw_os_error(code as i32))
                .context(what.to_owned()),
        );
    }
    Ok(())
}
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Hard links to a file (`nNumberOfLinks`), without following reparse points.
pub fn link_count(path: &Path) -> Option<u64> {
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(7)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    (ok != 0).then_some(u64::from(info.nNumberOfLinks))
}

// ---------------------------------------------------------------- DPAPI

fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr().cast_mut(),
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: null_mut(),
    };
    // User scope: only the node's own account can unprotect.
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &input,
                null(),
                &entropy,
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                &entropy,
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
    };
    if ok == 0 {
        return Err(last_error(if protect {
            "CryptProtectData"
        } else {
            "CryptUnprotectData"
        }));
    }
    let _free = Local(out.pbData.cast());
    Ok(unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec())
}
pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, true)
}
pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, false)
}

// ---------------------------------------------------------------- SIDs

/// A local account's SID string (`S-1-5-21-…`).
pub fn account_sid(user: &str) -> Result<String> {
    let name = wide(OsStr::new(user));
    let (mut sid_len, mut domain_len) = (0u32, 0u32);
    let mut kind: SID_NAME_USE = 0;
    unsafe {
        LookupAccountNameW(
            null(),
            name.as_ptr(),
            null_mut(),
            &mut sid_len,
            null_mut(),
            &mut domain_len,
            &mut kind,
        )
    };
    ensure!(
        sid_len > 0
            && std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_INSUFFICIENT_BUFFER as i32),
        "No local account named {user}"
    );
    let mut sid = vec![0u8; sid_len as usize];
    let mut domain = vec![0u16; domain_len as usize];
    let ok = unsafe {
        LookupAccountNameW(
            null(),
            name.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    ensure!(ok != 0, "LookupAccountNameW({user}) failed");
    sid_string(sid.as_mut_ptr().cast())
}
fn sid_string(sid: PSID) -> Result<String> {
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    let _free = Local(text.cast());
    let len = (0..).take_while(|i| unsafe { *text.add(*i) } != 0).count();
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, len)
    }))
}
/// A SID from its string form (freed on drop).
struct Sid(Local);
impl Sid {
    fn parse(s: &str) -> Result<Self> {
        let w = wide(OsStr::new(s));
        let mut sid = null_mut();
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 {
            return Err(last_error("ConvertStringSidToSidW"));
        }
        Ok(Self(Local(sid)))
    }
    fn ptr(&self) -> PSID {
        self.0.0
    }
}
/// The node's own user SID.
pub fn current_sid() -> Result<String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let _token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut len = 0;
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
    ensure!(
        (size_of::<TOKEN_USER>() as u32..=65536).contains(&len),
        "GetTokenInformation size"
    );
    let mut buf = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0
    {
        return Err(last_error("GetTokenInformation"));
    }
    sid_string(unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}

fn entry(sid: &Sid, mask: u32, mode: i32, inherit: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inherit,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.ptr().cast(),
        },
    }
}
fn new_acl(entries: &[EXPLICIT_ACCESS_W], old: *const ACL) -> Result<Local> {
    let mut acl = null_mut();
    status(
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), old, &mut acl) },
        "SetEntriesInAclW",
    )?;
    Ok(Local(acl.cast()))
}

const MODIFY: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// Replace the DACL of `dir` and everything below it.
fn set_tree(dir: &Path, entries: &[EXPLICIT_ACCESS_W], owner: Option<&Sid>) -> Result<()> {
    let acl = new_acl(entries, null())?;
    let path = wide(dir.as_os_str());
    let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    if owner.is_some() {
        info |= OWNER_SECURITY_INFORMATION;
    }
    status(
        unsafe {
            TreeSetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                info,
                owner.map_or(null_mut(), Sid::ptr),
                null_mut(),
                acl.0.cast(),
                null(),
                TREE_SEC_INFO_RESET,
                None,
                ProgressInvokeNever,
                null(),
            )
        },
        "TreeSetNamedSecurityInfoW",
    )
    .with_context(|| format!("setting permissions on {}", dir.display()))
}

/// Node full control and the slot modify on `dir` (inherited below it).
pub fn grant_dir(dir: &Path, slot_sid: &str) -> Result<()> {
    let node = Sid::parse(&current_sid()?)?;
    let slot = Sid::parse(slot_sid)?;
    let owner_rights = Sid::parse(OWNER_RIGHTS)?;
    set_tree(
        dir,
        &[
            entry(
                &node,
                FILE_ALL_ACCESS,
                SET_ACCESS,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            ),
            entry(
                &slot,
                MODIFY,
                SET_ACCESS,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            ),
            entry(
                &owner_rights,
                READ_CONTROL,
                SET_ACCESS,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            ),
        ],
        None,
    )
}
/// Back to node-only, and the node owns every file again.
pub fn revoke_dir(dir: &Path) -> Result<()> {
    let node = Sid::parse(&current_sid()?)?;
    let owner_rights = Sid::parse(OWNER_RIGHTS)?;
    set_tree(
        dir,
        &[
            entry(
                &node,
                FILE_ALL_ACCESS,
                SET_ACCESS,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            ),
            entry(
                &owner_rights,
                READ_CONTROL,
                SET_ACCESS,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            ),
        ],
        Some(&node),
    )
}
/// Node full control plus another account's modify (the editor account on
/// the dev tree and its handshake directory).
pub fn share_dir(dir: &Path, sid: &str) -> Result<()> {
    grant_dir(dir, sid)
}
/// Node full control plus another account's read (e.g. the bridge token).
pub fn share_file_read(file: &Path, sid: &str) -> Result<()> {
    let node = Sid::parse(&current_sid()?)?;
    let other = Sid::parse(sid)?;
    set_tree(
        file,
        &[
            entry(&node, FILE_ALL_ACCESS, SET_ACCESS, NO_INHERITANCE),
            entry(&other, FILE_GENERIC_READ, SET_ACCESS, NO_INHERITANCE),
        ],
        None,
    )
}

// ---------------------------------------------------------------- desktop

/// `HDESK` of the agent desktop, kept open for the node's lifetime.
static DESKTOP: Mutex<Option<usize>> = Mutex::new(None);

/// Create `WinSta0\eb-agents` (once) with access for the node and `slots`,
/// and let the slots use `WinSta0` itself.
pub fn ensure_agent_desktop(slot_sids: &[String]) -> Result<()> {
    let mut held = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
    if held.is_some() {
        return Ok(());
    }
    let sids: Vec<Sid> = std::iter::once(current_sid())
        .chain(slot_sids.iter().cloned().map(Ok))
        .map(|s| s.and_then(|s| Sid::parse(&s)))
        .collect::<Result<_>>()?;
    let entries: Vec<_> = sids
        .iter()
        .map(|s| entry(s, GENERIC_ALL, SET_ACCESS, NO_INHERITANCE))
        .collect();
    let acl = new_acl(&entries, null())?;
    let mut sd: SECURITY_DESCRIPTOR = unsafe { zeroed() };
    unsafe {
        ensure!(
            InitializeSecurityDescriptor((&mut sd as *mut SECURITY_DESCRIPTOR).cast(), 1) != 0,
            "InitializeSecurityDescriptor"
        );
        ensure!(
            SetSecurityDescriptorDacl(
                (&mut sd as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                acl.0.cast(),
                0
            ) != 0,
            "SetSecurityDescriptorDacl"
        );
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut sd as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    let name = wide(OsStr::new(AGENT_DESKTOP));
    let desk = unsafe { CreateDesktopW(name.as_ptr(), null(), null(), 0, GENERIC_ALL, &sa) };
    if desk.is_null() {
        return Err(last_error("CreateDesktopW(eb-agents)"));
    }
    // Slots need to open the window station to reach their desktop.
    let winsta_name = wide(OsStr::new("WinSta0"));
    // READ_CONTROL | WRITE_DAC.
    let winsta = unsafe { OpenWindowStationW(winsta_name.as_ptr(), 0, 0x0002_0000 | 0x0004_0000) };
    if winsta.is_null() {
        return Err(last_error("OpenWindowStationW(WinSta0)"));
    }
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    status(
        unsafe {
            GetSecurityInfo(
                winsta,
                SE_WINDOW_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        },
        "GetSecurityInfo(WinSta0)",
    )?;
    let _descriptor = Local(descriptor);
    // WINSTA_ENUMDESKTOPS | READATTRIBUTES | ACCESSGLOBALATOMS | ENUMERATE | READ_CONTROL.
    let winsta_rights = 0x0001 | 0x0002 | 0x0020 | 0x0100 | 0x0002_0000;
    let grants: Vec<_> = sids[1..]
        .iter()
        .map(|s| entry(s, winsta_rights, GRANT_ACCESS, NO_INHERITANCE))
        .collect();
    if !grants.is_empty() {
        let merged = new_acl(&grants, dacl)?;
        status(
            unsafe {
                SetSecurityInfo(
                    winsta,
                    SE_WINDOW_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    merged.0.cast(),
                    null(),
                )
            },
            "SetSecurityInfo(WinSta0)",
        )?;
    }
    *held = Some(desk as usize);
    Ok(())
}

// ---------------------------------------------------------------- processes

/// One `CreateProcessWithLogonW` launch.
pub struct LogonSpec<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    /// `LOGON_WITH_PROFILE` (the editor account; never for slots).
    pub profile: bool,
    /// A desktop on `WinSta0`; `None`: the node's own (`WinSta0\Default`).
    pub desktop: Option<&'a str>,
    /// Put the process in a new limited, UI-restricted Job Object.
    pub limits: Option<Limits>,
    /// Pipe stdin, stdout and stderr to the node.
    pub pipes: bool,
}

/// A process started as another account.
pub struct LogonChild {
    pub pid: u32,
    process: OwnedHandle,
    job: Option<OwnedHandle>,
    pipes: Mutex<Option<(File, File, File)>>,
}
impl LogonChild {
    pub fn take_pipes(&self) -> Result<(File, File, File)> {
        self.pipes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .context("no pipes")
    }
    pub fn process_handle(&self) -> HANDLE {
        self.process.as_raw_handle() as HANDLE
    }
    /// End the process (its whole job when it has one).
    pub fn terminate(&self) {
        unsafe {
            match &self.job {
                Some(job) => TerminateJobObject(job.as_raw_handle() as HANDLE, 1),
                None => TerminateProcess(self.process_handle(), 1),
            };
        }
    }
    pub fn exit_code(&self) -> Option<u32> {
        let mut code = 0;
        let ok = unsafe { GetExitCodeProcess(self.process_handle(), &mut code) };
        (ok != 0 && code != STILL_ACTIVE).then_some(code)
    }
}

/// Quote one argument for the MSVC runtime's command-line parser.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// A Unicode environment block, sorted case-insensitively as Windows wants.
fn env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<&(String, String)> = env.iter().collect();
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn limited_job(limits: &Limits) -> Result<OwnedHandle> {
    unsafe {
        let job = CreateJobObjectW(null(), null());
        ensure!(!job.is_null(), "CreateJobObjectW");
        let job = OwnedHandle::from_raw_handle(job);
        let h = job.as_raw_handle() as HANDLE;
        let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        // No JOB_OBJECT_LIMIT_BREAKAWAY_OK: nothing leaves the job.
        ext.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        ext.BasicLimitInformation.ActiveProcessLimit = limits.processes;
        ext.JobMemoryLimit = limits.memory_bytes as usize;
        ensure!(
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&ext as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0,
            "SetInformationJobObject(limits): {}",
            std::io::Error::last_os_error()
        );
        let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = zeroed();
        cpu.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        cpu.Anonymous.CpuRate = limits.cpu_percent.clamp(1, 100) * 100;
        ensure!(
            SetInformationJobObject(
                h,
                JobObjectCpuRateControlInformation,
                (&cpu as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            ) != 0,
            "SetInformationJobObject(cpu): {}",
            std::io::Error::last_os_error()
        );
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS,
        };
        ensure!(
            SetInformationJobObject(
                h,
                JobObjectBasicUIRestrictions,
                (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            ) != 0,
            "SetInformationJobObject(ui): {}",
            std::io::Error::last_os_error()
        );
        Ok(job)
    }
}

fn pipe() -> Result<(HANDLE, HANDLE)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w) = (null_mut(), null_mut());
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
        return Err(last_error("CreatePipe"));
    }
    Ok((r, w))
}

/// Start `spec.program` as `cred`'s account, suspended; put it in its job
/// (or `adopt_into`), then resume it. The password goes only to seclogon.
pub fn spawn_logon(cred: &Credential, spec: &LogonSpec) -> Result<LogonChild> {
    spawn_logon_into(cred, spec, None)
}
pub fn spawn_logon_into(
    cred: &Credential,
    spec: &LogonSpec,
    adopt_into: Option<HANDLE>,
) -> Result<LogonChild> {
    if let Some(desktop) = spec.desktop {
        ensure!(desktop == AGENT_DESKTOP, "unknown desktop");
    }
    let user = wide(OsStr::new(&cred.user));
    let domain = wide(OsStr::new("."));
    let mut password = wide(OsStr::new(&cred.password));
    let app = wide(spec.program.as_os_str());
    let mut line: Vec<u16> = wide(OsStr::new(
        &std::iter::once(quote_arg(&spec.program.to_string_lossy()))
            .chain(spec.args.iter().map(|a| quote_arg(a)))
            .collect::<Vec<_>>()
            .join(" "),
    ));
    let env = env_block(spec.env);
    let cwd = wide(spec.cwd.as_os_str());
    let mut desktop = spec
        .desktop
        .map(|d| wide(OsStr::new(&format!("WinSta0\\{d}"))));
    let job = spec.limits.as_ref().map(limited_job).transpose()?;
    // Parent ends are not inheritable; child ends are closed after launch.
    let mut child_ends: Vec<HANDLE> = Vec::new();
    let mut parent_ends: Vec<HANDLE> = Vec::new();
    let mut si: STARTUPINFOW = unsafe { zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    if let Some(d) = desktop.as_mut() {
        si.lpDesktop = d.as_mut_ptr();
    }
    if spec.pipes {
        let (in_r, in_w) = pipe()?;
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        for h in [in_w, out_r, err_r] {
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
        }
        si.dwFlags |= STARTF_USESTDHANDLES;
        si.hStdInput = in_r;
        si.hStdOutput = out_w;
        si.hStdError = err_w;
        child_ends.extend([in_r, out_w, err_w]);
        parent_ends.extend([in_w, out_r, err_r]);
    }
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    let mut flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT;
    if spec.pipes {
        flags |= CREATE_NO_WINDOW;
    }
    let ok = unsafe {
        CreateProcessWithLogonW(
            user.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            if spec.profile { LOGON_WITH_PROFILE } else { 0 },
            app.as_ptr(),
            line.as_mut_ptr(),
            flags,
            env.as_ptr().cast(),
            cwd.as_ptr(),
            &si,
            &mut pi,
        )
    };
    let error = std::io::Error::last_os_error();
    password.iter_mut().for_each(|c| *c = 0);
    for h in child_ends {
        unsafe { CloseHandle(h) };
    }
    if ok == 0 {
        for h in parent_ends {
            unsafe { CloseHandle(h) };
        }
        bail!("CreateProcessWithLogonW as {} failed: {error}", cred.user);
    }
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess) };
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread) };
    let assign = job
        .as_ref()
        .map(|j| j.as_raw_handle() as HANDLE)
        .or(adopt_into);
    if let Some(j) = assign
        && unsafe { AssignProcessToJobObject(j, pi.hProcess) } == 0
    {
        let error = std::io::Error::last_os_error();
        unsafe { TerminateProcess(pi.hProcess, 1) };
        for h in parent_ends {
            unsafe { CloseHandle(h) };
        }
        bail!(
            "Cannot hold the {} process in its Job Object: {error}",
            cred.user
        );
    }
    unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
    drop(thread);
    let pipes = if spec.pipes {
        let [i, o, e] = [parent_ends[0], parent_ends[1], parent_ends[2]]
            .map(|h| unsafe { File::from_raw_handle(h) });
        Some((i, o, e))
    } else {
        None
    };
    Ok(LogonChild {
        pid: pi.dwProcessId,
        process,
        job,
        pipes: Mutex::new(pipes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_for_msvcrt() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(
            quote_arg(r"C:\dir with space\"),
            "\"C:\\dir with space\\\\\""
        );
    }

    #[test]
    fn environment_blocks_are_sorted_and_terminated() {
        let block = env_block(&[("b".into(), "2".into()), ("A".into(), "1".into())]);
        let text = String::from_utf16_lossy(&block);
        assert_eq!(text, "A=1\0b=2\0\0");
    }

    /// Needs the engine machine's slot accounts (run on the box).
    #[test]
    #[ignore = "needs eb-agent-01 on the engine machine"]
    fn slot_account_sid_resolves() {
        assert!(account_sid("eb-agent-01").unwrap().starts_with("S-1-5-21-"));
    }

    /// Needs an interactive logon (SSH key logons have no DPAPI master key).
    #[test]
    #[ignore = "needs an interactive logon on the engine machine"]
    fn dpapi_round_trips_for_this_user() {
        let sealed = protect(b"secret").unwrap();
        assert_ne!(sealed, b"secret");
        assert_eq!(unprotect(&sealed).unwrap(), b"secret");
    }
}
