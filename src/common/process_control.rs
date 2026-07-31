//! Cross-platform pause/resume for external processes.
//!
//! * **Unix** – `nix::sys::signal::kill` with `SIGSTOP` / `SIGCONT`.
//! * **Windows** – `NtSuspendProcess` / `NtResumeProcess` loaded at runtime
//!   from `ntdll.dll` via the `windows` crate.

use std::fmt;
use std::io;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use sysinfo::System;

/// Configuration for memory-based process control.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Memory usage percentage that triggers a pause (e.g. 90.0).
    pub pause_threshold: f64,
    /// Memory usage percentage that triggers a resume (e.g. 70.0).
    pub resume_threshold: f64,
    /// How often to poll system memory.
    pub poll_interval: Duration,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            pause_threshold: 85.0,
            resume_threshold: 70.0,
            poll_interval: Duration::from_millis(750),
        }
    }
}

/// Monitors system memory and automatically pauses/resumes a subprocess.
///
/// The monitor owns a background thread that polls memory usage. When
/// memory crosses `pause_threshold` the subprocess is frozen; when it drops
/// below `resume_threshold` the subprocess is resumed.
///
/// The monitor automatically cleans up its thread when dropped or when
/// `stop()` is called. If the subprocess is paused when monitoring ends,
/// it is resumed first so it does not stay frozen.
#[derive(Debug)]
pub struct MemoryMonitor {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MemoryMonitor {
    /// Start monitoring the given child process.
    ///
    /// # Errors
    /// Returns an error if a `ProcessHandle` cannot be created for the
    /// child's PID (e.g. the process has already exited).
    pub fn new(child: &std::process::Child, config: MonitorConfig) -> Result<Self, ProcessError> {
        let pid = child.id();

        // Validate early so we fail fast instead of failing inside the thread.
        let _ = ProcessHandle::new(pid)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let thread = thread::spawn(move || {
            let proc = match ProcessHandle::new(pid) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[mon:{}] failed to open process handle: {}", pid, e);
                    return;
                }
            };

            let mut sys = System::new_all();
            let mut local_paused = false;

            while !shutdown_clone.load(Ordering::Relaxed) {
                sys.refresh_memory();

                let total = sys.total_memory() as f64;
                let percent = if total > 0.0 {
                    (sys.used_memory() as f64 / total) * 100.0
                } else {
                    0.0
                };

                if !local_paused && percent > config.pause_threshold {
                    match proc.pause() {
                        Ok(()) => {
                            local_paused = true;
                            println!("[mon:{}] memory {:.1}% → paused", pid, percent);
                        }
                        Err(e) => {
                            println!("[mon:{}] pause failed: {}", pid, e);
                        }
                    }
                } else if local_paused && percent < config.resume_threshold {
                    match proc.resume() {
                        Ok(()) => {
                            local_paused = false;
                            println!("[mon:{}] memory {:.1}% → resumed", pid, percent);
                        }
                        Err(e) => {
                            println!("[mon:{}] resume failed: {}", pid, e);
                        }
                    }
                }

                // Park with timeout instead of sleep. This lets Drop wake us
                // up immediately via unpark() rather than waiting out the interval.
                thread::park_timeout(config.poll_interval);
            }

            // Always resume on shutdown so the process doesn't stay frozen.
            if local_paused {
                let _ = proc.resume();
            }
        });

        Ok(MemoryMonitor {
            shutdown,
            thread: Some(thread),
        })
    }
}

impl Drop for MemoryMonitor {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark(); // wake the thread immediately
            let _ = t.join();
        }
    }
}

