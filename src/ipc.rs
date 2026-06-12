//! Out-of-process trigger: a Unix signal (SIGUSR1) toggles the microphone.
//!
//! Karabiner-Elements (or any other utility) can fire the mute toggle with:
//!
//!     /bin/kill -USR1 "$(cat ~/Library/Application\ Support/mic-mute/mic-mute.pid)"
//!
//! Implementation: SIGUSR1 is blocked process-wide before any threads spawn,
//! then a dedicated thread sigwait()s for it and forwards each delivery to the
//! tao event loop as Message::ToggleMic. End-to-end latency from `kill` to
//! mute action is dominated by fork+exec of /bin/kill (~5 ms); the in-process
//! path is sub-millisecond.

use crate::event_loop::{EventLoopProxyMessage, Message};
use anyhow::{Context, Result};
use libc::{c_int, kill, pthread_sigmask, sigaddset, sigemptyset, sigwait, SIGUSR1, SIG_BLOCK};
use log::{info, trace, warn};
use std::fs;
use std::io::Write;
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::thread;

pub fn pidfile_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mic-mute").join("mic-mute.pid"))
}

/// Block SIGUSR1 in this thread's mask. Call BEFORE any other thread spawns,
/// so every later thread inherits the block. The dedicated signal thread
/// later consumes deliveries via sigwait().
pub fn block_signals() -> Result<()> {
    unsafe {
        let mut set: MaybeUninit<libc::sigset_t> = MaybeUninit::uninit();
        if sigemptyset(set.as_mut_ptr()) != 0 {
            anyhow::bail!("sigemptyset failed");
        }
        if sigaddset(set.as_mut_ptr(), SIGUSR1) != 0 {
            anyhow::bail!("sigaddset(SIGUSR1) failed");
        }
        if pthread_sigmask(SIG_BLOCK, set.as_ptr(), std::ptr::null_mut()) != 0 {
            anyhow::bail!("pthread_sigmask failed");
        }
    }
    Ok(())
}

/// Refuse to start if another mic-mute is alive, otherwise atomically write
/// our pid. Privacy-critical: two instances racing on CoreAudio could leave
/// the microphone unmuted unexpectedly.
pub fn write_pidfile() -> Result<()> {
    let path = pidfile_path().context("Cannot resolve pidfile path")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create pidfile parent")?;
    }

    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(old_pid) = content.trim().parse::<i32>() {
            if old_pid > 0 && unsafe { kill(old_pid, 0) } == 0 {
                anyhow::bail!(
                    "Another mic-mute appears to be running (pid {}). \
                     If this is stale, remove {} and try again.",
                    old_pid,
                    path.display()
                );
            }
        }
    }

    let tmp = path.with_extension("pid.tmp");
    {
        let mut f = fs::File::create(&tmp).context("Failed to create pidfile tmp")?;
        writeln!(f, "{}", std::process::id())?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, &path).context("Failed to rename pidfile into place")?;
    info!("Wrote pidfile {}", path.display());
    Ok(())
}

/// Remove the pidfile, but only if it still holds our pid — never clobber a
/// newer instance's pidfile after a crash/restart race.
pub fn cleanup_pidfile() {
    let Some(path) = pidfile_path() else { return };
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    if content.trim().parse::<u32>().ok() == Some(std::process::id()) {
        let _ = fs::remove_file(&path);
    }
}

/// Spawn the dedicated thread that waits for SIGUSR1 and forwards each
/// delivery to the event loop. Must be called after `block_signals()`.
pub fn start_signal_thread(proxy: EventLoopProxyMessage) {
    thread::Builder::new()
        .name("mic-mute-ipc".to_string())
        .spawn(move || unsafe {
            let mut set: MaybeUninit<libc::sigset_t> = MaybeUninit::uninit();
            if sigemptyset(set.as_mut_ptr()) != 0 || sigaddset(set.as_mut_ptr(), SIGUSR1) != 0 {
                log::error!("Failed to construct SIGUSR1 set in IPC thread");
                return;
            }
            loop {
                let mut sig: c_int = 0;
                let rc = sigwait(set.as_ptr(), &mut sig);
                if rc != 0 {
                    log::error!("sigwait returned errno {}", rc);
                    continue;
                }
                if sig == SIGUSR1 {
                    trace!("SIGUSR1 received — dispatching ToggleMic");
                    if let Err(e) = proxy.send_event(Message::ToggleMic) {
                        warn!("Event loop closed; dropping ToggleMic: {:?}", e);
                        return;
                    }
                }
            }
        })
        .expect("Failed to spawn IPC signal thread");
}
