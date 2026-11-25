use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::core::Interface;

use crate::capture::{CaptureTarget, CapturedFrame};
use crate::clock;
use crate::d3d::SharedD3DDevice;

/// Anillo de 4 texturas BGRA en VRAM para copiar el desktop de DXGI sin bloquear a DWM,
/// permitiendo conservar `last_game_frame` para recomponer webcam en pantalla estática.
const COPY_RING_SIZE: usize = 4;

#[derive(Default)]
pub struct DxgiCaptureStats {
    pub frames_received: AtomicU64,
    pub frames_replaced: AtomicU64,
}

/// Captura Zero-Copy de monitor completo vía **DXGI Desktop Duplication** (`IDXGIOutputDuplication`).
///
/// Ventajas clave en **Windows 10**:
/// - No dibuja el borde amarillo de privacidad de WGC.
/// - Evita el estrangulamiento de DWM de WGC sobre juegos fullscreen.
/// - 100% Zero-Copy en VRAM (`CopyResource` GPU→GPU + `ReleaseFrame` inmediato).
/// - Se recupera automáticamente de `DXGI_ERROR_ACCESS_LOST` (cambios de resolución, 4:3, UAC).
pub struct DxgiDupCapture {
    closed: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
    pub stats: Arc<DxgiCaptureStats>,
    pub initial_width: u32,
    pub initial_height: u32,
    worker: Option<JoinHandle<()>>,
}

impl DxgiDupCapture {
    pub fn start(d3d: &SharedD3DDevice, target: CaptureTarget) -> Result<Self> {
        let target_hmonitor = match target {
            CaptureTarget::PrimaryMonitor => {
                unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
            }
            CaptureTarget::Monitor(h) => h,
            CaptureTarget::Window(_) => {
                bail!("DXGI Desktop Duplication solo soporta captura de monitor")
            }
        };

        let (duplication, initial_width, initial_height) =
            create_duplication_for_monitor(&d3d.device, target_hmonitor)?;

        let closed = Arc::new(AtomicBool::new(false));
        let latest_frame: Arc<Mutex<Option<CapturedFrame>>> = Arc::new(Mutex::new(None));
        let stats = Arc::new(DxgiCaptureStats::default());

        let send_d3d = SendD3D {
            device: d3d.device.clone(),
            context: d3d.context.clone(),
        };
        let closed_clone = Arc::clone(&closed);
        let latest_clone = Arc::clone(&latest_frame);
        let stats_clone = Arc::clone(&stats);
        let hmon_raw = target_hmonitor.0 as isize;

        // Enviar `IDXGIOutputDuplication` al hilo worker dedicado.
        let mut initial_dupl = Some(SendDuplication(duplication));

        let worker = thread::Builder::new()
            .name("bals-dxgi-dup".to_string())
            .spawn(move || {
                unsafe {
                    let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                }
                let hmon = HMONITOR(hmon_raw as *mut std::ffi::c_void);
                let first_dupl = initial_dupl.take().map(|s| s.0);
                dxgi_capture_loop(
                    send_d3d,
                    hmon,
                    first_dupl,
                    initial_width,
                    initial_height,
                    closed_clone,
                    latest_clone,
                    stats_clone,
                );
            })
            .context("No se pudo iniciar el hilo bals-dxgi-dup")?;

        info!(
            width = initial_width,
            height = initial_height,
            "Captura DXGI Desktop Duplication iniciada (Zero-Copy GPU · sin borde amarillo)"
        );

        Ok(Self {
            closed,
            latest_frame,
            stats,
            initial_width,
            initial_height,
            worker: Some(worker),
        })
    }

    pub fn take_latest(&self) -> Option<CapturedFrame> {
        self.latest_frame.lock().ok()?.take()
    }

