use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItemBuilder::with_id("status", "Status: Online")
        .enabled(false)
        .build(app)?;
    let wizard = MenuItemBuilder::with_id("wizard", "Preferences…").build(app)?;
    let open = MenuItemBuilder::with_id("open", "Open Downloads Folder").build(app)?;
    let logs = MenuItemBuilder::with_id("logs", "View Activity Log").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&status)
        .separator()
        .item(&wizard)
        .item(&open)
        .item(&logs)
        .separator()
        .item(&quit)
        .build()?;

    let handle = app.clone();
    TrayIconBuilder::with_id("bridge-tray")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "wizard" => crate::open_wizard_window(app),
            "open" => {
                let dir = crate::server::receive_dir();
                if !dir.exists() {
                    std::fs::create_dir_all(&dir).ok();
                }
                if let Err(e) = open_path(app, &dir) {
                    eprintln!("[tray] open failed: {e}");
                }
            }
            "logs" => {
                let path = crate::activity::path();
                crate::activity::write("activity log opened");
                if let Err(e) = open_log(&path) {
                    eprintln!("[tray] log open failed: {e}");
                }
            }
            "quit" => {
                handle.exit(0);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn open_log(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    std::process::Command::new("open")
        .arg("-a")
        .arg("TextEdit")
        .arg(path)
        .spawn()?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("notepad").arg(path).spawn()?;
    Ok(())
}

fn open_path(app: &AppHandle, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(path).spawn()?;
    }
    let _ = app; // reserved for future notification on failure
    Ok(())
}

pub fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}
