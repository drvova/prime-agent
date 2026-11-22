//! Windows job objects: the containment of one command's process tree.
//!
//! A command's shell is put in a job of its own before it runs anything, so
//! every process it starts is in the job too (breakaway is never allowed).
//! Ending the job ends them all, and so does closing its last handle: a
//! kernel that crashes takes its commands with it, since the system closes
//! its handles. Pinned Win32 constants and hand-declared externs, with no
//! windows-sys dependency (the policy of `pa_types::platform`).

#![allow(non_snake_case)] // The Win32 functions keep their documented names.

use std::ffi::c_void;

type Handle = *mut c_void;

/// `winnt.h`: every process in the job ends when its last handle closes.
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
/// `winnt.h` `JobObjectExtendedLimitInformation`.
const EXTENDED_LIMIT_INFORMATION: i32 = 9;

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

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

/// One kill-on-close job. Dropping it closes the handle, which ends
/// whatever is still in the job.
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
        let limits = ExtendedLimitInformation {
            basic: BasicLimitInformation {
                limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..BasicLimitInformation::default()
            },
            ..ExtendedLimitInformation::default()
        };
        // SAFETY: `limits` is a live, correctly laid out structure of the
        // length passed.
        let limited = unsafe {
            SetInformationJobObject(
                job.0,
                EXTENDED_LIMIT_INFORMATION,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of::<ExtendedLimitInformation>()).ok()?,
            )
        };
        (limited != 0).then_some(job)
    }

    /// Put the process behind `process` (a handle the caller keeps open) in
    /// the job. False when the system refuses.
    pub(super) fn assign(&self, process: std::os::windows::io::RawHandle) -> bool {
        // SAFETY: both handles are open for the duration of the call.
        unsafe { AssignProcessToJobObject(self.0, process) != 0 }
    }

    /// End every process in the job. False when the system refuses.
    pub(super) fn terminate(&self) -> bool {
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
