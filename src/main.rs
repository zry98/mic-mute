mod about;
mod audio_events;
mod config;
mod event_loop;
mod icons;
mod ipc;
mod launch_at_login;
mod mic;
mod popup;
mod popup_content;
mod settings;
mod tray;
mod ui;
mod utils;
// TODO: Use better Apple logging support? https://lib.rs/crates/oslog

#[macro_use]
extern crate objc;

use crate::config::AppVars;
use crate::event_loop::{restore_microphone_on_exit, start};
use crate::mic::MicController;
use crate::settings::Settings;
use crate::ui::UI;
use crate::utils::arc_lock;
use env_logger::{Builder, Env};
use log::{info, trace};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

fn main() {
    Builder::from_env(Env::default().default_filter_or("trace")).init();
    info!("Starting app");

    // Block SIGUSR1 process-wide BEFORE any thread spawns, so every later
    // thread inherits the block. The dedicated IPC thread consumes signals
    // via sigwait().
    if let Err(e) = ipc::block_signals() {
        log::error!("Failed to block SIGUSR1: {}", e);
        std::process::exit(1);
    }

    // Refuse to start if another instance is already running. Privacy-critical:
    // racing instances on CoreAudio could leave the mic unexpectedly hot.
    if let Err(e) = ipc::write_pidfile() {
        log::error!("{}", e);
        std::process::exit(1);
    }

    let mut settings = Settings::load();

    // On first run (or after upgrading from a version without launch_at_login in
    // settings), adopt the existing plist state so we don't silently disable it.
    let plist_enabled = launch_at_login::is_enabled();
    if plist_enabled != settings.launch_at_login {
        settings.launch_at_login = plist_enabled;
        let _ = settings.save();
    }

    let app_vars = AppVars::new();

    let controller = MicController::new().unwrap();
    let mic_muted = controller.muted;
    let controller = arc_lock(controller);
    trace!("Mic controller initialized {:?}", controller);

    // Register SIGTERM/SIGINT handlers. The signal handler only sets a flag;
    // a background thread performs microphone cleanup before exiting.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
    }
    let shutdown_controller = controller.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            info!("Signal received — restoring microphone state before exit");
            // Defense-in-depth: if restore hangs (e.g. a future deadlock in
            // the main thread leaves controller.write() unobtainable), force
            // the process to exit rather than blocking the user's terminal
            // forever. The mic may remain in its current state in that case.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(3));
                log::error!(
                    "Shutdown watchdog tripped — force-exiting (mic may remain in last state)"
                );
                std::process::exit(1);
            });
            restore_microphone_on_exit(&shutdown_controller);
            ipc::cleanup_pidfile();
            std::process::exit(0);
        }
    });

    let (ui, event_loop, event_ids) = UI::new(mic_muted, app_vars, &settings).unwrap();
    trace!("UI initialized");

    // Start the SIGUSR1 listener thread now that we have an EventLoopProxy.
    ipc::start_signal_thread(event_loop.create_proxy());

    let ui = arc_lock(ui);
    let settings = arc_lock(settings);
    start(event_loop, event_ids, ui, controller, settings);
}
