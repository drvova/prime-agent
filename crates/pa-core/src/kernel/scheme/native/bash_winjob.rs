//! Windows job objects: the containment of one command's process tree.
//!
//! A command's shell is put in a job of its own before it runs anything, so
//! every process it starts is in the job too (breakaway is never allowed).
//! Ending the job ends them all, and so does closing its last handle: a
//! kernel that crashes takes its commands with it, since the system closes
//! its handles. Pinned Win32 constants and hand-declared externs, with no
//! windows-sys dependency (the policy of `pa_types::platform`).
//!
//! One kind of process in the job is beyond this process's reach: an
//! elevated program started through UAC (`Start-Process -Verb RunAs`,
//! `ShellExecute`'s `runas` verb). Windows creates it from its `AppInfo` service
//! once the user has consented, as a child of the process that asked, so it
//! lands in that process's job; but it runs with the user's administrator
//! token, at a higher integrity level, and this process may not end it. Only
//! the job could, and the job is not a way around the user's consent: ending
//! the job spares such a process, and the job then stops ending it on close
//! (see [`JobObject::terminate`]). Everything else in the job is ended as
//! before. A job is not a boundary against the programs it holds: a program
//! of the same user can leave it on purpose in other ways (WMI's
//! `Win32_Process.Create` starts a process outside the caller's job).

#![allow(non_snake_case)] // The Win32 functions keep their documented names.

use std::collections::HashSet;
use std::ffi::c_void;

type Handle = *mut c_void;

