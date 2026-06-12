use crate::config::AppVars;
use crate::icons::{rasterize_svg, tray_icon_color};
use anyhow::{Context, Result};
use log::trace;
use muda::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use objc2_core_audio::AudioDeviceID;
use std::fmt;
use tao::window::Theme;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const MUTE_TEXT: &str = "Mute";
const UNMUTE_TEXT: &str = "Unmute";
const NONE_LABEL: &str = "(None)";

pub fn get_mute_menu_text(muted: bool) -> &'static str {
    if muted {
        UNMUTE_TEXT
    } else {
        MUTE_TEXT
    }
}

fn get_image(muted: bool, _theme: Theme) -> Result<(Vec<u8>, u32, u32)> {
    const MIC_ON: &[u8] = include_bytes!("../assets/mic.svg");
    const MIC_OFF: &[u8] = include_bytes!("../assets/mic-off.svg");
    let svg = if muted { MIC_OFF } else { MIC_ON };
    rasterize_svg(svg, &tray_icon_color(muted))
}

fn get_icon(muted: bool, theme: Theme) -> Result<Icon> {
    trace!("Fetching icons");
    let (icon_rgba, icon_width, icon_height) = get_image(muted, theme)?;
    let icon =
        Icon::from_rgba(icon_rgba, icon_width, icon_height).context("Failed to open icon")?;
    Ok(icon)
}

unsafe impl Send for Tray {}
unsafe impl Sync for Tray {}

impl fmt::Debug for Tray {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TrayIcon ID: {:?}", self.systray.id())
    }
}

/// Which entry in the "Preferred Input" submenu was activated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreferredInputSelection {
    /// User picked "(None)" — clear the preference and let macOS decide.
    None,
    /// User picked a specific input device by name.
    Device(String),
}

struct PreferredInputItem {
    menu_id: MenuId,
    selection: PreferredInputSelection,
    item: CheckMenuItem,
}

pub struct Tray {
    pub systray: TrayIcon,
    pub toggle_mute: MenuItem,
    pub launch_at_login: CheckMenuItem,
    pub show_in_dock: CheckMenuItem,
    pub about: MenuItem,
    pub quit: MenuItem,
    preferred_input_menu: Submenu,
    preferred_input_items: Vec<PreferredInputItem>,
    /// Snapshot of the menu's last-rendered state — used to skip rebuilds
    /// when nothing has changed. Stores (device-name-or-None, is_checked).
    preferred_input_snapshot: Vec<(Option<String>, bool)>,
}

