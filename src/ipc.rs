use std::mem::size_of;
use std::process::Command;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::time::{Duration, Instant};

use tracing::warn;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LPARAM, WPARAM};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, PostMessageW, SW_RESTORE, SetForegroundWindow, ShowWindow, WM_APP, WM_CLOSE,
};
use windows::core::{PCWSTR, w};

pub const TRAY_WINDOW_CLASS: PCWSTR = w!("BalsTrayWindowClass");
pub const UI_WINDOW_TITLE: PCWSTR = w!("Bals");

/// Mensajes IPC ultraligeros (sin sockets ni discos) entre `--ui` y el daemon.
pub const WM_BALS_RELOAD_CONFIG: u32 = WM_APP + 10;
pub const WM_BALS_TOGGLE_RECORD: u32 = WM_APP + 11;
pub const WM_BALS_SAVE_CLIP: u32 = WM_APP + 12;
pub const WM_BALS_SHUTDOWN: u32 = WM_APP + 13;
pub const WM_BALS_CHECK_UPDATE: u32 = WM_APP + 14;
pub const WM_BALS_PAUSE_HOTKEY: u32 = WM_APP + 15;
pub const WM_BALS_RESUME_HOTKEY: u32 = WM_APP + 16;

const SHARED_TELEMETRY_NAME: PCWSTR = w!("Local\\BalsEngineTelemetryV1");
const TELEMETRY_MAGIC: u32 = 0x4241_4C53; // "BALS"
const TELEMETRY_VERSION: u32 = 1;

/// Estado compacto de grabación publicado por el daemon para el HUD de la UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TelemetryState {
    Idle = 0,
    RecordingManual = 1,
    RecordingGame = 2,
    PausedAltTab = 3,
}

impl TelemetryState {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::RecordingManual,
            2 => Self::RecordingGame,
            3 => Self::PausedAltTab,
            _ => Self::Idle,
        }
    }
}

/// Instantánea legible en la UI de la telemetría viva del motor en segundo plano.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineTelemetrySnapshot {
    pub state: TelemetryState,
    pub replay_target_secs: u32,
    pub buffered_secs: f32,
    pub buffer_ram_mb: f32,
    pub clip_event_seq: u32,
    pub clip_event_is_err: bool,
    pub game_name: String,
    pub encoder_label: String,
    pub last_event: String,
}

#[repr(C)]
struct SharedTelemetryBlock {
    magic: u32,
    version: u32,
    seq: AtomicU32,
    state: u32,
    replay_target_secs: u32,
    buffered_secs: f32,
    buffer_ram_mb: f32,
    clip_event_seq: u32,
    clip_event_is_err: u32,
    game_name_len: u32,
    encoder_label_len: u32,
    last_event_len: u32,
    game_name: [u8; 96],
    encoder_label: [u8; 64],
    last_event: [u8; 256],
}

#[derive(Debug, Clone, PartialEq)]
struct LastPublished {
    state: u32,
    replay_target_secs: u32,
    buffered_secs: f32,
    buffer_ram_mb: f32,
    clip_event_seq: u32,
    clip_event_is_err: u32,
    game_name: String,
    encoder_label: String,
    last_event: String,
}

/// Escritor de telemetría en memoria compartida Win32 (`Local\BalsEngineTelemetryV1`).
/// Cero I/O en disco y latencia <1 µs; deduplica escrituras para 0 overhead en reposo.
pub struct TelemetryWriter {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    ptr: NonNull<SharedTelemetryBlock>,
    last_published: Option<LastPublished>,
    clip_event_seq: u32,
    clip_event_is_err: bool,
    last_event: String,
}

unsafe impl Send for TelemetryWriter {}

impl TelemetryWriter {
    pub fn create() -> Option<Self> {
        Self::create_named(SHARED_TELEMETRY_NAME)
    }

