#[cfg_attr(mobile, tauri::mobile_entry_point)]
mod app;
mod util;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::{webview::PageLoadEvent, Manager, Url, WebviewWindow};
use tauri_plugin_window_state::Builder as WindowStatePlugin;
use tauri_plugin_window_state::StateFlags;

#[cfg(target_os = "macos")]
use std::time::Duration;

// Fallback when PageLoadEvent::Finished never arrives (offline / stalled).
// Deliberately longer than a paint tick so the normal path can win first.
const STARTUP_WINDOW_FALLBACK_DELAY: u64 = 3_000;
#[cfg(target_os = "linux")]
const PAKE_LINUX_WEBKIT_SAFE_MODE: &str = "PAKE_LINUX_WEBKIT_SAFE_MODE";
#[cfg(target_os = "linux")]
const WEBKIT_DISABLE_DMABUF_RENDERER: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
#[cfg(target_os = "linux")]
const WEBKIT_DISABLE_COMPOSITING_MODE: &str = "WEBKIT_DISABLE_COMPOSITING_MODE";
#[cfg(target_os = "linux")]
const WEBKIT_DMABUF_RENDERER_FORCE_SHM: &str = "WEBKIT_DMABUF_RENDERER_FORCE_SHM";
#[cfg(target_os = "linux")]
const GDK_BACKEND: &str = "GDK_BACKEND";

#[cfg(target_os = "macos")]
use app::window::enter_configured_fullscreen;
use app::{
    invoke::{
        clear_dock_badge, close_notification, download_file, increment_dock_badge, quit_app,
        reveal_download, send_notification, set_dock_badge, set_dock_badge_label,
        set_title_bar_color, set_zoom, update_theme_mode, webview_navigate,
    },
    setup::{set_global_shortcut, set_system_tray},
    window::{
        open_additional_window_safe, reapply_window_icon, reveal_built_window, save_last_url,
        set_window, MultiWindowState,
    },
};
use util::get_pake_config;

/// Placeholder documents used before the real target URL navigates (e.g. macOS
/// cert-bypass starts on about:blank). Revealing on these would reintroduce the
/// blank-window flash the page-load gate is meant to prevent.
fn is_placeholder_startup_url(url: &Url) -> bool {
    url.scheme().eq_ignore_ascii_case("about")
}

/// First automatic reveal wins. Returns true if the caller should show the window.
fn claim_startup_reveal(revealed: &AtomicBool) -> bool {
    !revealed.swap(true, Ordering::AcqRel)
}

/// User took control of main-window visibility (tray, shortcut, dock, second
/// instance, hide-on-close). Drop any pending page-load / fallback reveal so a
/// slow cold start cannot re-open a window the user just hid.
pub(crate) fn cancel_startup_reveal(revealed: &AtomicBool) {
    revealed.store(true, Ordering::Release);
}

fn reveal_startup_window(window: WebviewWindow, init_fullscreen: bool, revealed: &Arc<AtomicBool>) {
    if !claim_startup_reveal(revealed) {
        return;
    }

    tauri::async_runtime::spawn(async move {
        let _ = window.show();
        reapply_window_icon(&window);

        // Fixed: Linux fullscreen issue with virtual keyboard
        #[cfg(target_os = "linux")]
        {
            if init_fullscreen {
                let _ = window.set_fullscreen(true);
                // Ensure webview maintains focus for input after fullscreen
                let _ = window.set_focus();
            } else {
                // Fix: Ubuntu 24.04/GNOME window buttons non-functional until resize (#1122)
                // The window manager needs time to process the MapWindow event before
                // accepting focus requests. Without this, decorations remain non-interactive.
                tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
                let _ = window.set_focus();
            }
        }

        #[cfg(target_os = "macos")]
        enter_configured_fullscreen(&window);

        #[cfg(not(target_os = "linux"))]
        let _ = init_fullscreen;
    });
}

#[cfg(any(target_os = "linux", test))]
fn is_disabled_env_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no" | "native" | "disabled"
    )
}

#[cfg(any(target_os = "linux", test))]
fn is_non_empty_env_value(value: Option<&str>) -> bool {
    value.map(|value| !value.trim().is_empty()).unwrap_or(false)
}

