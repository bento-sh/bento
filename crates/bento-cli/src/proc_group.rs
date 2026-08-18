//! Process-group control for the long-running `dev` / `serve`
//! children.
//!
//! Serve commands are run through `sh -c`, and `sh` forks the real
//! server. `Child::kill()` only reaps the shell, so every restart
//! left the previous server alive holding its port — the next spawn
//! then failed with EADDRINUSE forever. Each child therefore gets its
//! own process group (`setsid`) and restarts signal the whole group.
//!
//! The cost of a new group is that Ctrl-C, which the terminal
//! delivers to bento's foreground group only, no longer reaches the
//! children — so SIGINT/SIGTERM are handled here and kill the tracked
//! groups before exiting.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};

/// Slots for the process groups a `dev` / `serve` run is supervising
/// (one per dish with a `[serve]` block). Read from a signal handler,
/// so it has to be a fixed-size array of atomics rather than a Vec.
// ponytail: 32 concurrently-served dishes is well past any real
// bento; past that the extra groups just don't get the Ctrl-C sweep.
#[allow(clippy::declare_interior_mutable_const)]
const UNTRACKED: AtomicI32 = AtomicI32::new(0);
static GROUPS: [AtomicI32; 32] = [UNTRACKED; 32];

#[cfg(unix)]
mod sys {
    pub const SIGINT: i32 = 2;
    pub const SIGTERM: i32 = 15;
    pub const SIGKILL: i32 = 9;

    pub type Handler = extern "C" fn(i32);

    // Two syscalls and a signal registration — not worth a `libc`
    // dependency (same posture as bento-core's getuid/getgid).
    unsafe extern "C" {
        pub fn setsid() -> i32;
        pub fn killpg(pgrp: i32, sig: i32) -> i32;
        pub fn signal(sig: i32, handler: Handler) -> usize;
        pub fn _exit(code: i32) -> !;
        #[cfg(test)]
        pub fn kill(pid: i32, sig: i32) -> i32;
    }
}

/// Spawn `cmd` as the leader of a fresh process group, tracked for
/// the Ctrl-C sweep.
pub fn spawn(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            // Failing here (rather than carrying on) matters: without
            // its own group the child's pid is *bento's* group id, and
            // a later killpg would take down the whole session.
            if sys::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    track(child.id() as i32);
    Ok(child)
}

/// Kill the whole group `child` leads, then reap it.
pub fn kill(child: &mut Child) {
    let pgid = child.id() as i32;
    untrack(pgid);
    #[cfg(unix)]
    unsafe {
        sys::killpg(pgid, sys::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

/// Route SIGINT / SIGTERM through a handler that kills the tracked
/// groups first. Idempotent; call once before the first spawn.
pub fn install_signal_handler() {
    #[cfg(unix)]
    unsafe {
        sys::signal(sys::SIGINT, on_signal);
        sys::signal(sys::SIGTERM, on_signal);
    }
}

#[cfg(unix)]
extern "C" fn on_signal(sig: i32) {
    // Async-signal-safe: atomic swaps, killpg, _exit. Nothing here
    // allocates or takes a lock.
    for slot in GROUPS.iter() {
        let pgid = slot.swap(0, Ordering::SeqCst);
        if pgid > 0 {
            unsafe { sys::killpg(pgid, sys::SIGKILL) };
        }
    }
    unsafe { sys::_exit(128 + sig) }
}

fn track(pgid: i32) {
    for slot in GROUPS.iter() {
        if slot
            .compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
}

/// Drop the group before it's signalled — a pgid that outlives its
/// process could be recycled onto something unrelated.
fn untrack(pgid: i32) {
    for slot in GROUPS.iter() {
        let _ = slot.compare_exchange(pgid, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// The regression: `sh -c` forks the real server, so killing the
    /// shell alone leaves it running. Spawn a shell whose child
    /// outlives it, kill the group, and the grandchild must be gone.
    #[test]
    fn kill_takes_down_the_grandchild_not_just_the_shell() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo $!; wait")
            .stdout(Stdio::piped());
        let mut child = spawn(&mut cmd).unwrap();

        let mut buf = String::new();
        {
            use std::io::Read;
            let mut out = child.stdout.take().unwrap();
            let mut byte = [0u8; 1];
            while out.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
                buf.push(byte[0] as char);
            }
        }
        // signal 0 probes liveness without delivering anything.
        let grandchild: i32 = buf.trim().parse().expect("pid on stdout");
        assert_eq!(
            unsafe { sys::kill(grandchild, 0) },
            0,
            "sleep never started"
        );

        kill(&mut child);

        let gone = (0..200).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            (unsafe { sys::kill(grandchild, 0) }) == -1
        });
        assert!(gone, "grandchild {grandchild} survived the group kill");
    }
}
