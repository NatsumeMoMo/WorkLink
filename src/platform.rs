use anyhow::{Result, bail, ensure};
use std::{
    ffi::{OsStr, c_void},
    fs::File,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Cryptography::*, *},
    System::{Console::*, Environment::*, JobObjects::*, Pipes::*, RemoteDesktop::*, Threading::*},
};

pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn wincheck(value: i32) -> Result<()> {
    if value == 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub struct Handle(pub HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                CloseHandle(self.0);
            }
        }
    }
}

pub fn current_sid() -> Result<String> {
    unsafe {
        let mut token = null_mut();
        wincheck(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
        token_sid(token)
    }
}

fn token_sid(token: HANDLE) -> Result<String> {
    unsafe {
        let guard = Handle(token);
        sid_of_token(guard.0)
    }
}

unsafe fn sid_of_token(token: HANDLE) -> Result<String> {
    unsafe {
        let mut length = 0;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length);
        let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        wincheck(GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        ))?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text = null_mut();
        wincheck(ConvertSidToStringSidW(user.User.Sid, &mut text))?;
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(result)
    }
}

pub fn protect_directory(path: &Path, owner: &str) -> Result<()> {
    let status = std::process::Command::new("icacls.exe")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            &format!("*{owner}:(OI)(CI)F"),
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    ensure!(status.success(), "无法限制数据目录权限：{}", path.display());
    Ok(())
}

pub fn seal(bytes: &[u8]) -> Result<Vec<u8>> {
    dpapi(bytes, true)
}
pub fn unseal(bytes: &[u8]) -> Result<Vec<u8>> {
    dpapi(bytes, false)
}

fn dpapi(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len().try_into()?,
            pbData: bytes.as_ptr() as *mut u8,
        };
        let mut output: CRYPT_INTEGER_BLOB = zeroed();
        if encrypt {
            wincheck(CryptProtectData(
                &input,
                wide("WorkLink device identity").as_ptr(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            ))?;
        } else {
            wincheck(CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            ))?;
        }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(output.pbData.cast());
        Ok(result)
    }
}

pub struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    pub attrs: SECURITY_ATTRIBUTES,
}
impl PipeSecurity {
    pub fn new(owner: &str) -> Result<Self> {
        unsafe {
            let sddl = wide(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{owner})"));
            let mut descriptor = null_mut();
            wincheck(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            ))?;
            Ok(Self {
                descriptor,
                attrs: SECURITY_ATTRIBUTES {
                    nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
            })
        }
    }
}
impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub fn detach_console() {
    // 保留控制台子系统让 PowerShell 正确等待 CLI，图形界面启动后再释放自己的控制台。
    unsafe {
        FreeConsole();
    }
}

pub fn ensure_console() {
    unsafe {
        if GetConsoleWindow().is_null() && AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
    }
}

pub struct RawConsole {
    input: HANDLE,
    output: HANDLE,
    imode: u32,
    omode: u32,
}
impl RawConsole {
    pub fn new() -> Result<Self> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let (mut imode, mut omode) = (0, 0);
            wincheck(GetConsoleMode(input, &mut imode))?;
            wincheck(GetConsoleMode(output, &mut omode))?;
            wincheck(SetConsoleMode(
                input,
                (imode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT,
            ))?;
            wincheck(SetConsoleMode(
                output,
                omode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            ))?;
            Ok(Self {
                input,
                output,
                imode,
                omode,
            })
        }
    }
}
impl Drop for RawConsole {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.input, self.imode);
            SetConsoleMode(self.output, self.omode);
        }
    }
}

pub fn execution_token(owner: &str, system: bool) -> Result<Handle> {
    unsafe {
        let sid = current_sid()?;
        if system {
            ensure!(sid == "S-1-5-18", "SYSTEM 执行需要后台以 Windows 服务运行");
        }
        let mut raw = null_mut();
        if sid == "S-1-5-18" && !system {
            // 只借用配置所有者的登录令牌，避免把普通请求误当作 SYSTEM 执行。
            let mut sessions = null_mut();
            let mut count = 0;
            wincheck(WTSEnumerateSessionsW(
                null_mut(),
                0,
                1,
                &mut sessions,
                &mut count,
            ))?;
            for session in std::slice::from_raw_parts(sessions, count as usize) {
                let mut candidate = null_mut();
                if WTSQueryUserToken(session.SessionId, &mut candidate) != 0 {
                    let guard = Handle(candidate);
                    if sid_of_token(guard.0).is_ok_and(|s| s == owner) {
                        wincheck(DuplicateTokenEx(
                            guard.0,
                            TOKEN_ALL_ACCESS,
                            null(),
                            SecurityImpersonation,
                            TokenPrimary,
                            &mut raw,
                        ))?;
                        break;
                    }
                }
            }
            WTSFreeMemory(sessions.cast());
            ensure!(
                !raw.is_null(),
                "配置所有者尚未登录，无法取得普通用户令牌；可使用 SSH 或明确授权的 SYSTEM 请求"
            );
        } else {
            ensure!(system || sid == owner, "当前账户与设备配置所有者不一致");
            let mut original = null_mut();
            wincheck(OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut original,
            ))?;
            let original = Handle(original);
            wincheck(DuplicateTokenEx(
                original.0,
                TOKEN_ALL_ACCESS,
                null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut raw,
            ))?;
        }
        let mut token = Handle(raw);
        if !system {
            let mut elevation = 0i32;
            let mut size = 0;
            wincheck(GetTokenInformation(
                token.0,
                TokenElevationType,
                (&mut elevation as *mut i32).cast(),
                size_of::<i32>() as u32,
                &mut size,
            ))?;
            if elevation == TokenElevationTypeFull {
                let mut linked: TOKEN_LINKED_TOKEN = zeroed();
                wincheck(GetTokenInformation(
                    token.0,
                    TokenLinkedToken,
                    (&mut linked as *mut TOKEN_LINKED_TOKEN).cast(),
                    size_of::<TOKEN_LINKED_TOKEN>() as u32,
                    &mut size,
                ))?;
                token = Handle(linked.LinkedToken);
            }
        }
        Ok(token)
    }
}

