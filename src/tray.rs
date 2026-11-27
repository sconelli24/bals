use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use tracing::{info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIIF_WARNING, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, Shell_NotifyIconW,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HICON,
    IDI_APPLICATION, LoadIconW, MF_BYCOMMAND, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, MSG, PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOWNORMAL,
    SetForegroundWindow, SetMenuDefaultItem, SetTimer, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_NULL,
    WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::config::{
    AppConfig, AudioCaptureMode, CaptureMode, FrameRate, QualityPreset, ReplayDuration, Resolution,
};
use crate::engine::{EngineCommand, EngineEvent, EngineStatus, RecordingState};
use crate::hotkey::{display_hotkey, parse_hotkey};
use crate::ipc::{
    TRAY_WINDOW_CLASS, WM_BALS_CHECK_UPDATE, WM_BALS_PAUSE_HOTKEY, WM_BALS_RELOAD_CONFIG,
    WM_BALS_RESUME_HOTKEY, WM_BALS_SAVE_CLIP, WM_BALS_SHUTDOWN, WM_BALS_TOGGLE_RECORD,
    close_ui_window, spawn_or_focus_ui,
};

const WM_TRAY_ICON: u32 = WM_APP + 1;
const HOTKEY_SAVE_CLIP_ID: i32 = 1;
const TIMER_EVENTS_ID: usize = 1;
const TRAY_ICON_UID: u32 = 1;

// IDs de comandos del menú contextual de la bandeja.
const IDM_OPEN_UI: usize = 1000;
const IDM_SAVE_CLIP: usize = 1001;
const IDM_TOGGLE_RECORD: usize = 1002;
const IDM_OPEN_LAST_CLIP: usize = 1004;
const IDM_MODE_AUTO: usize = 1005;
const IDM_MODE_MANUAL: usize = 1006;
const IDM_RES_720: usize = 1010;
const IDM_RES_1080: usize = 1011;
const IDM_RES_1440: usize = 1012;
const IDM_FPS_30: usize = 1020;
const IDM_FPS_60: usize = 1021;
const IDM_QUAL_COMPACT: usize = 1025;
const IDM_QUAL_BALANCED: usize = 1026;
const IDM_QUAL_HIGH: usize = 1027;
const IDM_DUR_15: usize = 1030;
const IDM_DUR_30: usize = 1031;
const IDM_DUR_60: usize = 1032;
const IDM_DUR_120: usize = 1033;
const IDM_TOGGLE_MIC: usize = 1040;
const IDM_AUDIO_DESKTOP: usize = 1041;
const IDM_AUDIO_GAME_ONLY: usize = 1042;
const IDM_TOGGLE_CAMERA: usize = 1043;
const IDM_TOGGLE_SOUND: usize = 1044;
const IDM_TOGGLE_NOTIFICATIONS: usize = 1045;
const IDM_OPEN_CLIPS: usize = 1050;
const IDM_OPEN_CONFIG: usize = 1051;
const IDM_CHECK_UPDATE: usize = 1052;
const IDM_EXIT: usize = 1099;
const IDM_MONITOR_BASE: usize = 1100;
const IDM_MONITOR_MAX: usize = 1115;

struct TrayState {
    hwnd: HWND,
    cfg: AppConfig,
    hotkey_registered: bool,
    hotkey_paused: bool,
    hotkey_resumed_at: Option<Instant>,
    cmd_tx: Sender<EngineCommand>,
    event_rx: Receiver<EngineEvent>,
    status: Arc<Mutex<EngineStatus>>,
    update_check_tx: Sender<()>,
    last_notified_clip: Option<PathBuf>,
    menu_monitors: Vec<String>,
    icon_idle: HICON,
    icon_rec: HICON,
    icon_paused: HICON,
    custom_icons: bool,
}

thread_local! {
    static TRAY_STATE: RefCell<Option<TrayState>> = const { RefCell::new(None) };
    static MENU_OPEN: Cell<bool> = const { Cell::new(false) };
}

/// Ejecuta el bucle nativo Win32 de la bandeja del sistema + Hotkey global (`\`).
/// Consume ~1 MB de RAM y 0% CPU en reposo.
pub fn run_tray_loop(
    cfg: AppConfig,
    cmd_tx: Sender<EngineCommand>,
    event_rx: Receiver<EngineEvent>,
    status: Arc<Mutex<EngineStatus>>,
    update_check_tx: Sender<()>,
) -> Result<()> {
    let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW falló")?;
    let class_name = TRAY_WINDOW_CLASS;

    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };

    unsafe { RegisterClassW(&wc) };

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("BalsTrayDaemon"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .context("CreateWindowExW (Bals Tray) falló")?;

    // Registrar atajo global (por defecto `\`).
    let hotkey_registered = register_save_hotkey(hwnd, &cfg.save_clip_hotkey).is_ok();

    // Crear iconos programáticos (espera, grabando rojo, pausa Alt-Tab ámbar) sin archivos .ico externos.
    let (icon_idle, icon_rec, icon_paused, custom_icons) = create_status_icons();

    // Añadir icono a la bandeja del sistema.
    add_tray_icon(hwnd, icon_idle, &cfg);

    // Timer ligero cada 200 ms para drenar eventos del motor (notificaciones de clip guardado).
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_EVENTS_ID, 200, None);
    }

    TRAY_STATE.with(|slot| {
        *slot.borrow_mut() = Some(TrayState {
            hwnd,
            cfg: cfg.clone(),
            hotkey_registered,
            hotkey_paused: false,
            hotkey_resumed_at: None,
            cmd_tx,
            event_rx,
            status,
            update_check_tx,
            last_notified_clip: None,
            menu_monitors: Vec::new(),
            icon_idle,
            icon_rec,
            icon_paused,
            custom_icons,
        });
    });

    info!(
        hotkey = %cfg.save_clip_hotkey,
        clips_dir = %cfg.clips_dir.display(),
        "Bals ejecutándose en la bandeja del sistema (clic derecho en el icono para opciones)"
    );

    // Bucle estándar de mensajes Win32.
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Limpieza ordenada.
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_SAVE_CLIP_ID);
    }
    remove_tray_icon(hwnd);

    TRAY_STATE.with(|slot| {
        if let Some(st) = slot.borrow_mut().take() {
            if st.custom_icons {
                unsafe {
                    let _ = DestroyIcon(st.icon_idle);
                    let _ = DestroyIcon(st.icon_rec);
                    let _ = DestroyIcon(st.icon_paused);
                }
            }
        }
    });

    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_SAVE_CLIP_ID => {
            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow().as_ref() {
                    if st.hotkey_paused {
                        return;
                    }
                    if st
                        .hotkey_resumed_at
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(220))
                    {
                        return;
                    }
                    info!(hotkey = %st.cfg.save_clip_hotkey, "Hotkey presionada -> Guardando clip...");
                    let _ = st.cmd_tx.send(EngineCommand::SaveClip);
                }
            });
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_EVENTS_ID => {
            process_engine_events();
            LRESULT(0)
        }
        WM_TRAY_ICON => {
            let event = (lparam.0 & 0xFFFF) as u32;
            match event {
                WM_RBUTTONUP => show_context_menu(hwnd),
                WM_LBUTTONDBLCLK => {
                    spawn_or_focus_ui();
                    TRAY_STATE.with(|slot| {
                        if let Some(st) = slot.borrow().as_ref() {
                            let _ = st.update_check_tx.try_send(());
                        }
                    });
                }
                NIN_BALLOONUSERCLICK => {
                    let clip_to_open = TRAY_STATE.with(|slot| {
                        slot.borrow()
                            .as_ref()
                            .and_then(|st| st.last_notified_clip.clone())
                    });
                    if let Some(path) = clip_to_open {
                        if path.exists() {
                            open_in_shell(&path);
                        }
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_BALS_RELOAD_CONFIG => {
            let mut deferred_balloon: Option<(String, String)> = None;
            let mut deferred_icon: Option<(HICON, String)> = None;

            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow_mut().as_mut() {
                    let mut new_cfg = AppConfig::try_load().unwrap_or_else(|| st.cfg.clone());
                    let prev_hotkey = st.cfg.save_clip_hotkey.clone();
                    let hotkey_changed = new_cfg.save_clip_hotkey != prev_hotkey;
                    let was_paused = st.hotkey_paused;
                    st.hotkey_paused = false;

                    if hotkey_changed || !st.hotkey_registered || was_paused {
                        unsafe {
                            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_SAVE_CLIP_ID);
                        }
                        match register_save_hotkey(hwnd, &new_cfg.save_clip_hotkey) {
                            Ok(()) => {
                                st.hotkey_registered = true;
                            }
                            Err(err) => {
                                // Si falló registrar la nueva tecla, restaurar la anterior para no dejar sin atajo.
                                warn!(
                                    failed_hotkey = %new_cfg.save_clip_hotkey,
                                    fallback_hotkey = %prev_hotkey,
                                    "No se pudo registrar el nuevo atajo ({err:#}); restaurando atajo anterior"
                                );
                                st.hotkey_registered =
                                    register_save_hotkey(hwnd, &prev_hotkey).is_ok();
                                let failed_spec = display_hotkey(&new_cfg.save_clip_hotkey);
                                new_cfg.save_clip_hotkey = prev_hotkey;
                                let _ = new_cfg.save();
                                deferred_balloon = Some((
                                    "Bals — Atajo en uso".to_string(),
                                    format!(
                                        "No se pudo registrar [{failed_spec}] (en uso por otra app). Se mantuvo [{}].",
                                        display_hotkey(&new_cfg.save_clip_hotkey)
                                    ),
                                ));
                            }
                        }
                    }

                    st.cfg = new_cfg.clone();
                    let cur_status = st.status.lock().map(|g| g.clone()).unwrap_or_default();
                    deferred_icon = Some(build_tray_icon_update(st, &cur_status));
                    let _ = st.cmd_tx.send(EngineCommand::UpdateConfig {
                        config: new_cfg,
                        persist_to_disk: false,
                    });
                }
            });

            if let Some((icon, tip)) = deferred_icon {
                update_tray_icon(hwnd, icon, &tip);
            }
            if let Some((title, body)) = deferred_balloon {
                show_balloon(hwnd, &title, &body, true, false);
            }
            LRESULT(0)
        }
        WM_BALS_TOGGLE_RECORD => {
            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow().as_ref() {
                    let _ = st.cmd_tx.send(EngineCommand::ToggleManual);
                }
            });
            LRESULT(0)
        }
        WM_BALS_SAVE_CLIP => {
            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow().as_ref() {
                    let _ = st.cmd_tx.send(EngineCommand::SaveClip);
                }
            });
            LRESULT(0)
        }
        WM_BALS_SHUTDOWN => {
            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow().as_ref() {
                    let _ = st.cmd_tx.send(EngineCommand::Shutdown);
                }
            });
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_BALS_CHECK_UPDATE => {
            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow().as_ref() {
                    let _ = st.update_check_tx.try_send(());
                }
            });
            LRESULT(0)
        }
        WM_BALS_PAUSE_HOTKEY => {
            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow_mut().as_mut() {
                    if !st.hotkey_paused {
                        unsafe {
                            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_SAVE_CLIP_ID);
                        }
                        st.hotkey_paused = true;
                        st.hotkey_registered = false;
                    }
                }
            });
            LRESULT(0)
        }
        WM_BALS_RESUME_HOTKEY => {
            TRAY_STATE.with(|slot| {
                if let Some(st) = slot.borrow_mut().as_mut() {
                    if st.hotkey_paused {
                        st.hotkey_paused = false;
                        st.hotkey_registered =
                            register_save_hotkey(hwnd, &st.cfg.save_clip_hotkey).is_ok();
                        st.hotkey_resumed_at = Some(Instant::now());
                    }
                }
            });
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd_id = wparam.0 & 0xFFFF;
            handle_menu_command(hwnd, cmd_id);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

enum DeferredTrayAction {
    PlayClipSound,
    ShowBalloon {
        hwnd: HWND,
        title: String,
        body: String,
        is_warning: bool,
        silent: bool,
    },
    UpdateIcon {
        hwnd: HWND,
        icon: HICON,
        tip: String,
    },
}

fn process_engine_events() {
    let mut actions: Vec<DeferredTrayAction> = Vec::new();

    TRAY_STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(st) = borrow.as_mut() else {
            return;
        };

        while let Ok(ev) = st.event_rx.try_recv() {
            match ev {
                EngineEvent::ClipSaved(stats) => {
                    if st.cfg.sound_feedback {
                        actions.push(DeferredTrayAction::PlayClipSound);
                    }
                    st.last_notified_clip = Some(stats.path.clone());
                    if st.cfg.show_notifications {
                        let mb = stats.file_bytes as f64 / (1024.0 * 1024.0);
                        let filename = stats
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "clip.mp4".to_string());
                        actions.push(DeferredTrayAction::ShowBalloon {
                            hwnd: st.hwnd,
                            title: "Bals — Clip guardado".to_string(),
                            body: format!(
                                "{filename} ({:.1} s · {:.1} MB)\nClic para abrir el clip",
                                stats.clip_duration_secs, mb
                            ),
                            is_warning: false,
                            silent: true,
                        });
                    }
                }
                EngineEvent::ClipFailed(reason) => {
                    st.last_notified_clip = None;
                    if st.cfg.show_notifications {
                        actions.push(DeferredTrayAction::ShowBalloon {
                            hwnd: st.hwnd,
                            title: "Bals".to_string(),
                            body: reason,
                            is_warning: true,
                            silent: false,
                        });
                    }
                }
                EngineEvent::StatusChanged(status) => {
                    let (icon, tip) = build_tray_icon_update(st, &status);
                    actions.push(DeferredTrayAction::UpdateIcon {
                        hwnd: st.hwnd,
                        icon,
                        tip,
                    });
                }
            }
        }
    });

    // Ejecutar llamadas Shell/Win32 ya fuera del préstamo de `TRAY_STATE` para evitar re-entrancia.
    for action in actions {
        match action {
            DeferredTrayAction::PlayClipSound => crate::audio::play_clip_saved_sound(),
            DeferredTrayAction::ShowBalloon {
                hwnd,
                title,
                body,
                is_warning,
                silent,
            } => show_balloon(hwnd, &title, &body, is_warning, silent),
            DeferredTrayAction::UpdateIcon { hwnd, icon, tip } => {
                update_tray_icon(hwnd, icon, &tip);
            }
        }
    }
}