#[derive(Debug)]
pub enum ProcessError {
    InvalidPid(u32),
    Io(io::Error),
    #[cfg(unix)]
    Nix(nix::Error),
    #[cfg(windows)]
    Windows(windows::core::Error),
    #[cfg(windows)]
    NtStatus(i32),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessError::InvalidPid(pid) => write!(f, "invalid PID: {}", pid),
            ProcessError::Io(e) => write!(f, "io error: {}", e),
            #[cfg(unix)]
            ProcessError::Nix(e) => write!(f, "nix error: {}", e),
            #[cfg(windows)]
            ProcessError::Windows(e) => write!(f, "windows error: {}", e),
            #[cfg(windows)]
            ProcessError::NtStatus(s) => write!(f, "NTSTATUS: 0x{:08X}", s),
        }
    }
}

impl std::error::Error for ProcessError {}

#[derive(Debug)]
pub struct ProcessHandle {
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

impl ProcessHandle {
    pub fn new(pid: u32) -> Result<Self, ProcessError> {
        imp::open(pid)
    }

    pub fn pause(&self) -> Result<(), ProcessError> {
        imp::pause(self)
    }

    pub fn resume(&self) -> Result<(), ProcessError> {
        imp::resume(self)
    }
}

/* =================================================================== */
/*  Unix implementation (nix)                                           */
/* =================================================================== */
#[cfg(unix)]
mod imp {
    use super::{ProcessError, ProcessHandle};
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    pub fn open(pid: u32) -> Result<ProcessHandle, ProcessError> {
        if pid == 0 {
            return Err(ProcessError::InvalidPid(pid));
        }
        Ok(ProcessHandle { pid })
    }

    pub fn pause(h: &ProcessHandle) -> Result<(), ProcessError> {
        kill(Pid::from_raw(h.pid as i32), Signal::SIGSTOP).map_err(ProcessError::Nix)
    }

    pub fn resume(h: &ProcessHandle) -> Result<(), ProcessError> {
        kill(Pid::from_raw(h.pid as i32), Signal::SIGCONT).map_err(ProcessError::Nix)
    }
}

/* =================================================================== */
/*  Windows implementation (windows crate)                              */
/* =================================================================== */
#[cfg(windows)]
mod imp {
    use super::{ProcessError, ProcessHandle};
    use std::io; // <-- ADDED
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SUSPEND_RESUME};
    use windows::core::{PCSTR, s, w};

    type NtSuspendProcess = unsafe extern "system" fn(HANDLE) -> i32;
    type NtResumeProcess = unsafe extern "system" fn(HANDLE) -> i32;

    pub fn open(pid: u32) -> Result<ProcessHandle, ProcessError> {
        if pid == 0 {
            return Err(ProcessError::InvalidPid(pid));
        }
        unsafe {
            let handle =
                OpenProcess(PROCESS_SUSPEND_RESUME, false, pid).map_err(ProcessError::Windows)?;
            Ok(ProcessHandle { handle })
        }
    }

    pub fn pause(h: &ProcessHandle) -> Result<(), ProcessError> {
        call_ntdll(h.handle, s!("NtSuspendProcess"), true)
    }

    pub fn resume(h: &ProcessHandle) -> Result<(), ProcessError> {
        call_ntdll(h.handle, s!("NtResumeProcess"), false)
    }

    pub fn close(h: &mut ProcessHandle) {
        unsafe {
            let _ = CloseHandle(h.handle);
        }
    }

    fn call_ntdll(handle: HANDLE, name: PCSTR, is_suspend: bool) -> Result<(), ProcessError> {
        unsafe {
            let ntdll = GetModuleHandleW(w!("ntdll.dll")).map_err(ProcessError::Windows)?;

            let proc = GetProcAddress(ntdll, name).ok_or_else(|| {
                ProcessError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    "NtSuspendProcess/NtResumeProcess not found in ntdll.dll",
                ))
            })?;

            let status = if is_suspend {
                let f: NtSuspendProcess = std::mem::transmute(proc);
                f(handle)
            } else {
                let f: NtResumeProcess = std::mem::transmute(proc);
                f(handle)
            };

            if status != 0 {
                return Err(ProcessError::NtStatus(status));
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        imp::close(self);
    }
}
