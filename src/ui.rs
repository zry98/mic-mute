use crate::config::AppVars;
use crate::event_loop::{create, EventIds, EventLoopMessage};
use crate::mic::MicController;
use crate::popup::Popup;
use crate::settings::Settings;
use crate::tray::{PreferredInputSelection, Tray};
use anyhow::{Context, Result};
use log::trace;
use muda::MenuId;

/// Event loop must remain on the main thread and doesn't implement Copy
#[allow(dead_code)]
pub struct UI {
    /// `None` until `install_tray` is called from the event loop's
    /// `StartCause::Init` handler — tray-icon requires the NSStatusItem to be
    /// created *after* the runloop is running, otherwise the menu bar
    /// half-initialises and produces a ghost icon on multi-monitor /
    /// multi-Space setups (tauri-apps/tauri#9480, tauri-apps/tray-icon#90).
    tray: Option<Tray>,
    popup: Popup,
    mic_muted: bool,
    /// Last (muted, device_name, volume) tuple actually rendered to the tray
    /// + popup. The 200 ms enforce poll calls update_mic on every tick; this
    /// lets us short-circuit when nothing the user can see has changed so we
    /// don't keep re-issuing show_front / tray icon redraws.
    last_render: Option<(bool, Option<String>, Option<f32>)>,
}

unsafe impl Send for UI {}
unsafe impl Sync for UI {}

impl UI {
    pub fn new(mic_muted: bool) -> Result<(Self, EventLoopMessage)> {
        let event_loop = create();
        let popup = Popup::new(&event_loop, mic_muted).context("Failed to setup popup window")?;
        let ui = Self {
            tray: None,
            popup,
            mic_muted,
            last_render: None,
        };
        Ok((ui, event_loop))
    }

    /// Build the tray (creates the NSStatusItem). Must be called from the
    /// event loop on `StartCause::Init` so the runloop is already pumping
    /// when AppKit registers the status item — otherwise we get the ghost
    /// icon described in the issues linked on `tray: Option<Tray>` above.
    pub fn install_tray(
        &mut self,
        app_vars: AppVars,
        settings: &Settings,
    ) -> Result<EventIds> {
        if self.tray.is_some() {
            anyhow::bail!("Tray is already installed");
        }
        let theme = self.popup.get_theme();
        let tray = Tray::new(
            self.mic_muted,
            theme,
            app_vars,
            settings.launch_at_login,
            settings.show_in_dock,
            settings.mute_on_start,
        )
        .context("Failed to create system tray")?;

        let event_ids = EventIds {
            button_toggle_mute: tray.toggle_mute_id().clone(),
            button_mute_on_start: tray.mute_on_start_id().clone(),
            button_launch_at_login: tray.launch_at_login_id().clone(),
            button_show_in_dock: tray.show_in_dock_id().clone(),
            button_about: tray.about_id().clone(),
            button_quit: tray.quit_id().clone(),
        };
        self.tray = Some(tray);
        Ok(event_ids)
    }

    pub fn update_mic(
        &mut self,
        muted: bool,
        active_device_name: Option<&str>,
        volume: Option<f32>,
    ) -> Result<&mut Self> {
        // Compare against the last rendered tuple without allocating a new
        // owned String on the hot path — the enforce poll calls this every
        // tick and most ticks are no-ops.
        let unchanged = matches!(
            &self.last_render,
            Some((m, n, v))
                if *m == muted
                    && *v == volume
                    && n.as_deref() == active_device_name
        );
        if unchanged {
            return Ok(self);
        }
        trace!("Updating UI mic state {}", muted);
        self.mic_muted = muted;
        if let Some(tray) = &mut self.tray {
            tray.update(muted, self.popup.get_theme())
                .context("Failed to update UI tray")?;
        }
        self.popup
            .update(muted, active_device_name, volume)
            .context("Failed to update UI popup")?;
        self.last_render = Some((muted, active_device_name.map(str::to_string), volume));
        Ok(self)
    }

    pub fn hide_popup(&mut self) -> Result<&mut Self> {
        self.popup.hide().context("Failed to hide UI popup")?;
        Ok(self)
    }

    /// Bring the popup to the front. Use this on user-initiated toggles so
    /// both mute and unmute get visible feedback (the popup auto-hides 1 s
    /// later via the HidePopup timer).
    pub fn show_popup(&mut self) -> Result<&mut Self> {
        self.popup.show().context("Failed to show UI popup")?;
        Ok(self)
    }

    /// Apply all settings to the live app state.
    /// Safe to call whenever settings change — all operations are idempotent.
    pub fn apply_settings(&mut self, settings: &Settings) -> Result<()> {
        // Sync tray checkboxes with persisted settings.
        if let Some(tray) = &self.tray {
            tray.show_in_dock.set_checked(settings.show_in_dock);
            tray.launch_at_login.set_checked(settings.launch_at_login);
            tray.mute_on_start.set_checked(settings.mute_on_start);
        }
        crate::launch_at_login::set_dock_visible(settings.show_in_dock);
        if let Err(e) = crate::launch_at_login::set(settings.launch_at_login) {
            log::error!("Failed to apply launch_at_login setting: {}", e);
        }

        Ok(())
    }

    pub fn detect(&mut self) -> Result<&mut Self> {
        self.popup
            .detect_cursor_monitor()
            .context("Failed to update UI popup placement")?;
        Ok(self)
    }

    /// Rebuild the "Preferred Input" submenu — checks the entry that matches
    /// the currently-pinned device (or "(None)" if no preference is set).
    /// Idempotent: only mutates the tray when something has actually changed.
    /// No-op until the tray is installed.
    pub fn refresh_preferred_input(
        &mut self,
        controller: &MicController,
        preferred: Option<&str>,
    ) -> Result<()> {
        let Some(tray) = &mut self.tray else {
            return Ok(());
        };
        let devices = controller.list_input_devices().unwrap_or_default();
        tray.refresh_preferred_input(&devices, preferred)
    }

    /// Look up the "Preferred Input" submenu selection (if any) bound to the
    /// given menu id.
    pub fn preferred_input_for_menu_id(&self, id: &MenuId) -> Option<PreferredInputSelection> {
        self.tray.as_ref()?.preferred_input_for_menu_id(id)
    }
}