fn handle_menu_command(hwnd: HWND, cmd_id: usize) {
    let mut should_open_ui = false;
    let mut path_to_open: Option<PathBuf> = None;
    let mut check_update_balloon = false;
    let mut should_exit = false;

    TRAY_STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(st) = borrow.as_mut() else {
            return;
        };

        let mut config_changed = false;

        match cmd_id {
            IDM_OPEN_UI => {
                should_open_ui = true;
                let _ = st.update_check_tx.try_send(());
            }
            IDM_SAVE_CLIP => {
                let _ = st.cmd_tx.send(EngineCommand::SaveClip);
            }
            IDM_TOGGLE_RECORD => {
                let _ = st.cmd_tx.send(EngineCommand::ToggleManual);
            }
            IDM_OPEN_LAST_CLIP => {
                let candidate = st.last_notified_clip.clone().or_else(|| {
                    st.status
                        .lock()
                        .ok()
                        .and_then(|g| g.last_clip.clone())
                });
                if let Some(p) = candidate.filter(|p| p.exists()) {
                    path_to_open = Some(p);
                }
            }
            IDM_MODE_AUTO => {
                if st.cfg.capture_mode != CaptureMode::AutoDetectGames {
                    st.cfg.capture_mode = CaptureMode::AutoDetectGames;
                    config_changed = true;
                }
            }
            IDM_MODE_MANUAL => {
                if st.cfg.capture_mode != CaptureMode::ManualMonitor {
                    st.cfg.capture_mode = CaptureMode::ManualMonitor;
                    config_changed = true;
                }
            }
            IDM_MONITOR_BASE..=IDM_MONITOR_MAX => {
                let idx = cmd_id - IDM_MONITOR_BASE;
                if let Some(dev) = st.menu_monitors.get(idx).cloned() {
                    st.cfg.selected_monitor = Some(dev);
                    st.cfg.capture_mode = CaptureMode::ManualMonitor;
                    config_changed = true;
                }
            }
            IDM_RES_720 => {
                st.cfg.resolution = Resolution::P720;
                config_changed = true;
            }
            IDM_RES_1080 => {
                st.cfg.resolution = Resolution::P1080;
                config_changed = true;
            }
            IDM_RES_1440 => {
                st.cfg.resolution = Resolution::P1440;
                config_changed = true;
            }
            IDM_FPS_30 => {
                st.cfg.fps = FrameRate::Fps30;
                config_changed = true;
            }
            IDM_FPS_60 => {
                st.cfg.fps = FrameRate::Fps60;
                config_changed = true;
            }
            IDM_QUAL_COMPACT => {
                st.cfg.quality = QualityPreset::Compact;
                config_changed = true;
            }
            IDM_QUAL_BALANCED => {
                st.cfg.quality = QualityPreset::Balanced;
                config_changed = true;
            }
            IDM_QUAL_HIGH => {
                st.cfg.quality = QualityPreset::High;
                config_changed = true;
            }
            IDM_DUR_15 => {
                st.cfg.replay_duration = ReplayDuration::Sec15;
                config_changed = true;
            }
            IDM_DUR_30 => {
                st.cfg.replay_duration = ReplayDuration::Sec30;
                config_changed = true;
            }
            IDM_DUR_60 => {
                st.cfg.replay_duration = ReplayDuration::Sec60;
                config_changed = true;
            }
            IDM_DUR_120 => {
                st.cfg.replay_duration = ReplayDuration::Sec120;
                config_changed = true;
            }
            IDM_TOGGLE_MIC => {
                st.cfg.mic_enabled = !st.cfg.mic_enabled;
                config_changed = true;
            }
            IDM_TOGGLE_CAMERA => {
                st.cfg.camera_enabled = !st.cfg.camera_enabled;
                config_changed = true;
            }
            IDM_AUDIO_DESKTOP => {
                if st.cfg.audio_capture_mode != AudioCaptureMode::FullDesktop {
                    st.cfg.audio_capture_mode = AudioCaptureMode::FullDesktop;
                    config_changed = true;
                }
            }
            IDM_AUDIO_GAME_ONLY => {
                if st.cfg.audio_capture_mode != AudioCaptureMode::GameOnly {
                    st.cfg.audio_capture_mode = AudioCaptureMode::GameOnly;
                    config_changed = true;
                }
            }
            IDM_TOGGLE_SOUND => {
                st.cfg.sound_feedback = !st.cfg.sound_feedback;
                config_changed = true;
            }
            IDM_TOGGLE_NOTIFICATIONS => {
                st.cfg.show_notifications = !st.cfg.show_notifications;
                config_changed = true;
            }
            IDM_CHECK_UPDATE => {
                let _ = st.update_check_tx.try_send(());
                check_update_balloon = true;
            }
            IDM_OPEN_CLIPS => {
                let _ = std::fs::create_dir_all(&st.cfg.clips_dir);
                path_to_open = Some(st.cfg.clips_dir.clone());
            }
            IDM_OPEN_CONFIG => {
                let _ = st.cfg.save();
                path_to_open = Some(AppConfig::config_path());
            }
            IDM_EXIT => {
                let _ = st.cmd_tx.send(EngineCommand::Shutdown);
                should_exit = true;
            }
            _ => {}
        }

        if config_changed {
            let _ = st.cmd_tx.send(EngineCommand::UpdateConfig {
                config: st.cfg.clone(),
                persist_to_disk: true,
            });
        }
    });

    // Ejecutar llamadas Shell/Win32 fuera del préstamo de `TRAY_STATE`.
    if should_open_ui {
        spawn_or_focus_ui();
    }
    if let Some(path) = path_to_open {
        open_in_shell(&path);
    }
    if check_update_balloon {
        show_balloon(
            hwnd,
            "Bals",
            "Buscando actualizaciones en segundo plano...",
            false,
            true,
        );
    }
    if should_exit {
        close_ui_window();
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

struct MenuOpenGuard;

impl MenuOpenGuard {
    fn try_enter() -> Option<Self> {
        MENU_OPEN.with(|flag| {
            if flag.get() {
                None
            } else {
                flag.set(true);
                Some(Self)
            }
        })
    }
}

impl Drop for MenuOpenGuard {
    fn drop(&mut self) {
        MENU_OPEN.with(|flag| flag.set(false));
    }
}

fn show_context_menu(hwnd: HWND) {
    let Some(_guard) = MenuOpenGuard::try_enter() else {
        return;
    };

    let monitors = crate::capture::list_monitors();
    let monitor_devices: Vec<String> = monitors
        .iter()
        .take(16)
        .map(|m| m.device_name.clone())
        .collect();

    // Clonar solo el snapshot necesario y SOLTAR el préstamo de `TRAY_STATE` ANTES de
    // llamar a `TrackPopupMenu`, porque el menú modal ejecuta un bucle de mensajes en el mismo
    // hilo donde `WM_TIMER` / `WM_BALS_*` pueden re-entrar a `wnd_proc` y pedir `borrow_mut()`.
    let snapshot = TRAY_STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let st = borrow.as_mut()?;
        st.menu_monitors = monitor_devices;
        let status = st.status.lock().map(|g| g.clone()).unwrap_or_default();
        let last_clip = st
            .last_notified_clip
            .clone()
            .or_else(|| status.last_clip.clone())
            .filter(|p| p.exists());
        Some((st.cfg.clone(), status, last_clip))
    });

    let Some((cfg, status, last_clip)) = snapshot else {
        return;
    };

    let is_recording = !matches!(status.state, RecordingState::Idle);
    let hotkey_lbl = display_hotkey(&cfg.save_clip_hotkey);

    let selected_cmd = unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };

        let header = match &status.state {
            RecordingState::Idle => "○ Bals — En espera (0% GPU)".to_string(),
            RecordingState::RecordingManual { monitor_label } => format!(
                "● Grabando {monitor_label} ({:.0}s · {:.1} MB RAM)",
                status.buffered_secs, status.buffer_ram_mb
            ),
            RecordingState::RecordingGame {
                game_name,
                paused_alt_tab,
                ..
            } => {
                if *paused_alt_tab {
                    format!(
                        "⏸ {game_name} (Alt-Tab · {:.0}s · {:.1} MB en RAM)",
                        status.buffered_secs, status.buffer_ram_mb
                    )
                } else {
                    format!(
                        "● {game_name} ({:.0}s · {:.1} MB RAM)",
                        status.buffered_secs, status.buffer_ram_mb
                    )
                }
            }
        };
        append_text(menu, MF_STRING | MF_GRAYED, 0, &header);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        append_text(menu, MF_STRING, IDM_OPEN_UI, "Abrir Bals (Ajustes y Clips)");
        let _ = SetMenuDefaultItem(menu, IDM_OPEN_UI as u32, 0);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let save_label = format!(
            "Guardar últimos {}s\t[{hotkey_lbl}]",
            cfg.replay_duration.as_secs()
        );
        append_text(
            menu,
            if is_recording {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            },
            IDM_SAVE_CLIP,
            &save_label,
        );

        let toggle_label = match &status.state {
            RecordingState::Idle => "Iniciar Replay Buffer (Monitor)".to_string(),
            RecordingState::RecordingManual { .. } => "Pausar Replay Buffer".to_string(),
            RecordingState::RecordingGame { game_name, .. } => {
                format!("Detener grabación ({game_name})")
            }
        };
        append_text(menu, MF_STRING, IDM_TOGGLE_RECORD, &toggle_label);

        let last_clip_label = match &last_clip {
            Some(p) => {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "último clip".to_string());
                format!("Abrir último clip ({name})")
            }
            None => "Abrir último clip guardado".to_string(),
        };
        append_text(
            menu,
            if last_clip.is_some() {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            },
            IDM_OPEN_LAST_CLIP,
            &last_clip_label,
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        // Submenú Modo de captura + selección directa de monitor
        if let Ok(sub_mode) = CreatePopupMenu() {
            append_text(
                sub_mode,
                MF_STRING,
                IDM_MODE_AUTO,
                "Detectar juegos automáticamente (0% GPU)",
            );
            append_text(sub_mode, MF_STRING, IDM_MODE_MANUAL, "Grabar monitor completo");
            let active_mode_id = match cfg.capture_mode {
                CaptureMode::AutoDetectGames => IDM_MODE_AUTO,
                CaptureMode::ManualMonitor => IDM_MODE_MANUAL,
            };
            let _ = CheckMenuRadioItem(
                sub_mode,
                IDM_MODE_AUTO as u32,
                IDM_MODE_MANUAL as u32,
                active_mode_id as u32,
                MF_BYCOMMAND.0,
            );

            if !monitors.is_empty() {
                let _ = AppendMenuW(sub_mode, MF_SEPARATOR, 0, PCWSTR::null());
                append_text(sub_mode, MF_STRING | MF_GRAYED, 0, "Monitor para modo manual:");
                let mut selected_mon_id: Option<usize> = None;
                let max_mons = monitors.len().min(16);
                for (i, m) in monitors.iter().take(max_mons).enumerate() {
                    let id = IDM_MONITOR_BASE + i;
                    append_text(sub_mode, MF_STRING, id, &m.ui_label());
                    let is_sel = match cfg.selected_monitor.as_deref() {
                        Some(dev) => m.device_name.eq_ignore_ascii_case(dev),
                        None => m.is_primary || m.index == 1,
                    };
                    if is_sel {
                        selected_mon_id = Some(id);
                    }
                }
                if let Some(sel_id) = selected_mon_id {
                    let _ = CheckMenuRadioItem(
                        sub_mode,
                        IDM_MONITOR_BASE as u32,
                        (IDM_MONITOR_BASE + max_mons - 1) as u32,
                        sel_id as u32,
                        MF_BYCOMMAND.0,
                    );
                }
            }

            let mode_short = match cfg.capture_mode {
                CaptureMode::AutoDetectGames => "Juegos (Auto)",
                CaptureMode::ManualMonitor => "Monitor",
            };
            let label = HSTRING::from(format!("Modo de captura ({mode_short})"));
            let _ = AppendMenuW(menu, MF_POPUP, sub_mode.0 as usize, &label);
        }

        // Submenú Video (Resolución · FPS · Calidad) con radio-checks nativos
        if let Ok(sub_video) = CreatePopupMenu() {
            append_text(sub_video, MF_STRING | MF_GRAYED, 0, "Resolución");
            append_text(sub_video, MF_STRING, IDM_RES_720, "720p (1280×720) — Ultra liviano");
            append_text(sub_video, MF_STRING, IDM_RES_1080, "1080p (1920×1080) — Recomendado");
            append_text(sub_video, MF_STRING, IDM_RES_1440, "2K (2560×1440)");
            let active_res = match cfg.resolution {
                Resolution::P720 => IDM_RES_720,
                Resolution::P1080 => IDM_RES_1080,
                Resolution::P1440 => IDM_RES_1440,
            };
            let _ = CheckMenuRadioItem(
                sub_video,
                IDM_RES_720 as u32,
                IDM_RES_1440 as u32,
                active_res as u32,
                MF_BYCOMMAND.0,
            );

            let _ = AppendMenuW(sub_video, MF_SEPARATOR, 0, PCWSTR::null());
            append_text(sub_video, MF_STRING | MF_GRAYED, 0, "Cuadros por segundo");
            append_text(sub_video, MF_STRING, IDM_FPS_30, "30 fps");
            append_text(sub_video, MF_STRING, IDM_FPS_60, "60 fps");
            let active_fps = match cfg.fps {
                FrameRate::Fps30 => IDM_FPS_30,
                FrameRate::Fps60 => IDM_FPS_60,
            };
            let _ = CheckMenuRadioItem(
                sub_video,
                IDM_FPS_30 as u32,
                IDM_FPS_60 as u32,
                active_fps as u32,
                MF_BYCOMMAND.0,
            );

            let _ = AppendMenuW(sub_video, MF_SEPARATOR, 0, PCWSTR::null());
            append_text(sub_video, MF_STRING | MF_GRAYED, 0, "Calidad / Peso");
            append_text(sub_video, MF_STRING, IDM_QUAL_COMPACT, "Ahorro (~45% menos peso)");
            append_text(sub_video, MF_STRING, IDM_QUAL_BALANCED, "Normal (Recomendado)");
            append_text(sub_video, MF_STRING, IDM_QUAL_HIGH, "Alta (Máxima nitidez)");
            let active_q = match cfg.quality {
                QualityPreset::Compact => IDM_QUAL_COMPACT,
                QualityPreset::Balanced => IDM_QUAL_BALANCED,
                QualityPreset::High => IDM_QUAL_HIGH,
            };
            let _ = CheckMenuRadioItem(
                sub_video,
                IDM_QUAL_COMPACT as u32,
                IDM_QUAL_HIGH as u32,
                active_q as u32,
                MF_BYCOMMAND.0,
            );

            let res_short = match cfg.resolution {
                Resolution::P720 => "720p",
                Resolution::P1080 => "1080p",
                Resolution::P1440 => "2K",
            };
            let label = HSTRING::from(format!(
                "Video ({res_short} · {}fps · {})",
                cfg.fps.as_u32(),
                cfg.quality
            ));
            let _ = AppendMenuW(menu, MF_POPUP, sub_video.0 as usize, &label);
        }

        // Submenú Duración del clip
        if let Ok(sub_dur) = CreatePopupMenu() {
            append_text(sub_dur, MF_STRING, IDM_DUR_15, "Últimos 15 segundos");
            append_text(sub_dur, MF_STRING, IDM_DUR_30, "Últimos 30 segundos");
            append_text(sub_dur, MF_STRING, IDM_DUR_60, "Últimos 60 segundos");
            append_text(sub_dur, MF_STRING, IDM_DUR_120, "Últimos 120 segundos");
            let active_dur = match cfg.replay_duration {
                ReplayDuration::Sec15 => IDM_DUR_15,
                ReplayDuration::Sec30 => IDM_DUR_30,
                ReplayDuration::Sec60 => IDM_DUR_60,
                ReplayDuration::Sec120 => IDM_DUR_120,
            };
            let _ = CheckMenuRadioItem(
                sub_dur,
                IDM_DUR_15 as u32,
                IDM_DUR_120 as u32,
                active_dur as u32,
                MF_BYCOMMAND.0,
            );
            let label = HSTRING::from(format!("Duración del clip ({})", cfg.replay_duration));
            let _ = AppendMenuW(menu, MF_POPUP, sub_dur.0 as usize, &label);
        }

        // Submenú Audio y Cámara
        if let Ok(sub_av) = CreatePopupMenu() {
            append_text(sub_av, MF_STRING | MF_GRAYED, 0, "Fuente de audio");
            append_text(
                sub_av,
                MF_STRING,
                IDM_AUDIO_DESKTOP,
                "Todo el escritorio (Juego + Discord + Música)",
            );
            append_text(
                sub_av,
                MF_STRING,
                IDM_AUDIO_GAME_ONLY,
                "Solo el juego (ProcessLoopback)",
            );
            let active_audio = match cfg.audio_capture_mode {
                AudioCaptureMode::FullDesktop => IDM_AUDIO_DESKTOP,
                AudioCaptureMode::GameOnly => IDM_AUDIO_GAME_ONLY,
            };
            let _ = CheckMenuRadioItem(
                sub_av,
                IDM_AUDIO_DESKTOP as u32,
                IDM_AUDIO_GAME_ONLY as u32,
                active_audio as u32,
                MF_BYCOMMAND.0,
            );

            let _ = AppendMenuW(sub_av, MF_SEPARATOR, 0, PCWSTR::null());
            append_checked(sub_av, cfg.mic_enabled, IDM_TOGGLE_MIC, "Grabar micrófono");
            append_checked(sub_av, cfg.camera_enabled, IDM_TOGGLE_CAMERA, "Grabar cámara (webcam)");
            append_checked(sub_av, cfg.sound_feedback, IDM_TOGGLE_SOUND, "Sonido al guardar clip");

            let _ = AppendMenuW(menu, MF_POPUP, sub_av.0 as usize, w!("Audio y Cámara"));
        }

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        append_text(menu, MF_STRING, IDM_OPEN_CLIPS, "Abrir carpeta de clips");

        // Submenú Más opciones
        if let Ok(sub_more) = CreatePopupMenu() {
            append_checked(
                sub_more,
                cfg.show_notifications,
                IDM_TOGGLE_NOTIFICATIONS,
                "Mostrar notificaciones",
            );
            append_text(sub_more, MF_STRING, IDM_CHECK_UPDATE, "Buscar actualizaciones ahora");
            let _ = AppendMenuW(sub_more, MF_SEPARATOR, 0, PCWSTR::null());
            append_text(sub_more, MF_STRING, IDM_OPEN_CONFIG, "Editar config.toml");
            let _ = AppendMenuW(menu, MF_POPUP, sub_more.0 as usize, w!("Más opciones"));
        }

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        append_text(menu, MF_STRING, IDM_EXIT, "Salir de Bals");

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        )
        .0;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd
    };

    drop(_guard);
    if selected_cmd > 0 {
        handle_menu_command(hwnd, selected_cmd as usize);
    }
}