    fn create_named(name: PCWSTR) -> Option<Self> {
        let size = size_of::<SharedTelemetryBlock>() as u32;
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                size,
                name,
            )
        }
        .ok()?;
        if handle.is_invalid() {
            return None;
        }

        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size as usize) };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }

        let Some(ptr) = NonNull::new(view.Value.cast::<SharedTelemetryBlock>()) else {
            unsafe {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(handle);
            }
            return None;
        };

        unsafe {
            std::ptr::write_bytes(ptr.as_ptr(), 0, 1);
            let block = ptr.as_ptr();
            (*block).magic = TELEMETRY_MAGIC;
            (*block).version = TELEMETRY_VERSION;
            (*block).seq = AtomicU32::new(0);
        }

        Some(Self {
            handle,
            view,
            ptr,
            last_published: None,
            clip_event_seq: 0,
            clip_event_is_err: false,
            last_event: String::new(),
        })
    }

    pub fn record_clip_event(&mut self, msg: &str, is_err: bool) {
        self.clip_event_seq = self.clip_event_seq.wrapping_add(1).max(1);
        self.clip_event_is_err = is_err;
        self.last_event.clear();
        self.last_event.push_str(msg);
    }

    pub fn publish(
        &mut self,
        state: TelemetryState,
        replay_target_secs: u32,
        buffered_secs: f32,
        buffer_ram_mb: f32,
        game_name: &str,
        encoder_label: &str,
    ) {
        // Redondear a 1 decimal para evitar escrituras redundantes por ruido de flotantes.
        let buffered_rounded = (buffered_secs * 10.0).round() / 10.0;
        let ram_rounded = (buffer_ram_mb * 10.0).round() / 10.0;

        let candidate = LastPublished {
            state: state as u32,
            replay_target_secs,
            buffered_secs: buffered_rounded,
            buffer_ram_mb: ram_rounded,
            clip_event_seq: self.clip_event_seq,
            clip_event_is_err: u32::from(self.clip_event_is_err),
            game_name: game_name.to_string(),
            encoder_label: encoder_label.to_string(),
            last_event: self.last_event.clone(),
        };

        if self.last_published.as_ref() == Some(&candidate) {
            return;
        }

        unsafe {
            let block = &mut *self.ptr.as_ptr();
            let cur_seq = block.seq.load(Ordering::Relaxed);
            // SeqLock: impar mientras escribimos para que lectores reintenten si coinciden.
            block.seq.store(cur_seq.wrapping_add(1), Ordering::Release);
            fence(Ordering::Release);

            block.magic = TELEMETRY_MAGIC;
            block.version = TELEMETRY_VERSION;
            block.state = candidate.state;
            block.replay_target_secs = candidate.replay_target_secs;
            block.buffered_secs = candidate.buffered_secs;
            block.buffer_ram_mb = candidate.buffer_ram_mb;
            block.clip_event_seq = candidate.clip_event_seq;
            block.clip_event_is_err = candidate.clip_event_is_err;
            block.game_name_len = copy_utf8_truncated(&mut block.game_name, &candidate.game_name);
            block.encoder_label_len =
                copy_utf8_truncated(&mut block.encoder_label, &candidate.encoder_label);
            block.last_event_len =
                copy_utf8_truncated(&mut block.last_event, &candidate.last_event);

            block.seq.store(cur_seq.wrapping_add(2), Ordering::Release);
        }

        self.last_published = Some(candidate);
    }
}

impl Drop for TelemetryWriter {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

struct MappedReadGuard {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Drop for MappedReadGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Lee la telemetría viva publicada por el daemon desde memoria compartida (0 I/O en disco).
pub fn read_engine_telemetry() -> Option<EngineTelemetrySnapshot> {
    read_telemetry_named(SHARED_TELEMETRY_NAME)
}

fn read_telemetry_named(name: PCWSTR) -> Option<EngineTelemetrySnapshot> {
    let handle = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, name) }.ok()?;
    if handle.is_invalid() {
        return None;
    }

