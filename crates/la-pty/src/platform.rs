//! Platform-specific signal delivery.
//!
//! Unix uses `nix::sys::signal::kill` against the process group (the
//! child is spawned via `portable_pty`, which `setsid`s the slave —
//! pgid == pid). Windows uses `GenerateConsoleCtrlEvent` for Ctrl-C /
//! Ctrl-Break, falling back to `TerminateProcess` for hard kill.

use crate::{PtyError, Signal};

#[cfg(unix)]
pub(crate) fn send_signal(pid: u32, sig: Signal) -> Result<(), PtyError> {
    use nix::sys::signal::{killpg, Signal as NixSig};
    use nix::unistd::Pid;

    let nix_sig = match sig {
        Signal::Interrupt => NixSig::SIGINT,
        Signal::Terminate => NixSig::SIGTERM,
        Signal::Kill => NixSig::SIGKILL,
    };

    // Signal the whole pgrp so foreground children (shells spawning
    // subprocesses) also get the signal — matches what hitting Ctrl-C
    // on a real terminal would do.
    killpg(Pid::from_raw(pid as i32), nix_sig).map_err(|e| PtyError::Signal(e.to_string()))
}

#[cfg(windows)]
pub(crate) fn send_signal(pid: u32, sig: Signal) -> Result<(), PtyError> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::{
        GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    match sig {
        Signal::Interrupt => unsafe {
            // 0 = "all processes sharing the console" wouldn't isolate
            // the child; we pass the child's pid as the group id, which
            // requires the child to have been launched with
            // CREATE_NEW_PROCESS_GROUP (portable-pty does this by default
            // on Windows). See README.
            if GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid) == 0 {
                let err = std::io::Error::last_os_error();
                return Err(PtyError::Signal(format!("CTRL_C_EVENT: {}", err)));
            }
            Ok(())
        },
        Signal::Terminate => unsafe {
            if GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) == 0 {
                let err = std::io::Error::last_os_error();
                return Err(PtyError::Signal(format!("CTRL_BREAK_EVENT: {}", err)));
            }
            Ok(())
        },
        Signal::Kill => unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                let err = std::io::Error::last_os_error();
                return Err(PtyError::Signal(format!("OpenProcess: {}", err)));
            }
            let ok = TerminateProcess(handle, 1);
            CloseHandle(handle);
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                return Err(PtyError::Signal(format!("TerminateProcess: {}", err)));
            }
            Ok(())
        },
    }
}
