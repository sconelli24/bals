pub mod camera;
pub mod dxgi_dup;
pub mod wgc;

pub use camera::{CameraCapture, CameraDeviceInfo, list_cameras};

use anyhow::Result;
use tracing::warn;
use windows::Graphics::Capture::Direct3D11CaptureFrame;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};
use windows::core::{BOOL, w};

use crate::capture::dxgi_dup::DxgiDupCapture;
use crate::capture::wgc::WgcCapture;
use crate::d3d::SharedD3DDevice;

const MONITORINFOF_PRIMARY_FLAG: u32 = 1;
/// Build mínimo de Windows que soporta `GraphicsCaptureSession::SetIsBorderRequired(false)` (Win11+).
const WIN11_BORDERLESS_WGC_BUILD: u32 = 20348;

/// Consulta el número de build real de Windows desde el registro (`CurrentBuildNumber`),
/// sin depender de shims de manifiesto de `GetVersionExW`.
pub fn windows_build_number() -> Option<u32> {
    let mut hkey = HKEY::default();
    let open = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            Some(0),
            KEY_QUERY_VALUE,
            &mut hkey,
        )
    };
    if open.is_err() {
        return None;
    }

    let mut buf = [0u16; 32];
    let mut byte_len = (buf.len() * 2) as u32;
    let res = unsafe {
        RegQueryValueExW(
            hkey,
            w!("CurrentBuildNumber"),
            None,
            None,
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut byte_len),
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    if res.is_err() || byte_len < 2 {
        return None;
    }

    let u16_len = (byte_len as usize / 2).min(buf.len());
    let nul_pos = buf[..u16_len].iter().position(|&c| c == 0).unwrap_or(u16_len);
    String::from_utf16_lossy(&buf[..nul_pos]).trim().parse().ok()
}

/// Devuelve `true` en Windows 11+ (donde WGC soporta quitar el borde amarillo y captura MPO fluida).
pub fn is_windows_11_or_later() -> bool {
    windows_build_number().is_none_or(|b| b >= WIN11_BORDERLESS_WGC_BUILD)
}

/// Captura unificada Zero-Copy en GPU: WGC (Windows 11+) o DXGI Desktop Duplication (Windows 10).
pub enum FrameCapture {
    Wgc(WgcCapture),
    Dxgi(DxgiDupCapture),
}

impl FrameCapture {
    /// Inicia el mejor backend disponible para `target`.
    /// Si `prefer_dxgi` es `true` y el destino es un monitor, intenta primero DXGI Desktop Duplication
    /// (0 borde amarillo en Windows 10) y cae limpiamente a WGC si falla (ej. HDR o laptop híbrida).
    pub fn start(
        d3d: &SharedD3DDevice,
        target: CaptureTarget,
        capture_cursor: bool,
        prefer_dxgi: bool,
    ) -> Result<Self> {
        if prefer_dxgi && !matches!(target, CaptureTarget::Window(_)) {
            match DxgiDupCapture::start(d3d, target) {
                Ok(dxgi) => return Ok(FrameCapture::Dxgi(dxgi)),
                Err(err) => {
                    warn!("DXGI Desktop Duplication no disponible ({err:#}); usando WGC como fallback");
                }
            }
        }
        Ok(FrameCapture::Wgc(WgcCapture::start(
            d3d,
            target,
            capture_cursor,
        )?))
    }

    pub fn initial_size(&self) -> (u32, u32) {
        match self {
            FrameCapture::Wgc(w) => (w.initial_size.Width as u32, w.initial_size.Height as u32),
            FrameCapture::Dxgi(d) => (d.initial_width, d.initial_height),
        }
    }

    pub fn take_latest(&self) -> Option<CapturedFrame> {
        match self {
            FrameCapture::Wgc(w) => w.take_latest(),
            FrameCapture::Dxgi(d) => d.take_latest(),
        }
    }

    pub fn backend_label(&self) -> &'static str {
        match self {
            FrameCapture::Wgc(_) => "WGC",
            FrameCapture::Dxgi(_) => "DXGI",
        }
    }

    pub fn stats_counts(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        match self {
            FrameCapture::Wgc(w) => (
                w.stats.frames_received.load(Ordering::Relaxed),
                w.stats.frames_replaced.load(Ordering::Relaxed),
            ),
            FrameCapture::Dxgi(d) => (
                d.stats.frames_received.load(Ordering::Relaxed),
                d.stats.frames_replaced.load(Ordering::Relaxed),
            ),
        }
    }
}

/// Objetivo de captura (monitor principal, monitor específico o ventana de un juego).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum CaptureTarget {
    PrimaryMonitor,
    Monitor(HMONITOR),
    Window(HWND),
}

/// Información de un monitor conectado enumerado vía Win32 (`EnumDisplayMonitors`).
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Índice amigable base 1 (`1`, `2`, ...) con el monitor principal siempre primero.
    pub index: usize,
    /// Nombre estable del dispositivo en Windows (ej. `\\.\DISPLAY1`).
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub is_primary: bool,
    pub hmonitor: HMONITOR,
}

unsafe impl Send for MonitorInfo {}

impl MonitorInfo {
    /// Etiqueta corta para mostrar en el LCD/bandeja (ej. `PANTALLA 1`).
    pub fn short_label(&self) -> String {
        format!("PANTALLA {}", self.index)
    }

