//! Offline execution: run a command in its own network namespace with only a loopback interface, which
//! is brought up so local test servers (127.0.0.1) still work. Done in the child between fork and exec,
//! with raw syscalls only (the parent may be multi-threaded, so no allocation after fork).

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Make `cmd` run without network. `new_user`: also enter a new user namespace (mapped to root inside),
/// needed when we are not already privileged in our namespace.
pub fn offline(cmd: &mut Command, new_user: bool) {
    // Built before fork.
    let uid_map = format!("0 {} 1", unsafe { libc::getuid() }).into_bytes();
    let gid_map = format!("0 {} 1", unsafe { libc::getgid() }).into_bytes();
    unsafe {
        cmd.pre_exec(move || {
            let flags = if new_user { libc::CLONE_NEWUSER | libc::CLONE_NEWNET } else { libc::CLONE_NEWNET };
            if libc::unshare(flags) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if new_user {
                write_proc(c"/proc/self/setgroups", b"deny")?;
                write_proc(c"/proc/self/uid_map", &uid_map)?;
                write_proc(c"/proc/self/gid_map", &gid_map)?;
            }
            loopback_up();
            Ok(())
        });
    }
}

fn write_proc(path: &std::ffi::CStr, data: &[u8]) -> std::io::Result<()> {
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
        libc::close(fd);
        if n != data.len() as isize {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Best effort: a failure leaves loopback down, as `unshare --net` would.
fn loopback_up() {
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(s, libc::SIOCGIFFLAGS as _, &mut ifr) == 0 {
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            libc::ioctl(s, libc::SIOCSIFFLAGS as _, &ifr);
        }
        libc::close(s);
    }
}

/// Can this process create a user + network namespace? (Probed once, in a child.)
pub fn available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let mut c = Command::new("true");
        offline(&mut c, true);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
    })
}
