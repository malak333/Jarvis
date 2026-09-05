//! Process-tree ownership for supervised developer validation.
//!
//! The Windows runner creates the command interpreter suspended, assigns it to a
//! kill-on-close Job, and only then resumes its primary thread. Keeping the Job
//! handle in [`ValidationChild`] makes runner exit or crash terminate the entire
//! validation tree. This is a developer-build lifecycle guarantee, not hostile
//! process containment or production execution evidence.

use anyhow::{Context, Result};
use std::{fs::File, path::Path, process::ExitStatus};

#[cfg(windows)]
use anyhow::bail;

#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
const TERMINATED_EXIT_CODE: u32 = 1;

#[derive(Debug, thiserror::Error)]
#[error("validation spawn cleanup could not be confirmed: {0}")]
pub struct CleanupUnconfirmed(pub String);

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::OsString,
        mem::{size_of, zeroed},
        os::windows::{
            ffi::{OsStrExt, OsStringExt},
            io::AsRawHandle,
            process::ExitStatusExt,
        },
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, GENERIC_READ, HANDLE,
            INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::SECURITY_ATTRIBUTES,
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
                JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject, TerminateJobObject,
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            SystemInformation::GetSystemDirectoryW,
            Threading::{
                CreateProcessW, GetCurrentProcess, GetExitCodeProcess, ResumeThread,
                TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, PROCESS_INFORMATION,
                STARTF_USESTDHANDLES, STARTUPINFOW,
            },
        },
    };

    const TERMINATION_WAIT: Duration = Duration::from_secs(30);

    struct OwnedHandle(HANDLE);

    // A Windows kernel handle can be used and closed from any thread. This type
    // uniquely owns its handle and exposes no operation that aliases ownership.
    unsafe impl Send for OwnedHandle {}

    impl OwnedHandle {
        fn new(handle: HANDLE, operation: &str) -> Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error()).context(operation.to_owned());
            }
            Ok(Self(handle))
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    pub struct ValidationChild {
        // Field order is deliberate: closing the Job first applies kill-on-close
        // before the root process handle is released.
        job: OwnedHandle,
        process: OwnedHandle,
    }

    impl ValidationChild {
        pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
            match unsafe { WaitForSingleObject(self.process.raw(), 0) } {
                WAIT_TIMEOUT => Ok(None),
                WAIT_OBJECT_0 => {
                    let mut exit_code = 0;
                    if unsafe { GetExitCodeProcess(self.process.raw(), &mut exit_code) } == 0 {
                        return Err(std::io::Error::last_os_error())
                            .context("query validation exit code");
                    }
                    Ok(Some(ExitStatus::from_raw(exit_code)))
                }
                WAIT_FAILED => {
                    Err(std::io::Error::last_os_error()).context("poll validation process")
                }
                result => bail!("unexpected validation wait result {result}"),
            }
        }

        pub async fn terminate(&mut self) -> Result<()> {
            if job_is_empty(self.job.raw())? {
                return match unsafe { WaitForSingleObject(self.process.raw(), 0) } {
                    WAIT_OBJECT_0 => Ok(()),
                    WAIT_TIMEOUT => bail!("validation Job is empty while its root is still active"),
                    WAIT_FAILED => Err(std::io::Error::last_os_error())
                        .context("wait for completed validation process"),
                    result => bail!("unexpected completed validation wait result {result}"),
                };
            }
            if unsafe { TerminateJobObject(self.job.raw(), TERMINATED_EXIT_CODE) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("terminate validation process tree");
            }

            let deadline = Instant::now() + TERMINATION_WAIT;
            loop {
                let root_exited = match unsafe { WaitForSingleObject(self.process.raw(), 0) } {
                    WAIT_OBJECT_0 => true,
                    WAIT_TIMEOUT => false,
                    WAIT_FAILED => {
                        return Err(std::io::Error::last_os_error())
                            .context("wait for terminated validation process")
                    }
                    result => bail!("unexpected validation termination wait result {result}"),
                };
                let job_empty = job_is_empty(self.job.raw())?;
                if root_exited && job_empty {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("validation process tree did not terminate within 30 seconds");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    pub fn spawn(validation: &str, project: &Path, log: File) -> Result<ValidationChild> {
        if validation.contains('\0') {
            bail!("validation command contains a NUL character");
        }

        let job = create_kill_on_close_job()?;
        let inherited_log = duplicate_inheritable(log.as_raw_handle() as HANDLE)
            .context("duplicate validation log handle")?;
        let inherited_stdin = open_inheritable_null()?;
        let application = system_cmd_path()?;
        let current_directory = command_working_directory(project)?;
        let mut command_line: Vec<u16> =
            OsString::from(format!("\"cmd.exe\" /d /s /c \"{validation}\""))
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

        let mut startup: STARTUPINFOW = unsafe { zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = inherited_stdin.raw();
        startup.hStdOutput = inherited_log.raw();
        startup.hStdError = inherited_log.raw();
        let mut information: PROCESS_INFORMATION = unsafe { zeroed() };

        if unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED,
                null(),
                current_directory.as_ptr(),
                &startup,
                &mut information,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("spawn suspended validation");
        }

        let process = OwnedHandle::new(information.hProcess, "own validation process handle")?;
        let thread = OwnedHandle::new(information.hThread, "own validation thread handle")?;

        if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
            let assignment_error = std::io::Error::last_os_error();
            terminate_suspended_process(process.raw())
                .map_err(|error| CleanupUnconfirmed(format!("{error:#}")))
                .with_context(|| {
                    format!("clean up validation after Job assignment failed: {assignment_error}")
                })?;
            return Err(assignment_error).context("assign suspended validation to Job");
        }

        let previous_suspend_count = unsafe { ResumeThread(thread.raw()) };
        if previous_suspend_count != 1 {
            let resume_error = if previous_suspend_count == u32::MAX {
                std::io::Error::last_os_error().to_string()
            } else {
                format!("unexpected previous suspend count {previous_suspend_count}")
            };
            terminate_assigned_process(job.raw(), process.raw())
                .map_err(|error| CleanupUnconfirmed(format!("{error:#}")))
                .with_context(|| {
                    format!("clean up validation after resume failed: {resume_error}")
                })?;
            bail!("resume assigned validation: {resume_error}");
        }

        drop(thread);
        Ok(ValidationChild { job, process })
    }

    fn create_kill_on_close_job() -> Result<OwnedHandle> {
        let job = OwnedHandle::new(
            unsafe { CreateJobObjectW(null(), null()) },
            "create validation Job",
        )?;
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("set validation Job kill-on-close policy");
        }
        Ok(job)
    }

    fn duplicate_inheritable(source: HANDLE) -> Result<OwnedHandle> {
        let current_process = unsafe { GetCurrentProcess() };
        let mut duplicate = null_mut();
        if unsafe {
            DuplicateHandle(
                current_process,
                source,
                current_process,
                &mut duplicate,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("duplicate inheritable handle");
        }
        OwnedHandle::new(duplicate, "own duplicated handle")
    }

    fn open_inheritable_null() -> Result<OwnedHandle> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let name: Vec<u16> = "NUL\0".encode_utf16().collect();
        OwnedHandle::new(
            unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    &attributes,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    null_mut(),
                )
            },
            "open validation NUL input",
        )
    }

    fn system_cmd_path() -> Result<Vec<u16>> {
        let mut buffer = vec![0u16; 260];
        loop {
            let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
            if length == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("resolve Windows system directory");
            }
            if (length as usize) < buffer.len() {
                buffer.truncate(length as usize);
                break;
            }
            buffer.resize(length as usize + 1, 0);
        }
        let mut path = OsString::from_wide(&buffer);
        path.push("\\cmd.exe");
        wide_nul(path.as_os_str()).context("encode Windows command interpreter path")
    }

    fn wide_nul(value: &std::ffi::OsStr) -> Result<Vec<u16>> {
        let mut encoded: Vec<u16> = value.encode_wide().collect();
        if encoded.contains(&0) {
            bail!("Windows path contains a NUL character");
        }
        encoded.push(0);
        Ok(encoded)
    }

    fn command_working_directory(project: &Path) -> Result<Vec<u16>> {
        let encoded: Vec<u16> = project.as_os_str().encode_wide().collect();
        let verbatim_disk_prefix: Vec<u16> = "\\\\?\\".encode_utf16().collect();
        let unc_prefix: Vec<u16> = "\\\\".encode_utf16().collect();
        let normalized = encoded
            .strip_prefix(verbatim_disk_prefix.as_slice())
            .unwrap_or(&encoded);
        if normalized.starts_with(unc_prefix.as_slice())
            || normalized.get(1) != Some(&(b':' as u16))
        {
            bail!("validation project must use a local Windows drive path");
        }
        if normalized.contains(&0) {
            bail!("Windows path contains a NUL character");
        }
        let mut current_directory = normalized.to_vec();
        current_directory.push(0);
        Ok(current_directory)
    }

    fn terminate_suspended_process(process: HANDLE) -> Result<()> {
        if unsafe { TerminateProcess(process, TERMINATED_EXIT_CODE) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("terminate unassigned suspended validation");
        }
        wait_for_process(process, TERMINATION_WAIT)
    }

    fn terminate_assigned_process(job: HANDLE, process: HANDLE) -> Result<()> {
        if unsafe { TerminateJobObject(job, TERMINATED_EXIT_CODE) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("terminate assigned suspended validation");
        }
        wait_for_process(process, TERMINATION_WAIT)?;
        let deadline = Instant::now() + TERMINATION_WAIT;
        while !job_is_empty(job)? {
            if Instant::now() >= deadline {
                bail!("assigned suspended validation Job did not become empty");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn wait_for_process(process: HANDLE, timeout: Duration) -> Result<()> {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        match unsafe { WaitForSingleObject(process, timeout_ms) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => bail!("timed out waiting for validation process termination"),
            WAIT_FAILED => Err(std::io::Error::last_os_error())
                .context("wait for validation process termination"),
            result => bail!("unexpected validation cleanup wait result {result}"),
        }
    }

    fn job_is_empty(job: HANDLE) -> Result<bool> {
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        if unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("query validation Job process count");
        }
        Ok(accounting.ActiveProcesses == 0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn invalid_process_query_cannot_report_confirmed_termination() {
            let root = tempfile::tempdir().unwrap();
            let log = File::create(root.path().join("validation.log")).unwrap();
            let mut child = spawn("ping -n 30 127.0.0.1 >NUL", root.path(), log).unwrap();
            // Force a real kernel query failure without altering the owned Job.
            let process = std::mem::replace(&mut child.process, OwnedHandle(null_mut()));
            assert!(child.try_wait().is_err());
            assert!(child.terminate().await.is_err());
            // The Job still supplies cleanup when observation cannot prove it.
            drop(child);
            assert_eq!(
                unsafe { WaitForSingleObject(process.raw(), 5000) },
                WAIT_OBJECT_0
            );
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{os::unix::process::CommandExt, process::Stdio, time::Duration};

    pub struct ValidationChild {
        child: tokio::process::Child,
        pid: u32,
    }

    impl ValidationChild {
        pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
            self.child.try_wait().context("poll validation process")
        }

        pub async fn terminate(&mut self) -> Result<()> {
            let process_group = i32::try_from(self.pid).context("validation PID exceeds i32")?;
            let kill_result = unsafe { libc::kill(-process_group, libc::SIGKILL) };
            let kill_error = (kill_result == -1).then(std::io::Error::last_os_error);
            if let Some(error) = kill_error {
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).context("terminate validation process group");
                }
            }
            tokio::time::timeout(Duration::from_secs(30), self.child.wait())
                .await
                .context("timed out waiting for terminated validation process")?
                .context("wait for terminated validation process")?;
            Ok(())
        }
    }

    pub fn spawn(validation: &str, project: &Path, log: File) -> Result<ValidationChild> {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", validation]);
        command.as_std_mut().process_group(0);
        let child = command
            .current_dir(project)
            .stdin(Stdio::null())
            .stdout(log.try_clone().context("clone validation log")?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("spawn validation")?;
        let pid = child.id().context("validation process has no ID")?;
        Ok(ValidationChild { child, pid })
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("developer validation processes require Windows or Unix");

pub use platform::spawn;