unsafe fn append_text(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    flags: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_FLAGS,
    id: usize,
    text: &str,
) {
    let wide = HSTRING::from(text);
    let _ = unsafe { AppendMenuW(menu, flags, id, &wide) };
}

unsafe fn append_checked(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    checked: bool,
    id: usize,
    text: &str,
) {
    let flags = if checked {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING | MF_UNCHECKED
    };
    unsafe { append_text(menu, flags, id, text) };
}

fn build_tray_icon_update(st: &TrayState, status: &EngineStatus) -> (HICON, String) {
    let icon = match &status.state {
        RecordingState::Idle => st.icon_idle,
        RecordingState::RecordingManual { .. } => st.icon_rec,
        RecordingState::RecordingGame {
            paused_alt_tab: true,
            ..
        } => st.icon_paused,
        RecordingState::RecordingGame {
            paused_alt_tab: false,
            ..
        } => st.icon_rec,
    };
    let hotkey_lbl = display_hotkey(&st.cfg.save_clip_hotkey);
    let tip = match &status.state {
        RecordingState::Idle => format!("Bals — En espera (tecla: {hotkey_lbl})"),
        RecordingState::RecordingManual { monitor_label } => format!(
            "Bals — Grabando {monitor_label} ({} {} | tecla: {hotkey_lbl})",
            st.cfg.resolution, st.cfg.fps
        ),
        RecordingState::RecordingGame {
            game_name,
            paused_alt_tab,
            ..
        } => {
            if *paused_alt_tab {
                format!(
                    "Bals — {game_name} en pausa (Alt-Tab · buffer intacto | tecla: {hotkey_lbl})"
                )
            } else {
                format!("Bals — Grabando {game_name} (tecla: {hotkey_lbl})")
            }
        }
    };
    (icon, tip)
}