/// `winnt.h`: every process in the job ends when its last handle closes.
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
/// `winnt.h` `JobObjectBasicProcessIdList`.
const BASIC_PROCESS_ID_LIST: i32 = 3;
/// `winnt.h` `JobObjectExtendedLimitInformation`.
const EXTENDED_LIMIT_INFORMATION: i32 = 9;
/// `winnt.h`: the right to end a process.
const PROCESS_TERMINATE: u32 = 0x0001;
/// `winnt.h`: the right to ask which job a process is in, which Windows
/// grants on a process of the same user at a higher integrity level.
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
/// How many process ids one listing of a job holds. A job that cannot be
/// listed whole is ended whole.
const LISTED: usize = 1024;
/// How many times a job is listed while it is being ended, for processes
/// started in the meantime. A job still starting processes after that is
/// ended whole.
const SWEEPS: usize = 16;

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION`.
#[repr(C)]
#[derive(Default)]
struct BasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` (its `IO_COUNTERS` are six
/// unsigned 64-bit counts).
#[repr(C)]
#[derive(Default)]
struct ExtendedLimitInformation {
    basic: BasicLimitInformation,
    io_counters: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

/// `JOBOBJECT_BASIC_PROCESS_ID_LIST` with room for [`LISTED`] ids.
#[repr(C)]
struct ProcessIdList {
    assigned: u32,
    listed: u32,
    ids: [usize; LISTED],
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
    fn QueryInformationJobObject(
        job: Handle,
        class: i32,
        info: *mut c_void,
        len: u32,
        returned: *mut u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    // The id is a `DWORD`; declared as the crate's other declarations of
    // `OpenProcess` declare it (`crate::platform::process`), since the
    // declarations of one symbol must agree.
    fn OpenProcess(access: u32, inherit: i32, process_id: usize) -> Handle;
    fn TerminateProcess(process: Handle, exit_code: u32) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

/// A process handle opened here, closed on drop.
struct OpenedProcess(Handle);

impl OpenedProcess {
    /// Process `id` opened with `access`, or `None` when it is gone or the
    /// system refuses that access.
    fn open(id: usize, access: u32) -> Option<Self> {
        // SAFETY: plain call; a null result is checked.
        let handle = unsafe { OpenProcess(access, 0, id) };
        (!handle.is_null()).then_some(OpenedProcess(handle))
    }
}

impl Drop for OpenedProcess {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `OpenProcess` and is closed
        // exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// One kill-on-close job. Dropping it closes the handle, which ends
/// whatever is still in the job (but see [`JobObject::terminate`]).
pub(super) struct JobObject(Handle);

// SAFETY: a job handle is a process-wide kernel object reference; the Win32
// calls made on it here are safe from any thread.
unsafe impl Send for JobObject {}
// SAFETY: as above; no call here mutates state behind the handle unsynchronized.
unsafe impl Sync for JobObject {}

impl JobObject {
    /// A new, empty kill-on-close job, or `None` when the system refuses one.
    pub(super) fn create() -> Option<Self> {
        // SAFETY: null attributes and a null name ask for an unnamed job with
        // default security.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        // Owned from here, so a failed limit below still closes the handle.
        let job = JobObject(handle);
        job.limit(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE).then_some(job)
    }

    /// Set the job's limit flags to `flags` (and every other limit to none).
    /// False when the system refuses.
    fn limit(&self, flags: u32) -> bool {
        let limits = ExtendedLimitInformation {
            basic: BasicLimitInformation {
                limit_flags: flags,
                ..BasicLimitInformation::default()
            },
            ..ExtendedLimitInformation::default()
        };
        let Ok(len) = u32::try_from(std::mem::size_of::<ExtendedLimitInformation>()) else {
            return false;
        };
        // SAFETY: `limits` is a live, correctly laid out structure of the
        // length passed.
        unsafe {
            SetInformationJobObject(
                self.0,
                EXTENDED_LIMIT_INFORMATION,
                (&raw const limits).cast(),
                len,
            ) != 0
        }
    }

    /// Whether `process` is in this job (or a job nested in it).
    fn holds(&self, process: &OpenedProcess) -> bool {
        let mut held = 0;
        // SAFETY: both handles are open, and `held` is a live int.
        let asked = unsafe { IsProcessInJob(process.0, self.0, &raw mut held) };
        asked != 0 && held != 0
    }

    /// Put the process behind `process` (a handle the caller keeps open) in
    /// the job. False when the system refuses.
    pub(super) fn assign(&self, process: std::os::windows::io::RawHandle) -> bool {
        // SAFETY: both handles are open for the duration of the call.
        unsafe { AssignProcessToJobObject(self.0, process) != 0 }
    }

    /// End every process in the job that this process may end. A process it
    /// may not end (an elevated program the user consented to through UAC)
    /// is spared, and the job stops ending its processes on close, so that
    /// closing it spares that process too; everything else is ended before
    /// that. When nothing in the job is beyond reach, or the job cannot be
    /// listed or keeps starting processes, the whole job is ended at once.
    /// True when that was done; false when the system refused.
    pub(super) fn terminate(&self) -> bool {
        // Ids already ended here: a process can still be listed while it ends.
        let mut ended = HashSet::new();
        let mut beyond_reach = false;
        // A process this one may end that would not end: only the whole
        // job's end is sure to take it.
        let mut refused = false;
        let mut settled = false;
        for _ in 0..SWEEPS {
            let mut list = Box::new(ProcessIdList {
                assigned: 0,
                listed: 0,
                ids: [0; LISTED],
            });
            let Ok(len) = u32::try_from(std::mem::size_of::<ProcessIdList>()) else {
                break;
            };
            // SAFETY: `list` is a live, correctly laid out structure of the
            // length passed.
            let listed = unsafe {
                QueryInformationJobObject(
                    self.0,
                    BASIC_PROCESS_ID_LIST,
                    (&raw mut *list).cast(),
                    len,
                    std::ptr::null_mut(),
                )
            };
            if listed == 0 || list.assigned > list.listed {
                break;
            }
            beyond_reach = false;
            let mut ending = false;
            let count = usize::try_from(list.listed).map_or(LISTED, |count| count.min(LISTED));
            for &id in &list.ids[..count] {
                if ended.contains(&id) {
                    continue;
                }
                // Each handle keeps its process, so the id cannot name
                // another process while it is checked against the job.
                if let Some(process) =
                    OpenedProcess::open(id, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION)
                {
                    if self.holds(&process) {
                        // SAFETY: the handle is open with the right to end it.
                        if unsafe { TerminateProcess(process.0, 1) } == 0 {
                            refused = true;
                        } else {
                            ended.insert(id);
                            ending = true;
                        }
                    }
                } else if let Some(process) =
                    OpenedProcess::open(id, PROCESS_QUERY_LIMITED_INFORMATION)
                {
                    beyond_reach |= self.holds(&process);
                }
                // Otherwise it is gone, or not even visible: ending the job
                // whole takes it if it is still there.
            }
            if !ending {
                settled = true;
                break;
            }
        }
        if settled && beyond_reach && !refused {
            // Closing the job must not end what the user elevated either.
            return self.limit(0);
        }
        // SAFETY: the job handle is open until drop.
        unsafe { TerminateJobObject(self.0, 1) != 0 }
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `CreateJobObjectW` and is closed
        // exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;

    use super::*;

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn SetKernelObjectSecurity(
            handle: Handle,
            information: u32,
            descriptor: *mut c_void,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    }

    /// `winnt.h` `DACL_SECURITY_INFORMATION`.
    const DACL_SECURITY_INFORMATION: u32 = 4;
    /// `winbase.h` `WAIT_TIMEOUT`.
    const WAIT_TIMEOUT: u32 = 0x102;

    /// A program that runs far longer than the test (`ping` is on every
    /// Windows).
    fn long_running() -> std::process::Child {
        std::process::Command::new("ping")
            .args(["-n", "600", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Ending a command's job ends every program in it this process may end,
    /// and spares one it may not: an elevated program the user consented to
    /// through UAC, which Windows puts in the job of the program that asked.
    /// A program whose access list lets anyone only query and wait for it
    /// stands in for one here (a test cannot answer a consent prompt). The
    /// job then spares it on close too.
    #[test]
    fn ending_a_job_spares_a_program_beyond_this_processs_reach() {
        let job = JobObject::create().unwrap();
        let mut ours = long_running();
        let mut beyond = long_running();
        assert!(job.assign(ours.as_raw_handle()) && job.assign(beyond.as_raw_handle()));
        // Query (0x1000) and synchronize (0x100000) for everyone, nothing more.
        let text: Vec<u16> = "D:(A;;0x101000;;;WD)\0".encode_utf16().collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string and live out-pointers.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0);
        // SAFETY: `std`'s handle to the child carries every right, and the
        // descriptor is the one just converted, freed once after.
        let restricted = unsafe {
            let restricted = SetKernelObjectSecurity(
                beyond.as_raw_handle(),
                DACL_SECURITY_INFORMATION,
                descriptor,
            );
            LocalFree(descriptor);
            restricted
        };
        assert_ne!(restricted, 0);

        let terminated = job.terminate();
        drop(job);
        let ours_ended = ours.wait().unwrap().code();
        // Ending a process is asynchronous: `ours` has ended, and a process
        // the job ended or closed on with it would be gone within this bound.
        // SAFETY: the child's handle is open until `beyond` is dropped.
        let beyond_runs =
            unsafe { WaitForSingleObject(beyond.as_raw_handle(), 2_000) } == WAIT_TIMEOUT;
        // `std`'s handle predates the access list, so it still ends the program.
        beyond.kill().unwrap();
        assert_eq!((terminated, ours_ended, beyond_runs), (true, Some(1), true));
    }
}