#[cfg(any(target_os = "linux", test))]
fn contains_niri(value: &str) -> bool {
    value
        .split([':', ';', ',', ' '])
        .any(|part| part.eq_ignore_ascii_case("niri"))
}

#[cfg(any(target_os = "linux", test))]
fn is_niri_session(niri_socket: Option<&str>, desktop_values: &[Option<&str>]) -> bool {
    is_non_empty_env_value(niri_socket)
        || desktop_values
            .iter()
            .flatten()
            .any(|value| contains_niri(value))
}

#[cfg(any(target_os = "linux", test))]
fn should_disable_dmabuf_renderer(
    safe_mode: Option<&str>,
    niri_socket: Option<&str>,
    desktop_values: &[Option<&str>],
) -> bool {
    if let Some(value) = safe_mode.filter(|value| !value.trim().is_empty()) {
        return !is_disabled_env_value(value);
    }

    !is_niri_session(niri_socket, desktop_values)
}

#[cfg(any(target_os = "linux", test))]
fn should_disable_compositing_mode(
    safe_mode: Option<&str>,
    niri_socket: Option<&str>,
    wayland_display: Option<&str>,
    gdk_backend: Option<&str>,
    desktop_values: &[Option<&str>],
) -> bool {
    if let Some(value) = safe_mode.filter(|value| !value.trim().is_empty()) {
        return !is_disabled_env_value(value);
    }

    if gdk_backend.is_some_and(|value| value.trim().eq_ignore_ascii_case("x11")) {
        return false;
    }

    if !is_non_empty_env_value(wayland_display) {
        return false;
    }

    !is_niri_session(niri_socket, desktop_values)
}

#[cfg(any(target_os = "linux", test))]
fn should_use_shm_renderer(
    webkit_version: (u32, u32),
    safe_mode: Option<&str>,
    disable_dmabuf: bool,
) -> bool {
    webkit_version >= (2, 52) && !is_non_empty_env_value(safe_mode) && disable_dmabuf
}