fn register_save_hotkey(hwnd: HWND, spec: &str) -> Result<()> {
    let parsed = parse_hotkey(spec)?;
    unsafe { RegisterHotKey(Some(hwnd), HOTKEY_SAVE_CLIP_ID, parsed.modifiers, parsed.vk) }
        .with_context(|| {
            format!("RegisterHotKey rechazó '{spec}' (puede estar en uso por otra app)")
        })?;
    info!(
        hotkey = %spec,
        vk = format!("0x{:02X}", parsed.vk),
        "Hotkey global registrada"
    );
    Ok(())
}

fn add_tray_icon(hwnd: HWND, icon: HICON, cfg: &AppConfig) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_UID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_ICON,
        hIcon: icon,
        ..Default::default()
    };
    copy_utf16(
        &mut nid.szTip,
        &format!(
            "Bals — En espera (tecla: {})",
            display_hotkey(&cfg.save_clip_hotkey)
        ),
    );
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn update_tray_icon(hwnd: HWND, icon: HICON, tooltip: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_UID,
        uFlags: NIF_ICON | NIF_TIP,
        hIcon: icon,
        ..Default::default()
    };
    copy_utf16(&mut nid.szTip, tooltip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn show_balloon(hwnd: HWND, title: &str, body: &str, is_warning: bool, silent: bool) {
    let mut flags = if is_warning { NIIF_WARNING } else { NIIF_INFO };
    if silent {
        flags |= NIIF_NOSOUND;
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_UID,
        uFlags: NIF_INFO,
        dwInfoFlags: flags,
        ..Default::default()
    };
    copy_utf16(&mut nid.szInfoTitle, title);
    copy_utf16(&mut nid.szInfo, body);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_UID,
        ..Default::default()
    };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn open_in_shell(path: &Path) {
    let target = HSTRING::from(path.as_os_str());
    unsafe {
        ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

fn copy_utf16(dst: &mut [u16], src: &str) {
    dst.fill(0);
    let max_len = dst.len().saturating_sub(1);
    for (slot, ch) in dst.iter_mut().take(max_len).zip(src.encode_utf16()) {
        *slot = ch;
    }
}

/// Genera los iconos 32x32 de la bandeja desde el icono oficial de Bals embebido en el binario:
/// - `idle`: monitor CRT retro de Bals
/// - `rec`: mismo monitor CRT con insignia roja de grabación
/// - `paused`: mismo monitor CRT con insignia ámbar (Alt-Tab / buffer congelado)
fn create_status_icons() -> (HICON, HICON, HICON, bool) {
    let idle = build_tray_icon(None);
    let rec = build_tray_icon(Some((239, 68, 68)));
    let paused = build_tray_icon(Some((245, 158, 11)));
    match (idle, rec, paused) {
        (Ok(i), Ok(r), Ok(p)) => (i, r, p, true),
        _ => {
            let fallback = unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() };
            (fallback, fallback, fallback, false)
        }
    }
}

fn build_tray_icon(badge_rgb: Option<(u8, u8, u8)>) -> Result<HICON> {
    const SIZE: usize = 32;
    const RAW_RGBA: &[u8] = include_bytes!("../assets/icon_32.rgba");

    let mut color_bgra = vec![0u8; SIZE * SIZE * 4];
    let mut and_mask = vec![0xFFu8; SIZE * (SIZE / 8)];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = (y * SIZE + x) * 4;
            let mut r = RAW_RGBA[idx];
            let mut g = RAW_RGBA[idx + 1];
            let mut b = RAW_RGBA[idx + 2];
            let mut a = RAW_RGBA[idx + 3];

            if let Some((br, bg, bb)) = badge_rgb {
                // Insignia circular de estado (rojo = grabando, ámbar = Alt-Tab) abajo a la derecha.
                let dx = x as f32 - 24.5;
                let dy = y as f32 - 24.5;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist <= 6.8 {
                    if dist > 5.0 {
                        r = 250;
                        g = 250;
                        b = 250;
                        a = 255;
                    } else {
                        r = br;
                        g = bg;
                        b = bb;
                        a = 255;
                    }
                }
            }

            color_bgra[idx] = b;
            color_bgra[idx + 1] = g;
            color_bgra[idx + 2] = r;
            color_bgra[idx + 3] = a;

            if a > 20 {
                let mask_byte = y * (SIZE / 8) + (x / 8);
                let mask_bit = 7 - (x % 8);
                and_mask[mask_byte] &= !(1u8 << mask_bit);
            }
        }
    }

    let icon = unsafe {
        CreateIcon(
            None,
            SIZE as i32,
            SIZE as i32,
            1,
            32,
            and_mask.as_ptr(),
            color_bgra.as_ptr(),
        )
    }?;
    Ok(icon)
}
