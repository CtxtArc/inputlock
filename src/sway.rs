//! Tiny sway IPC client plus a watchdog process that restores the previous
//! binding mode however inputlock exits (normal exit, panic, SIGKILL...).
//!
//! Why a binding mode at all: sway handles its own keybindings *before* any
//! client sees the key, so even with an exclusive-keyboard overlay, Mod+Enter,
//! Mod+Shift+q etc. would still work. Switching to an empty mode disables them.

use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const GET_BINDING_MODES: u32 = 8;
const GET_BINDING_STATE: u32 = 12;

#[derive(Clone)]
pub struct Sway {
    socket: PathBuf,
}

impl Sway {
    pub fn from_env() -> Option<Self> {
        std::env::var_os("SWAYSOCK")
            .or_else(|| std::env::var_os("I3SOCK"))
            .map(|s| Sway { socket: s.into() })
    }

    fn request(&self, ty: u32, payload: &str) -> io::Result<serde_json::Value> {
        let mut s = UnixStream::connect(&self.socket)?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.set_write_timeout(Some(Duration::from_secs(3)))?;

        let mut msg = Vec::with_capacity(14 + payload.len());
        msg.extend_from_slice(MAGIC);
        msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        msg.extend_from_slice(&ty.to_ne_bytes());
        msg.extend_from_slice(payload.as_bytes());
        s.write_all(&msg)?;

        let mut hdr = [0u8; 14];
        s.read_exact(&mut hdr)?;
        if &hdr[..6] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad IPC magic"));
        }
        let len = u32::from_ne_bytes(hdr[6..10].try_into().unwrap()) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body)?;
        serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn run(&self, cmd: &str) -> Result<(), String> {
        let v = self.request(RUN_COMMAND, cmd).map_err(|e| e.to_string())?;
        let results = v.as_array().ok_or("unexpected reply")?;
        for r in results {
            if r.get("success").and_then(|s| s.as_bool()) != Some(true) {
                let err = r.get("error").and_then(|e| e.as_str()).unwrap_or("command failed");
                return Err(err.to_string());
            }
        }
        Ok(())
    }

    pub fn set_mode(&self, mode: &str) -> Result<(), String> {
        self.run(&format!("mode \"{mode}\""))
    }

    pub fn current_mode(&self) -> io::Result<String> {
        let v = self.request(GET_BINDING_STATE, "")?;
        Ok(v.get("name").and_then(|n| n.as_str()).unwrap_or("default").to_string())
    }

    pub fn modes(&self) -> io::Result<Vec<String>> {
        let v = self.request(GET_BINDING_MODES, "")?;
        Ok(v.as_array()
            .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect())
            .unwrap_or_default())
    }
}

pub fn valid_mode_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['"', '\\', '\n', ';', ','])
}

/// Forks a watchdog child that blocks on a pipe. When the returned fd is
/// closed — which the kernel does for us when this process dies for *any*
/// reason — the child switches sway back to `restore_mode` and exits.
///
/// Must be called before any threads are spawned.
pub fn spawn_watchdog(sway: &Sway, lock_mode: &str, restore_mode: &str) -> io::Result<OwnedFd> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (rd, wr) = (fds[0], fds[1]);

    match unsafe { libc::fork() } {
        -1 => {
            unsafe {
                libc::close(rd);
                libc::close(wr);
            }
            Err(io::Error::last_os_error())
        }
        0 => {
            // Child.
            unsafe {
                libc::close(wr);
                // Own session: not hit by Ctrl+C / terminal hangup / `kill -- -pgid`.
                libc::setsid();
                for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGPIPE] {
                    libc::signal(sig, libc::SIG_IGN);
                }
                // Distinct comm name so `pkill inputlock` doesn't take it down too.
                libc::prctl(libc::PR_SET_NAME, c"inputlock-wd".as_ptr());
            }
            let mut b = [0u8; 1];
            loop {
                let n = unsafe { libc::read(rd, b.as_mut_ptr() as *mut _, 1) };
                if n == 0 {
                    break; // parent gone
                }
                if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            // Only restore if we're still in *our* mode — don't stomp on a
            // mode the user picked after unlocking.
            if sway.current_mode().map(|m| m == lock_mode).unwrap_or(true) {
                let _ = sway.set_mode(restore_mode);
            }
            unsafe { libc::_exit(0) };
        }
        _ => {
            unsafe { libc::close(rd) };
            Ok(unsafe { OwnedFd::from_raw_fd(wr) })
        }
    }
}

/// Switches sway into the empty lock mode and back.
pub struct ModeGuard {
    sway: Sway,
    lock_mode: String,
    restore_mode: String,
    engaged: bool,
    _watchdog: OwnedFd,
}

impl ModeGuard {
    pub fn new(sway: Sway, lock_mode: String, restore_mode: String, watchdog: OwnedFd) -> Self {
        ModeGuard { sway, lock_mode, restore_mode, engaged: false, _watchdog: watchdog }
    }

    pub fn engage(&mut self) -> Result<(), String> {
        if !self.engaged {
            self.sway.set_mode(&self.lock_mode)?;
            self.engaged = true;
        }
        Ok(())
    }

    pub fn release(&mut self) {
        if self.engaged {
            // Leave the user's mode alone if something else already changed it.
            if self.sway.current_mode().map(|m| m != self.lock_mode).unwrap_or(false) {
                self.engaged = false;
                return;
            }
            if let Err(e) = self.sway.set_mode(&self.restore_mode) {
                eprintln!("inputlock: failed to restore sway mode: {e}");
            }
            self.engaged = false;
        }
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::valid_mode_name;

    #[test]
    fn mode_names() {
        assert!(valid_mode_name("inputlock"));
        assert!(valid_mode_name("my lock"));
        assert!(!valid_mode_name(""));
        assert!(!valid_mode_name("x\"; exit"));
        assert!(!valid_mode_name("a;b"));
        assert!(!valid_mode_name("a,b"));
    }
}