#[cfg(any(target_os = "linux", test))]
fn should_force_wayland_gdk_backend(
    gdk_backend: Option<&str>,
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> bool {
    if is_non_empty_env_value(gdk_backend) {
        return false;
    }

    is_non_empty_env_value(wayland_display) && !is_non_empty_env_value(display)
}

#[cfg(target_os = "linux")]
fn apply_linux_gdk_backend() {
    if should_force_wayland_gdk_backend(
        std::env::var(GDK_BACKEND).ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
    ) {
        std::env::set_var(GDK_BACKEND, "wayland");
    }
}

#[cfg(target_os = "linux")]
fn apply_linux_webkit_runtime_flags() {
    let safe_mode = std::env::var(PAKE_LINUX_WEBKIT_SAFE_MODE).ok();
    if safe_mode.as_deref().is_some_and(is_disabled_env_value) {
        std::env::remove_var(WEBKIT_DISABLE_DMABUF_RENDERER);
        std::env::remove_var(WEBKIT_DISABLE_COMPOSITING_MODE);
        return;
    }

    let desktop_values = [
        std::env::var("XDG_CURRENT_DESKTOP").ok(),
        std::env::var("XDG_SESSION_DESKTOP").ok(),
        std::env::var("DESKTOP_SESSION").ok(),
    ];
    let desktop_refs = desktop_values
        .iter()
        .map(|value| value.as_deref())
        .collect::<Vec<_>>();

    let niri_socket = std::env::var("NIRI_SOCKET").ok();

    let disable_dmabuf =
        should_disable_dmabuf_renderer(safe_mode.as_deref(), niri_socket.as_deref(), &desktop_refs);
    let disable_compositing = should_disable_compositing_mode(
        safe_mode.as_deref(),
        niri_socket.as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var(GDK_BACKEND).ok().as_deref(),
        &desktop_refs,
    );
    // SAFETY: WebKit's version getters take no pointers and require no GTK
    // initialization. They report the loaded library, not the build host.
    let webkit_version = unsafe {
        (
            webkit2gtk::ffi::webkit_get_major_version(),
            webkit2gtk::ffi::webkit_get_minor_version(),
        )
    };
    if should_use_shm_renderer(webkit_version, safe_mode.as_deref(), disable_dmabuf)
        && std::env::var_os(WEBKIT_DISABLE_DMABUF_RENDERER).is_none()
        && std::env::var_os(WEBKIT_DISABLE_COMPOSITING_MODE).is_none()
    {
        if std::env::var_os(WEBKIT_DMABUF_RENDERER_FORCE_SHM).is_none() {
            std::env::set_var(WEBKIT_DMABUF_RENDERER_FORCE_SHM, "1");
        }
        return;
    }

    if disable_dmabuf && std::env::var_os(WEBKIT_DISABLE_DMABUF_RENDERER).is_none() {
        std::env::set_var(WEBKIT_DISABLE_DMABUF_RENDERER, "1");
    }
    if disable_compositing && std::env::var_os(WEBKIT_DISABLE_COMPOSITING_MODE).is_none() {
        std::env::set_var(WEBKIT_DISABLE_COMPOSITING_MODE, "1");
    }
}

pub fn run_app() {
    #[cfg(target_os = "linux")]
    {
        apply_linux_gdk_backend();
        apply_linux_webkit_runtime_flags();
    }

    let (pake_config, tauri_config) = get_pake_config();
    // Set the shell's AppUserModelID to its identifier so shortcuts and toasts
    // group with its windows and match ToastNotificationManager's AUMID.
    #[cfg(windows)]
    util::set_app_user_model_id(&tauri_config.identifier);
    // A runtime app's login item follows its own setting; a CLI build has none to keep.
    if pake_config.runtime_app {
        app::autostart::reconcile(
            pake_config.launch_at_login,
            &tauri_config.identifier,
            tauri_config.product_name.as_deref().unwrap_or("Pake"),
        );
    }
    let mut context = tauri::generate_context!();
    // Paths, app data and the macOS menu derive from the context, so a runtime
    // app must not share the template's identity.
    if pake_config.runtime_app {
        context.config_mut().identifier = tauri_config.identifier.clone();
        if let Some(name) = &tauri_config.product_name {
            context.config_mut().product_name = Some(name.clone());
            context.package_info_mut().name = name.clone();
        }
    }

    let tauri_app = tauri::Builder::default();

    let show_system_tray = pake_config.show_system_tray();
    let hide_on_close = pake_config.windows[0].hide_on_close;
    let remember_url = pake_config.windows[0].remembers_last_url();
    let activation_shortcut = pake_config.windows[0].activation_shortcut.clone();
    let init_fullscreen = pake_config.windows[0].fullscreen;
    let start_to_tray = pake_config.windows[0].start_to_tray && show_system_tray;

    // Only Gmail, Google Chat, Calendar, and Apple Music stay resident in the system tray;
    // all other apps (Meet, Gemini, iCloud, etc.) have no tray icon and cold-exit on X.
    let product_lower = tauri_config
        .product_name
        .as_deref()
        .unwrap_or("")
        .to_lowercase();
    let is_calendar_app = product_lower == "calendar"
        || product_lower == "google calendar"
        || product_lower == "googlecalendar";
    let is_apple_music_app = product_lower == "apple music" || product_lower == "applemusic";
    let is_tray_app = product_lower == "gmail"
        || product_lower == "google chat"
        || product_lower == "googlechat"
        || is_calendar_app
        || is_apple_music_app;
    let show_system_tray = is_tray_app;
    let hide_on_close = is_tray_app;
    let start_to_tray = start_to_tray && is_tray_app;

    let multi_instance = pake_config.multi_instance;
    let multi_window = pake_config.multi_window;
    let _enable_find = pake_config.windows[0].enable_find;
    let startup_window_revealed = Arc::new(AtomicBool::new(false));

    let window_state_plugin = WindowStatePlugin::default()
        .with_state_flags(if init_fullscreen {
            StateFlags::FULLSCREEN
        } else {
            // Prevent flickering on the first open.
            // Exclude FULLSCREEN so a prior --fullscreen build's persisted state
            // doesn't force fullscreen on a rebuild without --fullscreen.
            StateFlags::all() & !StateFlags::VISIBLE & !StateFlags::FULLSCREEN
        })
        .build();

    #[allow(deprecated)]
    let mut app_builder = tauri_app
        .plugin(window_state_plugin)
        .plugin(tauri_plugin_oauth::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init());

    // Only add single instance plugin if multiple instances are not allowed
    if !multi_instance {
        let instance_revealed = startup_window_revealed.clone();
        app_builder = app_builder.plugin(tauri_plugin_single_instance::init(
            move |app, args, _cwd| {
                let nav_url = args
                    .iter()
                    .skip(1)
                    .find(|arg| arg.starts_with("http://") || arg.starts_with("https://"))
                    .and_then(|arg| tauri::Url::parse(arg).ok());
                let is_toast_activation = args
                    .iter()
                    .any(|arg| arg.to_ascii_lowercase().contains("toastactivated"));

                if multi_window && !is_toast_activation && nav_url.is_none() {
                    open_additional_window_safe(app);
                } else if let Some(window) = app.get_webview_window("pake") {
                    cancel_startup_reveal(&instance_revealed);
                    if let Some(target_url) = nav_url {
                        let _ = window.navigate(target_url);
                    }
                    crate::app::invoke::set_gmail_low_memory(app, false);
                    let _ = window.show();
                    let _ = window.unminimize();
                    crate::app::invoke::reapply_current_badge(app);
                    reapply_window_icon(&window);
                    
                    #[cfg(target_os = "windows")]
                    {
                        // Break Windows foreground lock: pulse always_on_top with a delay
                        let _ = window.set_always_on_top(true);
                        let _ = window.set_focus();
                        
                        let window_clone = window.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(300));
                            let _ = window_clone.set_always_on_top(false);
                        });

                        if is_toast_activation {
                            crate::app::notification::clear_all(app);
                            crate::app::invoke::clear_notify_badge(app);
                            let _ = window.eval("window.__pakeOnToastActivated?.()");
                        }
                    }
                    
                    #[cfg(not(target_os = "windows"))]
                    {
                        #[cfg(target_os = "macos")]
                        enter_configured_fullscreen(&window);
                        let _ = window.set_focus();
                    }
                }
            },
        ));
    }

    // Reveal hidden windows after the first real document finishes loading so
    // slow WKWebView cold starts do not expose an empty but interactive shell.
    {
        let page_load_revealed = startup_window_revealed.clone();
        app_builder = app_builder.on_page_load(move |webview, payload| {
            if !matches!(payload.event(), PageLoadEvent::Finished) {
                return;
            }
            if is_placeholder_startup_url(payload.url()) {
                return;
            }

            let label = webview.label();
            if label == "pake" {
                if start_to_tray {
                    return;
                }
                if let Some(window) = webview.app_handle().get_webview_window("pake") {
                    reveal_startup_window(window, init_fullscreen, &page_load_revealed);
                }
                return;
            }

            if label.starts_with("pake-") {
                if let Some(window) = webview.app_handle().get_webview_window(label) {
                    reveal_built_window(&window);
                }
            }
        });
    }

    // Clone before setup moves the Arc into tray / shortcut / fallback handlers.
    let close_revealed = startup_window_revealed.clone();
    #[cfg(target_os = "macos")]
    let reopen_revealed = startup_window_revealed.clone();

    app_builder
        .invoke_handler(tauri::generate_handler![
            download_file,
            send_notification,
            close_notification,
            increment_dock_badge,
            set_dock_badge,
            set_dock_badge_label,
            clear_dock_badge,
            update_theme_mode,
            set_zoom,
            webview_navigate,
            set_title_bar_color,
            quit_app,
            reveal_download,
        ])
        .setup(move |app| {
            // Grant remote IPC only to the immutable, packaged entry origins,
            // before any webview can start loading untrusted content.
            if let Some(capability) = pake_config.remote_capability()? {
                app.add_capability(capability)?;
            }
            app.manage(crate::util::LastDownload::default());
            app.manage(MultiWindowState::new(
                pake_config.clone(),
                tauri_config.clone(),
            ));

            // Main thread: installs the native notification-click delegate so a
            // click can be routed back to the page that raised the notification.
            app::notification::init_native_click(app.app_handle());

            // --- Menu Construction Start ---
            #[cfg(target_os = "macos")]
            {
                app::menu::set_app_menu(app.app_handle(), multi_window, _enable_find)?;

                // Event Handling for Custom Menu Item
                app.on_menu_event(move |app_handle, event| {
                    app::menu::handle_menu_click(app_handle, event.id().as_ref());
                });
            }
            // --- Menu Construction End ---

            let window = set_window(app.app_handle(), &pake_config, &tauri_config)?;

            if let Some(startup_url) = std::env::args()
                .skip(1)
                .find(|arg| arg.starts_with("http://") || arg.starts_with("https://"))
                .and_then(|arg| tauri::Url::parse(&arg).ok())
            {
                let _ = window.navigate(startup_url);
                reveal_startup_window(window.clone(), init_fullscreen, &startup_window_revealed);
            }

            #[cfg(target_os = "windows")]
            if std::env::args().any(|arg| arg.to_ascii_lowercase().contains("toastactivated")) {
                crate::app::notification::clear_all(app.app_handle());
                crate::app::invoke::clear_notify_badge(app.app_handle());
                let _ = window.eval("window.__pakeOnToastActivated?.()");
            }

            // An explicit --dark-mode wins over the desktop preference.
            #[cfg(target_os = "linux")]
            if !pake_config.windows[0].dark_mode {
                app::color_scheme::follow_desktop_color_scheme(app.app_handle());
            }

            set_system_tray(
                app.app_handle(),
                show_system_tray,
                &pake_config.system_tray_path,
                init_fullscreen,
                multi_window,
                startup_window_revealed.clone(),
            )?;
            set_global_shortcut(
                app.app_handle(),
                activation_shortcut,
                init_fullscreen,
                startup_window_revealed.clone(),
            )?;

            if is_calendar_app {
                let cal_app = app.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(25)).await;
                    loop {
                        crate::app::invoke::trigger_calendar_export(&cal_app, true);
                        tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
                    }
                });
            }

            // Show window after state restoration to prevent position flashing
            // once its first page finishes. A fallback keeps offline or stalled
            // pages reachable without exposing a blank webview during normal startup.
            if !start_to_tray {
                let window_clone = window.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        STARTUP_WINDOW_FALLBACK_DELAY,
                    ))
                    .await;
                    reveal_startup_window(window_clone, init_fullscreen, &startup_window_revealed);
                });
            } else {
                // Tray/shortcut already hold clones that cancel user-driven toggles.
                drop(startup_window_revealed);
            }

            Ok(())
        })
        .on_window_event(move |_window, _event| {
            if _window.label() == "pake" {
                match _event {
                    tauri::WindowEvent::Resized(_) => {
                        if _window.is_minimized().unwrap_or(false) {
                            crate::app::invoke::set_gmail_low_memory(_window.app_handle(), true);
                        } else if _window.is_visible().unwrap_or(false) {
                            crate::app::invoke::set_gmail_low_memory(_window.app_handle(), false);
                            crate::app::invoke::reapply_current_badge(_window.app_handle());
                        }
                    }
                    tauri::WindowEvent::Focused(true) => {
                        crate::app::invoke::set_gmail_low_memory(_window.app_handle(), false);
                        crate::app::invoke::reapply_current_badge(_window.app_handle());
                    }
                    tauri::WindowEvent::Focused(false) => {
                        if is_calendar_app {
                            crate::app::invoke::trigger_calendar_export(_window.app_handle(), false);
                        }
                    }
                    _ => {}
                }
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = _event {
                if is_calendar_app && _window.label() == "pake" {
                    crate::app::invoke::trigger_calendar_export(_window.app_handle(), false);
                }
                if remember_url && _window.label() == "pake" {
                    save_last_url(_window.app_handle());
                }
                if hide_on_close && _window.label() == "pake" {
                    // User dismissed the window; clear any transient notification badge
                    // while keeping any real unread message count from the page title.
                    crate::app::invoke::clear_notify_badge(_window.app_handle());
                    // User dismissed the window; do not let startup reveal reopen it.
                    cancel_startup_reveal(&close_revealed);
                    // Hide window when hide_on_close is enabled (regardless of tray status)
                    let window = _window.clone();
                    tauri::async_runtime::spawn(async move {
                        #[cfg(target_os = "macos")]
                        {
                            if window.is_fullscreen().unwrap_or(false) {
                                let _ = window.set_fullscreen(false);
                                tokio::time::sleep(Duration::from_millis(900)).await;
                            }
                        }
                        #[cfg(target_os = "linux")]
                        {
                            if window.is_fullscreen().unwrap_or(false) {
                                let _ = window.set_fullscreen(false);
                                // Restore focus after exiting fullscreen to fix input issues
                                let _ = window.set_focus();
                            }
                        }
                        // On macOS, directly hide without minimize to avoid duplicate Dock icons
                        #[cfg(not(target_os = "macos"))]
                        let _ = window.minimize();
                        let _ = window.hide();
                        crate::app::invoke::set_gmail_low_memory(window.app_handle(), true);
                    });
                    api.prevent_close();
                }
                // If hide_on_close is false, allow normal close behavior
                // This lets tauri-plugin-window-state save the window position and size
            }
        })
        .build(context)
        .unwrap_or_else(|error| {
            eprintln!("[Pake] Fatal error while building Tauri application: {error}");
            std::process::exit(1);
        })
        .run(move |_app, _event| {
            if remember_url && matches!(&_event, tauri::RunEvent::Exit) {
                save_last_url(_app);
            }
            // Handle macOS dock icon click to reopen hidden window
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = _event
            {
                if !has_visible_windows {
                    if let Some(window) = _app.get_webview_window("pake") {
                        cancel_startup_reveal(&reopen_revealed);
                        let _ = window.show();
                        reapply_window_icon(&window);
                        #[cfg(target_os = "macos")]
                        enter_configured_fullscreen(&window);
                        let _ = window.set_focus();
                    }
                }
            }
        });
}