    let size = size_of::<SharedTelemetryBlock>();
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, size) };
    if view.Value.is_null() {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return None;
    }

    let _guard = MappedReadGuard { handle, view };
    let block_ptr = view.Value.cast::<SharedTelemetryBlock>();

    for _ in 0..4 {
        let s1 = unsafe { (*block_ptr).seq.load(Ordering::Acquire) };
        if (s1 & 1) != 0 {
            std::hint::spin_loop();
            continue;
        }
        fence(Ordering::Acquire);

        let (
            magic,
            version,
            state,
            replay_target_secs,
            buffered_secs,
            buffer_ram_mb,
            clip_event_seq,
            clip_event_is_err,
            game_len,
            enc_len,
            evt_len,
            game_buf,
            enc_buf,
            evt_buf,
        ) = unsafe {
            let b = &*block_ptr;
            (
                b.magic,
                b.version,
                b.state,
                b.replay_target_secs,
                b.buffered_secs,
                b.buffer_ram_mb,
                b.clip_event_seq,
                b.clip_event_is_err,
                b.game_name_len as usize,
                b.encoder_label_len as usize,
                b.last_event_len as usize,
                b.game_name,
                b.encoder_label,
                b.last_event,
            )
        };

        fence(Ordering::Acquire);
        let s2 = unsafe { (*block_ptr).seq.load(Ordering::Acquire) };
        if s1 != s2 {
            continue;
        }
        if magic != TELEMETRY_MAGIC || version != TELEMETRY_VERSION {
            return None;
        }

        let game_name = decode_utf8_slice(&game_buf, game_len);
        let encoder_label = decode_utf8_slice(&enc_buf, enc_len);
        let last_event = decode_utf8_slice(&evt_buf, evt_len);

        return Some(EngineTelemetrySnapshot {
            state: TelemetryState::from_u32(state),
            replay_target_secs,
            buffered_secs,
            buffer_ram_mb,
            clip_event_seq,
            clip_event_is_err: clip_event_is_err != 0,
            game_name,
            encoder_label,
            last_event,
        });
    }

    None
}

fn copy_utf8_truncated(dst: &mut [u8], s: &str) -> u32 {
    if dst.is_empty() {
        return 0;
    }
    let mut end = s.len().min(dst.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    dst[..end].copy_from_slice(&s.as_bytes()[..end]);
    end as u32
}

fn decode_utf8_slice(buf: &[u8], len: usize) -> String {
    let clamped = len.min(buf.len());
    String::from_utf8_lossy(&buf[..clamped]).into_owned()
}

/// Devuelve `true` si ya existe una instancia del daemon de Bals corriendo en la bandeja.
pub fn is_daemon_running() -> bool {
    unsafe { FindWindowW(TRAY_WINDOW_CLASS, PCWSTR::null()) }
        .map(|h| !h.is_invalid())
        .unwrap_or(false)
}

/// Avisa al daemon en segundo plano que recargue `%APPDATA%\Bals\config.toml`.
pub fn notify_reload_config() -> bool {
    post_to_daemon(WM_BALS_RELOAD_CONFIG)
}

/// Solicita al daemon iniciar o pausar el Replay Buffer manual.
pub fn notify_toggle_record() -> bool {
    post_to_daemon(WM_BALS_TOGGLE_RECORD)
}

/// Solicita al daemon guardar inmediatamente los últimos N segundos del buffer.
pub fn notify_save_clip() -> bool {
    post_to_daemon(WM_BALS_SAVE_CLIP)
}

/// Solicita al daemon cerrarse ordenadamente (usado al aplicar una actualización automática).
pub fn notify_shutdown() -> bool {
    post_to_daemon(WM_BALS_SHUTDOWN)
}

/// Solicita al daemon comprobar si hay una actualización disponible (al abrir la app).
pub fn notify_check_update() -> bool {
    post_to_daemon(WM_BALS_CHECK_UPDATE)
}

/// Pausa temporalmente la hotkey global mientras el usuario está asignando una tecla en la UI.
pub fn notify_pause_hotkey() -> bool {
    post_to_daemon(WM_BALS_PAUSE_HOTKEY)
}

/// Reanuda la hotkey global al terminar o cancelar la asignación de tecla en la UI.
pub fn notify_resume_hotkey() -> bool {
    post_to_daemon(WM_BALS_RESUME_HOTKEY)
}

/// Espera hasta que el daemon en segundo plano cierre su ventana de bandeja (o venza `timeout`).
pub fn wait_until_daemon_stopped(timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_daemon_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !is_daemon_running()
}

/// Devuelve `true` si la ventana de interfaz (`bals.exe --ui`) está abierta actualmente.
pub fn is_ui_open() -> bool {
    let tray_hwnd = unsafe { FindWindowW(TRAY_WINDOW_CLASS, PCWSTR::null()) }.ok();
    if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR::null(), UI_WINDOW_TITLE) } {
        return !hwnd.is_invalid() && Some(hwnd) != tray_hwnd;
    }
    false
}

