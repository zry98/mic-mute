/// About window for the app.
/// Shows version info, settings location, and a link to the GitHub repo via a native macOS NSAlert.
use crate::ipc;
use crate::settings::Settings;
use anyhow::Result;
use cocoa::base::nil;
use cocoa::foundation::NSString;
use objc::runtime::Object;
use std::process::Command;

/// Show the About window as an NSAlert dialog.
/// Returns Ok(true) if settings were changed and need re-applying, Ok(false) otherwise.
pub fn show_about(_settings: &mut Settings) -> Result<bool> {
    let pidfile = ipc::pidfile_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/Library/Application Support/mic-mute/mic-mute.pid".to_string());

    let response = unsafe {
        let alert: *mut Object = msg_send![class!(NSAlert), new];

        let title = NSString::alloc(nil).init_str("Mic Mute");
        let _: () = msg_send![alert, setMessageText: title];
        let _: () = msg_send![title, release];

        let version = env!("CARGO_PKG_VERSION");
        let info = format!(
            "Toggle mute by left-clicking the menu bar icon, or send SIGUSR1 to the running process:\n\n  /bin/kill -USR1 \"$(cat {pidfile})\"\n\nUse Karabiner-Elements (or any hotkey utility) to bind this command to a key.\n\nSettings:\n~/Library/Application Support/mic-mute/settings.json\n\nVersion: {version}\n\nSource:\ngithub.com/brettinternet/mic-mute"
        );
        let info_str = NSString::alloc(nil).init_str(&info);
        let _: () = msg_send![alert, setInformativeText: info_str];
        let _: () = msg_send![info_str, release];

        let ok_str = NSString::alloc(nil).init_str("OK");
        let _: () = msg_send![alert, addButtonWithTitle: ok_str];
        let _: () = msg_send![ok_str, release];
        let open_str = NSString::alloc(nil).init_str("Open Settings");
        let _: () = msg_send![alert, addButtonWithTitle: open_str];
        let _: () = msg_send![open_str, release];

        // 1000 = OK, 1001 = Open Settings
        let response: i64 = msg_send![alert, runModal];
        let _: () = msg_send![alert, release];
        response
    };

    if response == 1001 {
        if let Some(path) = dirs::config_dir().map(|d| d.join("mic-mute").join("settings.json")) {
            let _ = Command::new("open").arg("-t").arg(&path).spawn();
        }
    }
    Ok(false)
}