pub fn as_user<T>(owner: &str, system: bool, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let token = execution_token(owner, system)?;
    unsafe {
        wincheck(ImpersonateLoggedOnUser(token.0))?;
    }
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            unsafe {
                if RevertToSelf() == 0 {
                    std::process::abort();
                }
            }
        }
    }
    let _guard = Revert;
    action()
}

pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut text = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            text.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            text.push_str(&"\\".repeat(slashes));
        }
        text.push(ch);
        slashes = 0;
    }
    text.push_str(&"\\".repeat(slashes * 2));
    text.push('"');
    text
}

struct Attributes {
    _memory: Vec<usize>,
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
}
impl Attributes {
    fn new() -> Result<Self> {
        unsafe {
            let mut size = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
            let mut memory = vec![0usize; size.div_ceil(size_of::<usize>())];
            let ptr = memory.as_mut_ptr().cast();
            wincheck(InitializeProcThreadAttributeList(ptr, 1, 0, &mut size))?;
            Ok(Self {
                _memory: memory,
                ptr,
            })
        }
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr);
        }
    }
}

pub struct Child {
    pub process: Handle,
    pub job: Handle,
    pub input: Option<File>,
    pub output: Option<File>,
    pub error: Option<File>,
    pub console: Option<PseudoConsole>,
}

pub struct PseudoConsole(pub HPCON);
unsafe impl Send for PseudoConsole {}
impl Drop for PseudoConsole {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.0);
        }
    }
}

impl Child {
    pub fn exit_code(&self) -> Result<Option<u32>> {
        unsafe {
            if WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT {
                return Ok(None);
            }
            let mut code = 0;
            wincheck(GetExitCodeProcess(self.process.0, &mut code))?;
            Ok(Some(code))
        }
    }
    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.job.0, 1);
        }
    }
}

fn pipe() -> Result<(File, File)> {
    unsafe {
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let (mut read, mut write) = (null_mut(), null_mut());
        wincheck(CreatePipe(&mut read, &mut write, &attrs, 0))?;
        Ok((File::from_raw_handle(read), File::from_raw_handle(write)))
    }
}