/// Cierra ordenadamente la ventana de escritorio (`bals.exe --ui`) si está abierta.
pub fn close_ui_window() {
    let tray_hwnd = unsafe { FindWindowW(TRAY_WINDOW_CLASS, PCWSTR::null()) }.ok();
    if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR::null(), UI_WINDOW_TITLE) } {
        if !hwnd.is_invalid() && Some(hwnd) != tray_hwnd {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn post_to_daemon(msg: u32) -> bool {
    match unsafe { FindWindowW(TRAY_WINDOW_CLASS, PCWSTR::null()) } {
        Ok(hwnd) if !hwnd.is_invalid() => {
            unsafe { PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0)) }.is_ok()
        }
        _ => false,
    }
}

/// Abre la ventana nativa liviana (`bals.exe --ui`) como proceso independiente de corta vida.
/// Si ya está abierta, la trae al frente; al cerrarla, el SO libera el 100% de su RAM y contexto GPU.
pub fn spawn_or_focus_ui() {
    let tray_hwnd = unsafe { FindWindowW(TRAY_WINDOW_CLASS, PCWSTR::null()) }.ok();

    if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR::null(), UI_WINDOW_TITLE) } {
        if !hwnd.is_invalid() && Some(hwnd) != tray_hwnd {
            unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            return;
        }
    }

    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(err) = Command::new(exe).arg("--ui").spawn() {
                warn!("No se pudo lanzar la ventana UI de Bals: {err}");
            }
        }
        Err(err) => warn!("No se pudo resolver current_exe: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HSTRING;

    #[test]
    fn shared_memory_telemetry_roundtrip() {
        let name = HSTRING::from(format!(
            "Local\\BalsEngineTelemetryTest_{}",
            std::process::id()
        ));
        let mut writer = TelemetryWriter::create_named(PCWSTR(name.as_ptr()))
            .expect("debería crear archivo de memoria compartida");

        writer.publish(
            TelemetryState::RecordingGame,
            30,
            12.34,
            18.56,
            "Valorant — Acción 🎮",
            "NVENC · H.264",
        );

        let snap1 = read_telemetry_named(PCWSTR(name.as_ptr()))
            .expect("debería leer snapshot desde memoria compartida");
        assert_eq!(snap1.state, TelemetryState::RecordingGame);
        assert_eq!(snap1.replay_target_secs, 30);
        assert!((snap1.buffered_secs - 12.3).abs() < 0.01);
        assert!((snap1.buffer_ram_mb - 18.6).abs() < 0.01);
        assert_eq!(snap1.clip_event_seq, 0);
        assert!(!snap1.clip_event_is_err);
        assert_eq!(snap1.game_name, "Valorant — Acción 🎮");
        assert_eq!(snap1.encoder_label, "NVENC · H.264");

        // Probar evento de clip y truncado UTF-8 seguro con caracteres multi-byte.
        let long_msg = "CLIP GUARDADO: 🎬 ".repeat(20);
        writer.record_clip_event(&long_msg, true);
        writer.publish(
            TelemetryState::PausedAltTab,
            60,
            12.34,
            18.56,
            "Valorant",
            "NVENC · H.264",
        );

        let snap2 = read_telemetry_named(PCWSTR(name.as_ptr())).expect("debería leer snap2");
        assert_eq!(snap2.state, TelemetryState::PausedAltTab);
        assert_eq!(snap2.clip_event_seq, 1);
        assert!(snap2.clip_event_is_err);
        assert!(snap2.last_event.len() <= 256);
        assert!(snap2.last_event.starts_with("CLIP GUARDADO:"));
    }
}
