use crate::about::show_about;
use crate::audio_events::MuteListeners;
use crate::config::AppVars;
use crate::launch_at_login;
use crate::mic::MicController;
use crate::settings::Settings;
use crate::tray::PreferredInputSelection;
use crate::ui::UI;
use async_std::task;
use core_foundation_sys::runloop::{CFRunLoopGetMain, CFRunLoopWakeUp};
use log::trace;
use muda::{MenuEvent, MenuId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

/// Safety-net poll interval. Primary mute enforcement is driven by
/// per-device CoreAudio property listeners (see `audio_events::MuteListeners`),
/// so this just exists to catch any edge case where a listener might miss
/// an event. 2 s is also slow enough for cursor-monitor popup placement.
const POLL_INTERVAL_MILLIS: u64 = 2000;

#[derive(Debug)]
pub enum Message {
    /// Hide the popup if the carried generation still matches the latest
    /// toggle — protects against the timer scheduled by an earlier toggle
    /// firing after the user has toggled again.
    HidePopup(u64),
    /// Out-of-process toggle request (from the SIGUSR1 IPC thread).
    ToggleMic,
    /// CoreAudio fired a property listener for the device set or the system
    /// default input device. Rebuild the input-device submenu.
    InputDevicesChanged,
    /// A per-device mute/volume property listener fired — something
    /// (possibly another app) changed mute or volume on a device we've
    /// muted. Re-assert the desired state.
    ExternalMuteChanged(u32),
}

pub type EventLoopMessage = EventLoop<Message>;
pub type EventLoopProxyMessage = tao::event_loop::EventLoopProxy<Message>;

pub fn create() -> EventLoopMessage {
    EventLoopBuilder::<Message>::with_user_event().build()
}

pub struct EventIds {
    pub button_toggle_mute: MenuId,
    pub button_mute_on_start: MenuId,
    pub button_launch_at_login: MenuId,
    pub button_show_in_dock: MenuId,
    pub button_about: MenuId,
    pub button_quit: MenuId,
}

/// Shared event-loop state. Holds the Arc-counted handles every helper needs
/// so call sites become `ctx.update_mic(true)` instead of cloning five Arcs.
struct EventLoopCtx {
    ui: Arc<RwLock<UI>>,
    controller: Arc<RwLock<MicController>>,
    settings: Arc<RwLock<Settings>>,
    mute_listeners: Arc<Mutex<MuteListeners>>,
    proxy: EventLoopProxyMessage,
    popup_generation: Arc<AtomicU64>,
}

impl EventLoopCtx {
    fn update_mic(&self, toggle: bool) {
        let mut controller_guard = self.controller.write().unwrap();
        if toggle || controller_guard.should_enforce_mute() {
            let state = if toggle { None } else { Some(true) };
            if let Err(err) = controller_guard.toggle(state) {
                log::error!("Failed to update microphone mute state: {}", err);
            }
            let device_name = controller_guard.active_device_name();
            let volume = controller_guard.current_input_volume();
            let muted = controller_guard.muted;
            let tracked = controller_guard.tracked_device_ids();
            // Release the controller write lock before grabbing the listener
            // mutex — they are independent, and listeners.sync may run for a
            // short while if many devices changed.
            drop(controller_guard);

            // Keep per-device mute listeners aligned with the tracking set.
            self.mute_listeners.lock().unwrap().sync(&tracked);

            let mut ui = self.ui.write().unwrap();
            ui.update_mic(muted, device_name.as_deref(), volume).unwrap();
            // Show the popup as transient feedback for user-initiated toggles
            // (mute or unmute). The HidePopup timer scheduled below tucks it
            // back away after 1 s. Enforce paths do not show the popup.
            if toggle {
                ui.show_popup().unwrap();
            }
        }
        if toggle {
            // Schedule auto-hide 1s after every toggle (mute OR unmute). The
            // generation counter coalesces rapid back-to-back toggles so the
            // popup stays for a full second after the LAST toggle.
            let gen = self.popup_generation.fetch_add(1, Ordering::Relaxed) + 1;
            let proxy = self.proxy.clone();
            task::spawn(async move {
                task::sleep(Duration::from_secs(1)).await;
                proxy.send_event(Message::HidePopup(gen)).unwrap();
            });
        }
    }

    /// Re-assert the user's preferred input device as the system default when
    /// macOS (or anything else) has moved the default off it. Always refreshes
    /// the "Preferred Input" submenu so its check state stays in sync.
    fn enforce_preferred_input(&self) {
        let preferred = self.settings.read().unwrap().preferred_input_device.clone();

        if let Some(name) = preferred.as_deref() {
            let (current_name, target_id) = {
                let c = self.controller.read().unwrap();
                (c.active_device_name(), c.find_input_device_id_by_name(name))
            };
            if current_name.as_deref() != Some(name) {
                if let Some(id) = target_id {
                    trace!("Enforcing preferred input '{}' → AudioDeviceID {}", name, id);
                    if let Err(e) = self.controller.write().unwrap().set_default_input_device(id) {
                        log::error!("Failed to enforce preferred input: {}", e);
                    }
                } else {
                    trace!("Preferred input '{}' not currently available", name);
                }
            }
        }

        let c = self.controller.read().unwrap();
        let mut ui_w = self.ui.write().unwrap();
        if let Err(e) = ui_w.refresh_preferred_input(&c, preferred.as_deref()) {
            log::error!("Failed to refresh Preferred Input submenu: {}", e);
        }
    }
}

pub fn restore_microphone_on_exit(controller: &Arc<RwLock<MicController>>) {
    if let Err(err) = controller.write().unwrap().restore_on_exit() {
        log::error!("Failed to restore microphone state on exit: {}", err);
    }
}

pub fn start(
    mut event_loop: EventLoop<Message>,
    ui: Arc<RwLock<UI>>,
    controller: Arc<RwLock<MicController>>,
    settings: Arc<RwLock<Settings>>,
    app_vars: AppVars,
) {
    let poll_interval = Duration::from_millis(POLL_INTERVAL_MILLIS);
    // Start in the past so the first iteration triggers the poll immediately.
    let mut last_poll = Instant::now() - poll_interval;

    // Poll the settings file for changes every 2 seconds so edits to
    // settings.json take effect without restarting the app.
    let settings_poll_interval = Duration::from_secs(2);
    let mut last_settings_check = Instant::now();
    let mut last_settings_mtime = Settings::mtime();

    // Register CoreAudio property listeners that push InputDevicesChanged into
    // the event loop. Held in a local so the OS cleans up at process exit.
    let _audio_listeners =
        crate::audio_events::DeviceListeners::install(event_loop.create_proxy())
            .expect("Failed to install CoreAudio property listeners");

    let ctx = EventLoopCtx {
        ui,
        controller,
        settings,
        // Per-device mute/volume listeners follow the controller's tracking
        // set — empty on startup, populated when we mute, drained when we
        // unmute.
        mute_listeners: Arc::new(Mutex::new(MuteListeners::new(event_loop.create_proxy()))),
        proxy: event_loop.create_proxy(),
        // Generation counter that lets HidePopup messages tell whether their
        // scheduling toggle is still the most-recent one.
        popup_generation: Arc::new(AtomicU64::new(0)),
    };

    trace!("Starting event loop");
    // Set activation policy based on persisted show_in_dock before the loop starts.
    let initial_show_in_dock = ctx.settings.read().unwrap().show_in_dock;
    event_loop.set_activation_policy(if initial_show_in_dock {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    });
    // Populated during StartCause::Init below — we deliberately defer the
    // NSStatusItem creation until the runloop is running to avoid the ghost
    // status item on multi-monitor setups
    // (tauri-apps/tauri#9480, tauri-apps/tray-icon#90).
    let mut event_ids: Option<EventIds> = None;
    event_loop.run(move |event, _, control_flow| {
        let mut exit_requested = false;

        match event {
            Event::NewEvents(StartCause::Init) => {
                // tray-icon requires NSStatusItem to be created *after* the
                // runloop is actively running — see comment on UI::tray.
                let s = ctx.settings.read().unwrap();
                let new_event_ids = ctx
                    .ui
                    .write()
                    .unwrap()
                    .install_tray(app_vars.clone(), &s)
                    .expect("Failed to install system tray");
                drop(s);
                event_ids = Some(new_event_ids);
                // Populate the Preferred Input submenu and enforce the pinned
                // input device once on startup; listeners only fire on changes.
                ctx.enforce_preferred_input();
                // Kick the runloop so the freshly-registered NSStatusItem
                // gets drawn immediately rather than on the next external
                // event. Mirrors the recipe in tray-icon's tao/winit examples.
                unsafe { CFRunLoopWakeUp(CFRunLoopGetMain()); }
            }
            Event::UserEvent(Message::HidePopup(gen)) => {
                // Ignore stale timers from earlier toggles.
                if ctx.popup_generation.load(Ordering::Relaxed) == gen {
                    ctx.ui.write().unwrap().hide_popup().unwrap();
                }
            }
            Event::UserEvent(Message::ToggleMic) => {
                trace!("ToggleMic event received from IPC");
                ctx.update_mic(true);
            }
            Event::UserEvent(Message::InputDevicesChanged) => {
                trace!("CoreAudio device set/default changed — enforcing preference + mute");
                ctx.enforce_preferred_input();
                // A new input device may have appeared. Re-enforce mute so
                // any newly-discovered device gets muted right away when
                // we're already in desired_muted state.
                ctx.update_mic(false);
            }
            Event::UserEvent(Message::ExternalMuteChanged(device_id)) => {
                trace!("Per-device listener fired for {} — re-asserting mute", device_id);
                ctx.update_mic(false);
            }
            _ => {}
        };

        if let (Ok(event), Some(ids)) = (MenuEvent::receiver().try_recv(), event_ids.as_ref()) {
            trace!("Tray menu event: {:?}", event);
            if event.id == ids.button_quit {
                trace!("Exit tray menu item selected");
                exit_requested = true;
            } else if event.id == ids.button_toggle_mute {
                trace!("Toggle mic tray menu item selected");
                ctx.update_mic(true);
            } else if event.id == ids.button_launch_at_login {
                trace!("Launch at login toggled");
                let mut s = ctx.settings.write().unwrap();
                s.launch_at_login = !s.launch_at_login;
                let enabled = s.launch_at_login;
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
                drop(s);
                if let Err(e) = launch_at_login::set(enabled) {
                    log::error!("Launch at login error: {}", e);
                }
            } else if event.id == ids.button_show_in_dock {
                trace!("Show in dock toggled");
                let mut s = ctx.settings.write().unwrap();
                s.show_in_dock = !s.show_in_dock;
                let visible = s.show_in_dock;
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
                drop(s);
                launch_at_login::set_dock_visible(visible);
            } else if event.id == ids.button_mute_on_start {
                let mut s = ctx.settings.write().unwrap();
                s.mute_on_start = !s.mute_on_start;
                trace!("Mute on Start toggled → {}", s.mute_on_start);
                if let Err(e) = s.save() {
                    log::error!("Failed to save settings: {}", e);
                }
            } else if event.id == ids.button_about {
                trace!("About tray menu item selected");
                let mut s = ctx.settings.write().unwrap();
                match show_about(&mut s) {
                    Ok(true) => {
                        // Reset to Default clicked — apply all settings immediately
                        if let Err(e) = ctx.ui.write().unwrap().apply_settings(&s) {
                            log::error!("Failed to apply settings: {}", e);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => log::error!("Preferences error: {}", e),
                }
            } else if let Some(selection) = {
                // Scope the ui.read() to this block — the temporary guard
                // from an if-let condition lives until the end of the body,
                // and `ctx.enforce_preferred_input` below takes ui.write(),
                // which would self-deadlock the main thread.
                let u = ctx.ui.read().unwrap();
                u.preferred_input_for_menu_id(&event.id)
            } {
                {
                    let mut s = ctx.settings.write().unwrap();
                    s.preferred_input_device = match selection {
                        PreferredInputSelection::None => None,
                        PreferredInputSelection::Device(name) => Some(name),
                    };
                    if let Err(e) = s.save() {
                        log::error!("Failed to save settings: {}", e);
                    }
                    trace!("Preferred input updated: {:?}", s.preferred_input_device);
                }
                ctx.enforce_preferred_input();
                // Refresh the popup line so the device name + volume match
                // whatever the system default is right now (it may have just
                // changed as a result of enforcement).
                let (muted, device_name, volume) = {
                    let c = ctx.controller.read().unwrap();
                    (c.muted, c.active_device_name(), c.current_input_volume())
                };
                if let Err(e) = ctx
                    .ui
                    .write()
                    .unwrap()
                    .update_mic(muted, device_name.as_deref(), volume)
                {
                    log::error!("Failed to update mic UI after preferred change: {}", e);
                }
            }
        }

        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                trace!("Tray icon left-clicked — toggling mic");
                ctx.update_mic(true);
            }
        }

        // Reload settings if the file has been modified since we last checked.
        if last_settings_check.elapsed() >= settings_poll_interval {
            last_settings_check = Instant::now();
            let current_mtime = Settings::mtime();
            if current_mtime != last_settings_mtime {
                last_settings_mtime = current_mtime;
                trace!("settings.json changed on disk — reloading");
                let new_settings = Settings::load();
                *ctx.settings.write().unwrap() = new_settings.clone();
                if let Err(e) = ctx.ui.write().unwrap().apply_settings(&new_settings) {
                    log::error!("Failed to apply reloaded settings: {}", e);
                } else {
                    trace!("Settings reloaded from settings.json");
                }
                ctx.enforce_preferred_input();
            }
        }

        // Safety-net poll for mic state + cursor-monitor position. Primary
        // enforcement is listener-driven now (Message::ExternalMuteChanged),
        // so this just catches any edge case where a property listener
        // doesn't fire.
        if last_poll.elapsed() >= poll_interval {
            last_poll = Instant::now();
            ctx.update_mic(false);
            ctx.ui.write().unwrap().detect().unwrap();
        }

        if exit_requested {
            restore_microphone_on_exit(&ctx.controller);
            crate::ipc::cleanup_pidfile();
            *control_flow = ControlFlow::Exit;
        } else {
            // Sleep until the next scheduled check rather than spinning.
            let next_poll = last_poll + poll_interval;
            let next_settings = last_settings_check + settings_poll_interval;
            *control_flow = ControlFlow::WaitUntil(next_poll.min(next_settings));
        }
    });
}