pub fn run() {
    run_app()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_startup_urls_cover_about_blank() {
        let blank: Url = "about:blank".parse().unwrap();
        let srcdoc: Url = "about:srcdoc".parse().unwrap();
        let https: Url = "https://github.com/".parse().unwrap();
        let tauri: Url = "tauri://localhost/".parse().unwrap();

        assert!(is_placeholder_startup_url(&blank));
        assert!(is_placeholder_startup_url(&srcdoc));
        assert!(!is_placeholder_startup_url(&https));
        assert!(!is_placeholder_startup_url(&tauri));
    }

    #[test]
    fn first_claim_wins_startup_reveal() {
        let revealed = AtomicBool::new(false);
        assert!(claim_startup_reveal(&revealed));
        assert!(!claim_startup_reveal(&revealed));
    }

    #[test]
    fn user_show_then_hide_blocks_automatic_startup_reveal() {
        let revealed = AtomicBool::new(false);
        cancel_startup_reveal(&revealed);
        cancel_startup_reveal(&revealed);
        assert!(
            !claim_startup_reveal(&revealed),
            "automatic reveal must stay cancelled after user visibility control"
        );
    }

    #[test]
    fn cancel_before_any_claim_blocks_reveal() {
        let revealed = AtomicBool::new(false);
        cancel_startup_reveal(&revealed);
        assert!(!claim_startup_reveal(&revealed));
    }

    #[test]
    fn dmabuf_renderer_stays_disabled_outside_niri() {
        let none = [None, None, None];
        assert!(should_disable_dmabuf_renderer(None, None, &none));
        assert!(should_disable_dmabuf_renderer(Some("1"), None, &none));
    }

    #[test]
    fn dmabuf_renderer_can_be_re_enabled_explicitly() {
        let none = [None, None, None];
        for value in ["0", "false", "off", "no", "native", "disabled"] {
            assert!(
                !should_disable_dmabuf_renderer(Some(value), None, &none),
                "expected {value} to restore the dmabuf renderer"
            );
        }
    }

    #[test]
    fn dmabuf_renderer_is_kept_for_niri_socket() {
        assert!(!should_disable_dmabuf_renderer(
            None,
            Some("/run/user/501/niri.sock"),
            &[None, None, None]
        ));
    }

    #[test]
    fn dmabuf_renderer_is_kept_for_niri_desktop() {
        assert!(!should_disable_dmabuf_renderer(
            None,
            None,
            &[Some("niri"), None, None]
        ));
    }

    #[test]
    fn dmabuf_renderer_can_be_forced_on_for_niri() {
        assert!(should_disable_dmabuf_renderer(
            Some("1"),
            Some("/run/user/501/niri.sock"),
            &[Some("niri"), None, None]
        ));
    }

    #[test]
    fn legacy_x11_keeps_dmabuf_disabled_and_skips_compositing() {
        let desktop = [Some("i3"), None, None];
        assert!(should_disable_dmabuf_renderer(None, None, &desktop));
        assert!(!should_disable_compositing_mode(
            None, None, None, None, &desktop
        ));
    }

    #[test]
    fn explicit_x11_gdk_backend_keeps_compositing_on_wayland() {
        assert!(!should_disable_compositing_mode(
            None,
            None,
            Some("wayland-0"),
            Some("x11"),
            &[None, None, None]
        ));
        assert!(should_disable_compositing_mode(
            None,
            None,
            Some("wayland-0"),
            Some("wayland"),
            &[None, None, None]
        ));
    }

    #[test]
    fn modern_x11_uses_shm_without_disabling_the_backing_store() {
        let desktop = [Some("i3"), None, None];
        assert!(should_use_shm_renderer(
            (2, 52),
            None,
            should_disable_dmabuf_renderer(None, None, &desktop),
        ));
        assert!(should_use_shm_renderer((2, 52), Some(" "), true));
    }

    #[test]
    fn modern_wayland_uses_shm_instead_of_disabling_compositing() {
        let desktop = [Some("GNOME"), None, None];
        assert!(should_disable_compositing_mode(
            None,
            None,
            Some("wayland-0"),
            None,
            &desktop
        ));
        assert!(should_use_shm_renderer(
            (2, 52),
            None,
            should_disable_dmabuf_renderer(None, None, &desktop),
        ));
    }

    #[test]
    fn shm_preserves_legacy_webkit_niri_and_explicit_modes() {
        assert!(!should_use_shm_renderer((2, 50), None, true));
        assert!(!should_use_shm_renderer((2, 38), None, true));
        let niri = [Some("niri"), None, None];
        assert!(!should_use_shm_renderer(
            (2, 52),
            None,
            should_disable_dmabuf_renderer(None, None, &niri),
        ));
        for mode in ["0", "false", "1", "true"] {
            assert!(!should_use_shm_renderer((2, 52), Some(mode), true));
        }
    }

    #[test]
    fn compositing_mode_stays_disabled_by_default_on_wayland() {
        assert!(should_disable_compositing_mode(
            None,
            None,
            Some("wayland-0"),
            None,
            &[None, None, None]
        ));
    }

    #[test]
    fn compositing_mode_can_be_forced_on_x11() {
        assert!(should_disable_compositing_mode(
            Some("1"),
            None,
            None,
            None,
            &[Some("i3"), None, None]
        ));
    }

    #[test]
    fn compositing_mode_is_kept_for_niri_socket() {
        assert!(!should_disable_compositing_mode(
            None,
            Some("/run/user/501/niri.sock"),
            Some("wayland-0"),
            None,
            &[None, None, None]
        ));
    }

    #[test]
    fn compositing_mode_is_kept_for_niri_desktop() {
        assert!(!should_disable_compositing_mode(
            None,
            None,
            Some("wayland-0"),
            None,
            &[Some("niri"), None, None]
        ));
    }

    #[test]
    fn compositing_mode_can_be_forced_on_for_niri() {
        assert!(should_disable_compositing_mode(
            Some("1"),
            Some("/run/user/501/niri.sock"),
            Some("wayland-0"),
            None,
            &[Some("niri"), None, None]
        ));
    }

    #[test]
    fn compositing_mode_can_be_disabled_explicitly() {
        for value in ["0", "false", "off", "no", "native", "disabled"] {
            assert!(
                !should_disable_compositing_mode(
                    Some(value),
                    None,
                    Some("wayland-0"),
                    None,
                    &[None, None, None]
                ),
                "expected {value} to restore compositing"
            );
        }
    }

    #[test]
    fn forces_wayland_backend_on_pure_wayland() {
        assert!(should_force_wayland_gdk_backend(
            None,
            Some("wayland-0"),
            None
        ));
    }

    #[test]
    fn forces_wayland_backend_when_display_is_blank() {
        assert!(should_force_wayland_gdk_backend(
            None,
            Some("wayland-0"),
            Some("   ")
        ));
    }

    #[test]
    fn keeps_default_backend_when_x11_display_present() {
        assert!(!should_force_wayland_gdk_backend(
            None,
            Some("wayland-0"),
            Some(":0")
        ));
    }

    #[test]
    fn keeps_default_backend_without_wayland_display() {
        assert!(!should_force_wayland_gdk_backend(None, None, None));
    }

    #[test]
    fn respects_explicit_gdk_backend_override() {
        assert!(!should_force_wayland_gdk_backend(
            Some("x11"),
            Some("wayland-0"),
            None
        ));
    }
}