impl Tray {
    pub fn new(
        muted: bool,
        theme: Theme,
        app_vars: AppVars,
        login_enabled: bool,
        dock_visible: bool,
    ) -> Result<Self> {
        trace!("Creating tray icon");
        let icon = get_icon(muted, theme)?;
        let tray_menu = Menu::new();
        let toggle_mute = MenuItem::new(get_mute_menu_text(muted), true, None);
        let preferred_input_menu = Submenu::new("Preferred Input", true);
        let launch_at_login = CheckMenuItem::new("Launch at Login", true, login_enabled, None);
        let show_in_dock = CheckMenuItem::new("Show in Dock", true, dock_visible, None);
        let about = MenuItem::new("About", true, None);
        let quit = MenuItem::new("Exit", true, None);

        tray_menu
            .append_items(&[
                &toggle_mute,
                &PredefinedMenuItem::separator(),
                &preferred_input_menu,
                &PredefinedMenuItem::separator(),
                &launch_at_login,
                &show_in_dock,
                &about,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .context("Failed to append menu items")?;

        let systray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip(format!("{} service is running", app_vars.name))
            .with_icon(icon)
            // When muted, use the icon as a macOS template image so the
            // system tints it like other built-in menu-bar icons. When
            // unmuted, leave it as-is so the red color survives intact.
            .with_icon_as_template(muted)
            .with_menu_on_left_click(false)
            .build()
            .context("Failed to create tray icon")?;

        trace!("Tray item created");
        let tray = Self {
            systray,
            toggle_mute,
            launch_at_login,
            show_in_dock,
            about,
            quit,
            preferred_input_menu,
            preferred_input_items: Vec::new(),
            preferred_input_snapshot: Vec::new(),
        };
        Ok(tray)
    }

    pub fn update(&mut self, muted: bool, theme: Theme) -> Result<()> {
        trace!("Updating tray with {} state", get_mute_menu_text(muted));
        self.update_icon(muted, theme)?;
        self.update_menu(muted)?;
        Ok(())
    }

    fn update_icon(&mut self, muted: bool, theme: Theme) -> Result<()> {
        let icon = get_icon(muted, theme)?;
        // Atomic icon + template-flag update so the icon and its tinting
        // mode are consistent. muted → template (system-tinted), unmuted →
        // explicit color.
        self.systray.set_icon_with_as_template(Some(icon), muted)?;
        trace!("Updated tray icon");
        Ok(())
    }

    fn update_menu(&mut self, muted: bool) -> Result<()> {
        self.toggle_mute.set_text(get_mute_menu_text(muted));
        trace!("Updated tray menu");
        Ok(())
    }

    pub fn toggle_mute_id(&self) -> &MenuId {
        self.toggle_mute.id()
    }

    pub fn launch_at_login_id(&self) -> &MenuId {
        self.launch_at_login.id()
    }

    pub fn show_in_dock_id(&self) -> &MenuId {
        self.show_in_dock.id()
    }

    pub fn about_id(&self) -> &MenuId {
        self.about.id()
    }

    pub fn quit_id(&self) -> &MenuId {
        self.quit.id()
    }

    /// If the menu id matches an entry in the "Preferred Input" submenu,
    /// return what the user chose.
    pub fn preferred_input_for_menu_id(&self, id: &MenuId) -> Option<PreferredInputSelection> {
        self.preferred_input_items
            .iter()
            .find(|entry| entry.menu_id == *id)
            .map(|entry| entry.selection.clone())
    }

    /// Rebuild the "Preferred Input" submenu when the device list or the
    /// stored preference changes. Idempotent.
    pub fn refresh_preferred_input(
        &mut self,
        devices: &[(AudioDeviceID, String)],
        preferred: Option<&str>,
    ) -> Result<()> {
        let mut next_snapshot: Vec<(Option<String>, bool)> = Vec::with_capacity(devices.len() + 1);
        next_snapshot.push((None, preferred.is_none()));
        for (_id, name) in devices {
            let checked = preferred == Some(name.as_str());
            next_snapshot.push((Some(name.clone()), checked));
        }

        if next_snapshot == self.preferred_input_snapshot {
            return Ok(());
        }
        trace!(
            "Rebuilding Preferred Input submenu: {} device(s), preferred={:?}",
            devices.len(),
            preferred
        );

        for entry in &self.preferred_input_items {
            let _ = self.preferred_input_menu.remove(&entry.item);
        }
        self.preferred_input_items.clear();

        let none_item = CheckMenuItem::new(NONE_LABEL, true, preferred.is_none(), None);
        self.preferred_input_menu
            .append(&none_item)
            .context("Failed to append (None) item to Preferred Input")?;
        self.preferred_input_items.push(PreferredInputItem {
            menu_id: none_item.id().clone(),
            selection: PreferredInputSelection::None,
            item: none_item,
        });

        for (_id, name) in devices {
            let checked = preferred == Some(name.as_str());
            let item = CheckMenuItem::new(name, true, checked, None);
            self.preferred_input_menu
                .append(&item)
                .context("Failed to append device item to Preferred Input")?;
            self.preferred_input_items.push(PreferredInputItem {
                menu_id: item.id().clone(),
                selection: PreferredInputSelection::Device(name.clone()),
                item,
            });
        }

        self.preferred_input_snapshot = next_snapshot;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_mute_menu_text_muted() {
        assert_eq!(get_mute_menu_text(true), "Unmute");
    }

    #[test]
    fn test_get_mute_menu_text_unmuted() {
        assert_eq!(get_mute_menu_text(false), "Mute");
    }
}