    pub fn stop(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            if let Ok(mut slot) = self.latest_frame.lock() {
                *slot = None;
            }
            if let Some(handle) = self.worker.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for DxgiDupCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SendDuplication(IDXGIOutputDuplication);
unsafe impl Send for SendDuplication {}

/// Wrapper `Send` para `ID3D11Device` + `ID3D11DeviceContext` (seguro porque `SharedD3DDevice`
/// activa `ID3D11Multithread::SetMultithreadProtected(true)`).
struct SendD3D {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}
unsafe impl Send for SendD3D {}

#[allow(clippy::too_many_arguments)]
fn dxgi_capture_loop(
    d3d: SendD3D,
    hmon: HMONITOR,
    initial_dupl: Option<IDXGIOutputDuplication>,
    initial_w: u32,
    initial_h: u32,
    closed: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
    stats: Arc<DxgiCaptureStats>,
) {
    let mut duplication = initial_dupl;
    let mut ring: Vec<ID3D11Texture2D> = create_bgra_ring(&d3d.device, initial_w, initial_h)
        .unwrap_or_default();
    let mut ring_w = initial_w;
    let mut ring_h = initial_h;
    let mut next_slot = 0usize;
    let mut got_first_frame = false;

    while !closed.load(Ordering::Relaxed) {
        let Some(dupl) = duplication.as_ref() else {
            thread::sleep(Duration::from_millis(80));
            if closed.load(Ordering::Relaxed) {
                break;
            }
            match create_duplication_for_monitor(&d3d.device, hmon) {
                Ok((new_dupl, w, h)) => {
                    if w != ring_w || h != ring_h || ring.is_empty() {
                        if let Ok(new_ring) = create_bgra_ring(&d3d.device, w, h) {
                            ring = new_ring;
                            ring_w = w;
                            ring_h = h;
                            next_slot = 0;
                        }
                    }
                    duplication = Some(new_dupl);
                    got_first_frame = false;
                    debug!(width = w, height = h, "DXGI Desktop Duplication reconectado");
                }
                Err(err) => {
                    debug!("Reintentando reconexión DXGI Desktop Duplication: {err:#}");
                }
            }
            continue;
        };

        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut desktop_res: Option<IDXGIResource> = None;

        match unsafe { dupl.AcquireNextFrame(16, &mut frame_info, &mut desktop_res) } {
            Ok(()) => {
                let has_visual_update = !got_first_frame || frame_info.LastPresentTime != 0;

                if has_visual_update {
                    if let Some(res) = desktop_res.take() {
                        if let Ok(src_tex) = res.cast::<ID3D11Texture2D>() {
                            let mut desc = D3D11_TEXTURE2D_DESC::default();
                            unsafe { src_tex.GetDesc(&mut desc) };

                            if desc.Width >= 320 && desc.Height >= 200 {
                                if desc.Width != ring_w || desc.Height != ring_h || ring.is_empty() {
                                    if let Ok(new_ring) =
                                        create_bgra_ring(&d3d.device, desc.Width, desc.Height)
                                    {
                                        ring = new_ring;
                                        ring_w = desc.Width;
                                        ring_h = desc.Height;
                                        next_slot = 0;
                                    }
                                }

                                if !ring.is_empty() {
                                    let dst_tex = &ring[next_slot];
                                    next_slot = (next_slot + 1) % ring.len();

                                    unsafe {
                                        d3d.context.CopyResource(dst_tex, &src_tex);
                                        d3d.context.Flush();
                                    }

                                    let captured = CapturedFrame::new_dxgi(
                                        dst_tex.clone(),
                                        desc.Width,
                                        desc.Height,
                                        clock::now_100ns(),
                                    );

                                    stats.frames_received.fetch_add(1, Ordering::Relaxed);
                                    got_first_frame = true;

                                    if let Ok(mut slot) = latest_frame.lock() {
                                        if slot.replace(captured).is_some() {
                                            stats.frames_replaced.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                let _ = unsafe { dupl.ReleaseFrame() };
            }
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                debug!("DXGI_ERROR_ACCESS_LOST (cambio de modo/resolución); reconectando sin perder buffer");
                duplication = None;
            }
            Err(err) => {
                warn!("DXGI AcquireNextFrame error ({err}); reintentando duplicación");
                duplication = None;
            }
        }
    }
}

fn create_duplication_for_monitor(
    device: &ID3D11Device,
    target_hmonitor: HMONITOR,
) -> Result<(IDXGIOutputDuplication, u32, u32)> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .context("No se pudo obtener IDXGIDevice")?;
    let adapter = unsafe { dxgi_device.GetAdapter() }
        .context("IDXGIDevice::GetAdapter falló")?;

    let mut idx = 0u32;
    let mut matched_output = None;
    let mut first_output = None;

    while let Ok(output) = unsafe { adapter.EnumOutputs(idx) } {
        if first_output.is_none() {
            first_output = Some(output.clone());
        }
        if let Ok(desc) = unsafe { output.GetDesc() } {
            if desc.Monitor == target_hmonitor {
                matched_output = Some(output);
                break;
            }
        }
        idx += 1;
    }

    let output = matched_output
        .or(first_output)
        .context("El adaptador D3D11 activo no tiene salidas DXGI para este monitor")?;

    let output1: IDXGIOutput1 = output
        .cast()
        .context("El monitor no expone IDXGIOutput1")?;

    let duplication = unsafe { output1.DuplicateOutput(device) }
        .context("IDXGIOutput1::DuplicateOutput falló")?;

    let dupl_desc = unsafe { duplication.GetDesc() };

    // Si el monitor está en modo HDR (10-bit/16-bit), caer a WGC que realiza tone-mapping SDR.
    if dupl_desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        bail!(
            "Monitor en formato no-BGRA8 ({:?}); usando WGC para preservar color HDR→SDR",
            dupl_desc.ModeDesc.Format
        );
    }

    let width = dupl_desc.ModeDesc.Width;
    let height = dupl_desc.ModeDesc.Height;
    if width < 320 || height < 200 {
        bail!("Dimensiones DXGI inválidas: {width}x{height}");
    }

    Ok((duplication, width, height))
}

fn create_bgra_ring(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut ring = Vec::with_capacity(COPY_RING_SIZE);
    for _ in 0..COPY_RING_SIZE {
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .context("No se pudo crear textura BGRA en VRAM para DXGI Duplication")?;
        ring.push(tex.context("CreateTexture2D devolvió None")?);
    }
    Ok(ring)
}