    /// Etiqueta descriptiva para botones de la UI (ej. `PANTALLA 1 · 1920x1080 ★`).
    pub fn ui_label(&self) -> String {
        if self.is_primary {
            format!("PANTALLA {} · {}x{} ★", self.index, self.width, self.height)
        } else {
            format!("PANTALLA {} · {}x{}", self.index, self.width, self.height)
        }
    }
}

/// Enumera todos los monitores activos en Windows (0 dependencias externas).
/// Ordena dejando el monitor principal primero y luego de izquierda a derecha.
pub fn list_monitors() -> Vec<MonitorInfo> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    let ptr = (&mut monitors as *mut Vec<MonitorInfo>) as isize;
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(enum_monitor_proc), LPARAM(ptr));
    }

    monitors.sort_by(|a, b| {
        b.is_primary
            .cmp(&a.is_primary)
            .then_with(|| a.left.cmp(&b.left))
            .then_with(|| a.top.cmp(&b.top))
            .then_with(|| a.device_name.cmp(&b.device_name))
    });

    for (i, m) in monitors.iter_mut().enumerate() {
        m.index = i + 1;
    }

    monitors
}

unsafe extern "system" fn enum_monitor_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _clip_rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe {
        GetMonitorInfoW(hmonitor, (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>())
    }
    .as_bool();

    if ok {
        let rc = info.monitorInfo.rcMonitor;
        let width = rc.right.saturating_sub(rc.left).unsigned_abs();
        let height = rc.bottom.saturating_sub(rc.top).unsigned_abs();
        let nul_pos = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let device_name = String::from_utf16_lossy(&info.szDevice[..nul_pos]);
        let is_primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY_FLAG) != 0;

        monitors.push(MonitorInfo {
            index: 0,
            device_name,
            width,
            height,
            left: rc.left,
            top: rc.top,
            is_primary,
            hmonitor,
        });
    }

    BOOL(1)
}

/// Resuelve el `CaptureTarget` y la etiqueta corta para el monitor configurado.
/// Si el monitor elegido fue desconectado (o es `None`), cae limpiamente al monitor principal.
pub fn resolve_monitor_target(selected_device: Option<&str>) -> (CaptureTarget, String) {
    let monitors = list_monitors();
    if let Some(wanted) = selected_device.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(m) = monitors
            .iter()
            .find(|m| m.device_name.eq_ignore_ascii_case(wanted))
        {
            return (CaptureTarget::Monitor(m.hmonitor), m.short_label());
        }
    }

    if let Some(primary) = monitors.first() {
        return (CaptureTarget::Monitor(primary.hmonitor), primary.short_label());
    }

    (CaptureTarget::PrimaryMonitor, "MONITOR".to_string())
}

/// Frame capturado en GPU (Zero-Copy).
///
/// Mantiene vivo `_wgc_frame` (cuando proviene de WGC) para que el FramePool no reutilice
/// la textura hasta que el consumidor (VideoProcessor / Encoder) termine de usarla.
pub struct CapturedFrame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// Marca de tiempo relativa del sistema en unidades de 100 nanosegundos (ticks de WinRT/MF).
    #[allow(dead_code)]
    pub timestamp_100ns: i64,
    _wgc_frame: Option<Direct3D11CaptureFrame>,
}

// Los objetos COM de D3D11 con ID3D11Multithread activo y Direct3D11CaptureFrame
// de un FramePool FreeThreaded pueden transferirse entre hilos de forma segura.
unsafe impl Send for CapturedFrame {}

impl CapturedFrame {
    pub(crate) fn new(
        texture: ID3D11Texture2D,
        width: u32,
        height: u32,
        timestamp_100ns: i64,
        wgc_frame: Direct3D11CaptureFrame,
    ) -> Self {
        Self {
            texture,
            width,
            height,
            timestamp_100ns,
            _wgc_frame: Some(wgc_frame),
        }
    }

    pub(crate) fn new_dxgi(
        texture: ID3D11Texture2D,
        width: u32,
        height: u32,
        timestamp_100ns: i64,
    ) -> Self {
        Self {
            texture,
            width,
            height,
            timestamp_100ns,
            _wgc_frame: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_labels_and_fallback_target() {
        let primary = MonitorInfo {
            index: 1,
            device_name: r"\\.\DISPLAY1".to_string(),
            width: 1920,
            height: 1080,
            left: 0,
            top: 0,
            is_primary: true,
            hmonitor: HMONITOR::default(),
        };
        assert_eq!(primary.short_label(), "PANTALLA 1");
        assert_eq!(primary.ui_label(), "PANTALLA 1 · 1920x1080 ★");

        let secondary = MonitorInfo {
            index: 2,
            device_name: r"\\.\DISPLAY2".to_string(),
            width: 2560,
            height: 1440,
            left: 1920,
            top: 0,
            is_primary: false,
            hmonitor: HMONITOR::default(),
        };
        assert_eq!(secondary.ui_label(), "PANTALLA 2 · 2560x1440");

        let (_target, label) = resolve_monitor_target(Some(r"\\.\DISPLAY_NON_EXISTENT"));
        assert!(!label.is_empty());

        let build = windows_build_number().expect("debería leer CurrentBuildNumber en Windows");
        assert!(build >= 10240);
        assert_eq!(is_windows_11_or_later(), build >= WIN11_BORDERLESS_WGC_BUILD);
    }
}