pub fn spawn_process(
    spec: &crate::protocol::JobSpec,
    owner: &str,
    pty: Option<(u16, u16)>,
) -> Result<Child> {
    ensure!(
        !spec.program.is_empty() && !spec.program.contains('\0'),
        "程序名无效"
    );
    ensure!(
        spec.args.iter().all(|s| !s.contains('\0')),
        "参数不能含空字符"
    );
    let token = execution_token(owner, spec.system)?;
    let (child_input, input) = pipe()?;
    let (output, child_output) = pipe()?;
    let (error, child_error) = pipe()?;
    let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    let attributes = Attributes::new()?;
    startup.lpAttributeList = attributes.ptr;
    let mut console = None;
    let inherited = [
        child_input.as_raw_handle(),
        child_output.as_raw_handle(),
        child_error.as_raw_handle(),
    ];
    unsafe {
        for file in [&input, &output, &error] {
            wincheck(SetHandleInformation(
                file.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                0,
            ))?;
        }
        if let Some((cols, rows)) = pty {
            // 后台常把标准流重定向到空设备；显式清空继承值，子进程才会使用 ConPTY 的控制台句柄。
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
            let mut hpc = 0;
            let result = CreatePseudoConsole(
                COORD {
                    X: cols.clamp(20, 500) as i16,
                    Y: rows.clamp(5, 200) as i16,
                },
                child_input.as_raw_handle(),
                child_output.as_raw_handle(),
                0,
                &mut hpc,
            );
            ensure!(result >= 0, "创建 ConPTY 失败：{result:#x}");
            console = Some(PseudoConsole(hpc));
            wincheck(UpdateProcThreadAttribute(
                attributes.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const c_void,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            ))?;
        } else {
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = inherited[0];
            startup.StartupInfo.hStdOutput = inherited[1];
            startup.StartupInfo.hStdError = inherited[2];
            // 显式限制继承句柄，避免并发任务持有彼此的管道而无法收到 EOF。
            wincheck(UpdateProcThreadAttribute(
                attributes.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_ptr().cast(),
                size_of_val(&inherited),
                null_mut(),
                null(),
            ))?;
        }
        if current_sid()? == "S-1-5-18" && !spec.system {
            let session = 0u32;
            wincheck(SetTokenInformation(
                token.0,
                TokenSessionId,
                (&session as *const u32).cast(),
                4,
            ))?;
        }
        let mut environment = null_mut();
        wincheck(CreateEnvironmentBlock(&mut environment, token.0, 0))?;
        let mut command = wide(
            std::iter::once(&spec.program)
                .chain(spec.args.iter())
                .map(|s| quote_arg(s))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let cwd = spec.cwd.as_ref().map(wide);
        let mut info: PROCESS_INFORMATION = zeroed();
        let flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_UNICODE_ENVIRONMENT
            | CREATE_SUSPENDED
            | if pty.is_none() { CREATE_NO_WINDOW } else { 0 };
        let launched = if current_sid()? == "S-1-5-18" {
            CreateProcessAsUserW(
                token.0,
                null(),
                command.as_mut_ptr(),
                null(),
                null(),
                i32::from(pty.is_none()),
                flags,
                environment,
                cwd.as_ref().map_or(null(), |s| s.as_ptr()),
                &startup.StartupInfo,
                &mut info,
            )
        } else {
            // 当前进程可能已提升，普通任务仍须通过受限令牌启动。
            let mut elevation = 0i32;
            let mut len = 0;
            let mut original = null_mut();
            wincheck(OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY,
                &mut original,
            ))?;
            let original = Handle(original);
            GetTokenInformation(
                original.0,
                TokenElevationType,
                (&mut elevation as *mut i32).cast(),
                4,
                &mut len,
            );
            if elevation == TokenElevationTypeFull && !spec.system {
                CreateProcessWithTokenW(
                    token.0,
                    0,
                    null(),
                    command.as_mut_ptr(),
                    flags,
                    environment,
                    cwd.as_ref().map_or(null(), |s| s.as_ptr()),
                    &startup.StartupInfo,
                    &mut info,
                )
            } else {
                CreateProcessW(
                    null(),
                    command.as_mut_ptr(),
                    null(),
                    null(),
                    i32::from(pty.is_none()),
                    flags,
                    environment,
                    cwd.as_ref().map_or(null(), |s| s.as_ptr()),
                    &startup.StartupInfo,
                    &mut info,
                )
            }
        };
        let launch_error = std::io::Error::last_os_error();
        DestroyEnvironmentBlock(environment);
        if launched == 0 {
            bail!("无法启动 {}：{}", spec.program, launch_error);
        }
        let process = Handle(info.hProcess);
        let thread = Handle(info.hThread);
        let job = Handle(CreateJobObjectW(null(), null()));
        if job.0.is_null() {
            TerminateProcess(process.0, 1);
            bail!("无法创建进程 Job Object");
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of_val(&limits) as u32,
        ) == 0
            || AssignProcessToJobObject(job.0, process.0) == 0
        {
            TerminateProcess(process.0, 1);
            bail!("无法限制任务进程树：{}", std::io::Error::last_os_error());
        }
        ensure!(ResumeThread(thread.0) != u32::MAX, "无法启动任务线程");
        drop(child_input);
        drop(child_output);
        drop(child_error);
        Ok(Child {
            process,
            job,
            input: Some(input),
            output: Some(output),
            error: if pty.is_none() { Some(error) } else { None },
            console,
        })
    }
}

pub fn absolute_file(path: &str) -> Result<std::path::PathBuf> {
    ensure!(
        !path.contains('\0') && !path.starts_with(['\\', '/']),
        "仅支持本机绝对路径"
    );
    ensure!(
        path.len() >= 3
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[1] == b':'
            && matches!(path.as_bytes()[2], b'\\' | b'/'),
        "路径应为 C:\\目录\\文件 形式"
    );
    ensure!(!path[2..].contains(':'), "不支持 NTFS 备用数据流路径");
    Ok(path.into())
}

pub fn open_terminal_program() -> String {
    format!(
        "{}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_arguments_and_paths() {
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert!(absolute_file("C:\\Temp\\a.txt").is_ok());
        assert!(absolute_file("C:relative").is_err());
        assert!(absolute_file("\\\\host\\share").is_err());
        assert!(absolute_file("C:\\a:secret").is_err());
    }
    #[test]
    fn private_material_roundtrip() {
        let sealed = seal(b"test secret").unwrap();
        assert_ne!(sealed, b"test secret");
        assert_eq!(unseal(&sealed).unwrap(), b"test secret");
    }
}
