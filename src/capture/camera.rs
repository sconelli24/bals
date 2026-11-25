use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaSource, IMFMediaType, IMFSample,
    IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFShutdown, MFStartup, MFVideoFormat_MJPG, MFVideoFormat_NV12, MFVideoFormat_RGB32,
    MFVideoFormat_YUY2,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::GUID;

use crate::config::CameraPreset;

/// Dispositivo de cámara (webcam) detectado vía Windows Media Foundation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraDeviceInfo {
    /// Nombre amigable para mostrar en la UI (ej. `"Logitech HD Pro Webcam C920"`).
    pub friendly_name: String,
    /// Identificador único y estable del dispositivo en Windows (symbolic link).
    pub symbolic_link: String,
    /// Presets reales de resolución y FPS soportados por esta cámara (ordenados ascendentemente).
    pub supported_presets: Vec<CameraPreset>,
}

impl CameraDeviceInfo {
    /// Resuelve el preset activo garantizando que pertenezca a `supported_presets`
    /// (por defecto prioriza `720p30`, luego cualquier `720p`, luego el primero disponible).
    pub fn resolve_preset(&self, preferred: Option<CameraPreset>) -> Option<CameraPreset> {
        if self.supported_presets.is_empty() {
            return None;
        }
        if let Some(pref) = preferred {
            if self.supported_presets.contains(&pref) {
                return Some(pref);
            }
        }
        self.supported_presets
            .iter()
            .copied()
            .find(|p| p.height == 720 && p.fps == 30)
            .or_else(|| self.supported_presets.iter().copied().find(|p| p.height == 720))
            .or_else(|| self.supported_presets.first().copied())
    }

    /// Devuelve el siguiente preset disponible en orden ascendente (ej. `720p30 → 720p60 → 1080p30 → 1080p60`).
    /// Si solo hay 0 o 1 preset disponible, no realiza ningún cambio.
    pub fn next_preset(&self, current: Option<CameraPreset>) -> Option<CameraPreset> {
        let resolved = self.resolve_preset(current)?;
        if self.supported_presets.len() <= 1 {
            return Some(resolved);
        }
        let idx = self
            .supported_presets
            .iter()
            .position(|&p| p == resolved)
            .unwrap_or(0);
        let next = (idx + 1) % self.supported_presets.len();
        Some(self.supported_presets[next])
    }
}

/// Frame capturado de la webcam en formato `BGRA8` opaco (alfa = 255) listo para subir a D3D11.
pub struct CameraFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Enumera todas las cámaras conectadas en Windows usando Media Foundation (0 crates externos).
pub fn list_cameras() -> Vec<CameraDeviceInfo> {
    let ro_ok = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();
    if unsafe { MFStartup(MF_VERSION, 0) }.is_err() {
        if ro_ok {
            unsafe { RoUninitialize() };
        }
        return Vec::new();
    }

    let list = enumerate_camera_activates()
        .map(|items| items.into_iter().map(|(_, info)| info).collect())
        .unwrap_or_default();

    unsafe {
        let _ = MFShutdown();
        if ro_ok {
            RoUninitialize();
        }
    }
    list
}

/// Sesión activa de captura de webcam en un hilo dedicado con buzón de último frame.
///
/// - Solo se crea cuando `camera_enabled == true` y hay grabación activa (0% consumo y LED apagado en reposo).
/// - Selecciona automáticamente un formato eficiente cercano a `1280×720 @ 30 fps`.
/// - Soporta activar/desactivar espejo horizontal (`set_mirror`) en vivo sin reabrir el dispositivo.
pub struct CameraCapture {
    pub device_name: String,
    closed: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    mirror: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    recycle_buf: Arc<Mutex<Option<Vec<u8>>>>,
    worker: Option<JoinHandle<()>>,
}

impl CameraCapture {
    /// Inicia la captura de la cámara elegida (`symbolic_link`) o la primera disponible si es `None`,
    /// usando `preferred_preset` (resolución + FPS) si se especificó.
    pub fn start(
        selected_symbolic_link: Option<&str>,
        mirror: bool,
        preferred_preset: Option<CameraPreset>,
    ) -> Result<Self> {
        // Comprobar rápidamente qué dispositivo vamos a abrir para reportar error síncrono
        // si no hay cámaras conectadas.
        let cameras = list_cameras();
        if cameras.is_empty() {
            bail!("No se detectó ninguna cámara conectada");
        }

        let target = selected_symbolic_link
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|wanted| {
                cameras
                    .iter()
                    .find(|c| c.symbolic_link.eq_ignore_ascii_case(wanted))
            })
            .unwrap_or(&cameras[0])
            .clone();

        let effective_preset = target.resolve_preset(preferred_preset);

        let closed = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let mirror_flag = Arc::new(AtomicBool::new(mirror));
        let latest_frame: Arc<Mutex<Option<CameraFrame>>> = Arc::new(Mutex::new(None));
        let recycle_buf: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));

        let closed_bg = Arc::clone(&closed);
        let alive_bg = Arc::clone(&alive);
        let mirror_bg = Arc::clone(&mirror_flag);
        let latest_bg = Arc::clone(&latest_frame);
        let recycle_bg = Arc::clone(&recycle_buf);
        let target_link = target.symbolic_link.clone();
        let target_name = target.friendly_name.clone();

        // Canal one-shot para confirmar si la cámara abrió correctamente (o está en uso por otra app).
        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<()>>(1);

        let worker = thread::Builder::new()
            .name("bals-webcam".to_string())
            .spawn(move || {
                let ro_ok = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();
                if let Err(err) = unsafe { MFStartup(MF_VERSION, 0) } {
                    let _ = init_tx.send(Err(anyhow::anyhow!("MFStartup (webcam) falló: {err}")));
                    alive_bg.store(false, Ordering::Release);
                    if ro_ok {
                        unsafe { RoUninitialize() };
                    }
                    return;
                }

                camera_worker_loop(
                    &target_link,
                    &target_name,
                    effective_preset,
                    init_tx,
                    closed_bg,
                    alive_bg.clone(),
                    mirror_bg,
                    latest_bg,
                    recycle_bg,
                );

                alive_bg.store(false, Ordering::Release);
                unsafe {
                    let _ = MFShutdown();
                    if ro_ok {
                        RoUninitialize();
                    }
                }
            })
            .context("No se pudo iniciar el hilo bals-webcam")?;

        match init_rx.recv() {
            Ok(Ok(())) => {
                info!(
                    camera = %target.friendly_name,
                    preset = effective_preset.map(|p| p.label()).unwrap_or_else(|| "auto".to_string()),
                    mirror,
                    "Captura de cámara iniciada"
                );
                Ok(Self {
                    device_name: target.friendly_name,
                    closed,
                    alive,
                    mirror: mirror_flag,
                    latest_frame,
                    recycle_buf,
                    worker: Some(worker),
                })
            }
            Ok(Err(err)) => {
                closed.store(true, Ordering::SeqCst);
                let _ = worker.join();
                Err(err)
            }
            Err(_) => {
                closed.store(true, Ordering::SeqCst);
                let _ = worker.join();
                bail!("El hilo de captura de cámara terminó inesperadamente al iniciar")
            }
        }
    }

    /// Extrae el frame más reciente de la webcam (si llegó alguno desde el último tick).
    pub fn take_latest(&self) -> Option<CameraFrame> {
        self.latest_frame.lock().ok()?.take()
    }

    /// Devuelve un buffer `BGRA8` ya subido a GPU para reutilizarlo en el siguiente frame
    /// (evita asignaciones repetidas de varios MB en el heap).
    pub fn recycle_buffer(&self, buf: Vec<u8>) {
        if let Ok(mut slot) = self.recycle_buf.lock() {
            *slot = Some(buf);
        }
    }

    /// Cambia el modo espejo horizontal en caliente sin reiniciar la cámara.
    pub fn set_mirror(&self, mirror: bool) {
        self.mirror.store(mirror, Ordering::Relaxed);
    }

    /// Devuelve `false` si la cámara se desconectó o dejó de entregar frames.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn stop(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            if let Ok(mut slot) = self.latest_frame.lock() {
                *slot = None;
            }
            if let Ok(mut slot) = self.recycle_buf.lock() {
                *slot = None;
            }
            if let Some(handle) = self.worker.take() {
                let _ = handle.join();
            }
            debug!(camera = %self.device_name, "Captura de cámara detenida (LED apagado)");
        }
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn camera_worker_loop(
    target_link: &str,
    target_name: &str,
    preferred_preset: Option<CameraPreset>,
    init_tx: crossbeam_channel::Sender<Result<()>>,
    closed: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    mirror: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CameraFrame>>>,
    recycle_buf: Arc<Mutex<Option<Vec<u8>>>>,
) {
    let setup = open_camera_reader(target_link, target_name, preferred_preset);
    let (source, reader, mut width, mut height, mut stride) = match setup {
        Ok(ok) => {
            let _ = init_tx.send(Ok(()));
            ok
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    let stream_idx = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut consecutive_errors = 0u32;

    while !closed.load(Ordering::Relaxed) {
        let mut flags = 0u32;
        let mut sample_opt: Option<IMFSample> = None;

        let read_res = unsafe {
            reader.ReadSample(
                stream_idx,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut sample_opt),
            )
        };

        if closed.load(Ordering::Relaxed) {
            break;
        }

        if let Err(err) = read_res {
            consecutive_errors += 1;
            if consecutive_errors >= 5 {
                warn!(camera = %target_name, "Error leyendo frames de cámara ({err}); deteniendo fuente");
                break;
            }
            thread::sleep(std::time::Duration::from_millis(30));
            continue;
        }
        consecutive_errors = 0;

        if (flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32)) != 0
        {
            warn!(camera = %target_name, flags, "Flujo de cámara finalizado o desconectado");
            break;
        }

        if (flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0 {
            if let Ok((w, h, s)) = query_current_rgb32_layout(&reader, stream_idx) {
                width = w;
                height = h;
                stride = s;
                debug!(camera = %target_name, width, height, stride, "Formato activo de cámara actualizado");
            }
        }

        let Some(sample) = sample_opt else {
            continue;
        };

        let Ok(buffer): windows::core::Result<IMFMediaBuffer> =
            (unsafe { sample.ConvertToContiguousBuffer() })
        else {
            continue;
        };

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        if unsafe { buffer.Lock(&mut ptr, None, Some(&mut cur_len)) }.is_err() {
            continue;
        }

        let mut bgra = recycle_buf
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or_default();
        if !ptr.is_null() && cur_len > 0 && width >= 64 && height >= 48 {
            let src = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize) };
            copy_rgb32_to_bgra_into(
                &mut bgra,
                src,
                width,
                height,
                stride,
                mirror.load(Ordering::Relaxed),
            );
        } else {
            bgra.clear();
        }

        unsafe {
            let _ = buffer.Unlock();
        }

        if !bgra.is_empty() {
            if let Ok(mut slot) = latest_frame.lock() {
                if let Some(old) = slot.replace(CameraFrame {
                    width,
                    height,
                    bgra,
                }) {
                    if let Ok(mut rec) = recycle_buf.lock() {
                        *rec = Some(old.bgra);
                    }
                }
            }
        } else if let Ok(mut rec) = recycle_buf.lock() {
            *rec = Some(bgra);
        }
    }

    alive.store(false, Ordering::Release);
    drop(reader);
    unsafe {
        let _ = source.Shutdown();
    }
}

fn open_camera_reader(
    target_link: &str,
    target_name: &str,
    preferred_preset: Option<CameraPreset>,
) -> Result<(IMFMediaSource, IMFSourceReader, u32, u32, i32)> {
    let activates = enumerate_camera_activates_raw()?;
    let activate = activates
        .into_iter()
        .find(|(_, _, link)| link.eq_ignore_ascii_case(target_link))
        .map(|(act, _, _)| act)
        .with_context(|| format!("La cámara '{target_name}' ya no está disponible"))?;

    let source: IMFMediaSource = unsafe { activate.ActivateObject() }
        .with_context(|| format!("No se pudo activar la cámara '{target_name}'"))?;

    let mut reader_attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut reader_attrs, 2) }?;
    let reader_attrs = reader_attrs.context("MFCreateAttributes (webcam) devolvió None")?;
    unsafe {
        let _ = reader_attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1);
        let _ = reader_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1);
    }

    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, Some(&reader_attrs)) }
        .with_context(|| {
            let _ = unsafe { source.Shutdown() };
            format!("No se pudo abrir '{target_name}' (¿está en uso por otra aplicación?)")
        })?;

    let stream_idx = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // 1. Elegir el formato nativo que coincida con el preset elegido (o ~720p @ 30fps por defecto).
    if let Some((best_type, nat_w, nat_h, nat_fps)) =
        select_best_native_type(&reader, stream_idx, preferred_preset)
    {
        if let Err(err) = unsafe { reader.SetCurrentMediaType(stream_idx, None, &best_type) } {
            debug!(camera = %target_name, "No se pudo fijar formato nativo preferido: {err}");
        } else {
            debug!(
                camera = %target_name,
                width = nat_w,
                height = nat_h,
                fps = format!("{nat_fps:.1}"),
                "Formato nativo de webcam seleccionado"
            );
        }
    }

    // 2. Pedir salida descomprimida RGB32 (BGRA/BGRX) para subir directo a D3D11 VideoProcessor.
    let rgb32_type = unsafe { MFCreateMediaType() }?;
    unsafe {
        rgb32_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        rgb32_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(stream_idx, None, &rgb32_type)
    }
    .with_context(|| {
        let _ = unsafe { source.Shutdown() };
        format!("No se pudo negociar salida RGB32 en '{target_name}'")
    })?;

    let (width, height, stride) = query_current_rgb32_layout(&reader, stream_idx).map_err(|e| {
        let _ = unsafe { source.Shutdown() };
        e
    })?;

    debug!(
        camera = %target_name,
        width,
        height,
        stride,
        "IMFSourceReader de webcam configurado (RGB32 -> BGRA8)"
    );

    Ok((source, reader, width, height, stride))
}

fn select_best_native_type(
    reader: &IMFSourceReader,
    stream_idx: u32,
    preferred_preset: Option<CameraPreset>,
) -> Option<(IMFMediaType, u32, u32, f64)> {
    let mut best: Option<(i64, IMFMediaType, u32, u32, f64)> = None;

    for idx in 0..128u32 {
        let Ok(mt): windows::core::Result<IMFMediaType> =
            (unsafe { reader.GetNativeMediaType(stream_idx, idx) })
        else {
            break;
        };

        let Ok(packed_size) = (unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }) else {
            continue;
        };
        let (w, h) = unpack_u32_pair(packed_size);
        if w < 320 || h < 240 {
            continue;
        }

        let fps = unsafe { mt.GetUINT64(&MF_MT_FRAME_RATE) }
            .map(|p| {
                let (n, d) = unpack_u32_pair(p);
                if d > 0 { n as f64 / d as f64 } else { 0.0 }
            })
            .unwrap_or(30.0);

        let subtype = unsafe { mt.GetGUID(&MF_MT_SUBTYPE) }.ok();
        let score = score_native_camera_format(w, h, fps, subtype, preferred_preset);

        if best.as_ref().is_none_or(|(best_score, _, _, _, _)| score > *best_score) {
            best = Some((score, mt, w, h, fps));
        }
    }

    best.map(|(_, mt, w, h, fps)| (mt, w, h, fps))
}

/// Puntúa formatos nativos de webcam priorizando `preferred_preset` (si está definido)
/// o `1280×720 @ ~30fps` por defecto, prefiriendo subtipos ligeros de decodificar.
fn score_native_camera_format(
    w: u32,
    h: u32,
    fps: f64,
    subtype: Option<GUID>,
    preferred: Option<CameraPreset>,
) -> i64 {
    let mut score = 0i64;

    if let Some(pref) = preferred {
        let same_size = w == pref.width && h == pref.height;
        let fps_diff = (fps - pref.fps as f64).abs();
        if same_size && fps_diff <= 5.5 {
            score += 2000 - (fps_diff * 10.0) as i64;
        } else if same_size {
            score += 900 - (fps_diff * 5.0) as i64;
        } else {
            let size_dist = (w as i64 - pref.width as i64).abs() + (h as i64 - pref.height as i64).abs();
            score += 400 - (size_dist / 4) - (fps_diff * 4.0) as i64;
        }
    } else {
        // 1. Resolución ideal para PiP: 1280×720 (nítida y liviana). Penalizar 4K/1080p innecesarios.
        if w == 1280 && h == 720 {
            score += 500;
        } else if (640..=1280).contains(&w) && (360..=720).contains(&h) {
            let dist = (1280 - w as i64).abs() + (720 - h as i64).abs();
            score += 420 - (dist / 4);
        } else if w <= 1920 && h <= 1080 {
            score += 220;
        } else {
            score -= 300;
        }

        // 2. FPS ideal: 24–30 fps (suficiente fluidez con bajo consumo USB/CPU).
        if (24.0..=31.0).contains(&fps) {
            score += 300;
        } else if (15.0..=61.0).contains(&fps) {
            score += 180;
        } else {
            score -= 100;
        }
    }

    // 3. Subtipo: NV12/YUY2 no requieren decodificación JPEG; MJPG es muy común a 720p/1080p.
    if let Some(sub) = subtype {
        if sub == MFVideoFormat_NV12 {
            score += 120;
        } else if sub == MFVideoFormat_YUY2 {
            score += 100;
        } else if sub == MFVideoFormat_MJPG {
            score += 80;
        } else if sub == MFVideoFormat_RGB32 {
            score += 70;
        }
    }

    score
}

fn query_current_rgb32_layout(reader: &IMFSourceReader, stream_idx: u32) -> Result<(u32, u32, i32)> {
    let current = unsafe { reader.GetCurrentMediaType(stream_idx) }
        .context("GetCurrentMediaType (webcam) falló")?;
    let packed = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }
        .context("MF_MT_FRAME_SIZE ausente en webcam")?;
    let (w, h) = unpack_u32_pair(packed);
    if w < 64 || h < 48 {
        bail!("Dimensiones inválidas reportadas por webcam: {w}x{h}");
    }
    let stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|u| u as i32)
        .unwrap_or((w * 4) as i32);
    Ok((w, h, stride))
}

fn enumerate_camera_activates() -> Result<Vec<(IMFActivate, CameraDeviceInfo)>> {
    let raw = enumerate_camera_activates_raw()?;
    let mut out = Vec::with_capacity(raw.len());
    for (activate, friendly_name, symbolic_link) in raw {
        let supported_presets = query_supported_presets_without_streaming(&activate);
        out.push((
            activate,
            CameraDeviceInfo {
                friendly_name,
                symbolic_link,
                supported_presets,
            },
        ));
    }
    Ok(out)
}

fn enumerate_camera_activates_raw() -> Result<Vec<(IMFActivate, String, String)>> {
    let mut attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attrs, 1) }.context("MFCreateAttributes falló")?;
    let attrs = attrs.context("MFCreateAttributes devolvió None")?;
    unsafe {
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
    }

    let mut raw_activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attrs, &mut raw_activates, &mut count) }
        .context("MFEnumDeviceSources falló")?;

    if count == 0 || raw_activates.is_null() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(count as usize);
    let slice = unsafe { std::slice::from_raw_parts_mut(raw_activates, count as usize) };
    for slot in slice.iter_mut() {
        if let Some(activate) = slot.take() {
            let friendly_name = get_activate_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                .unwrap_or_else(|| "Cámara USB".to_string());
            let symbolic_link = get_activate_string(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            )
            .unwrap_or_else(|| friendly_name.clone());
            out.push((activate, friendly_name, symbolic_link));
        }
    }

    unsafe {
        CoTaskMemFree(Some(raw_activates as *const _));
    }

    Ok(out)
}

/// Consulta los formatos `(ancho, alto, fps)` soportados por la cámara inspeccionando su
/// `IMFPresentationDescriptor` sin iniciar captura ni encender el LED de la webcam.
fn query_supported_presets_without_streaming(activate: &IMFActivate) -> Vec<CameraPreset> {
    let Ok(source): windows::core::Result<IMFMediaSource> = (unsafe { activate.ActivateObject() })
    else {
        return Vec::new();
    };

    let mut raw_modes: Vec<(u32, u32, f64)> = Vec::new();

    if let Ok(pd) = unsafe { source.CreatePresentationDescriptor() } {
        let stream_count = unsafe { pd.GetStreamDescriptorCount() }.unwrap_or(0);
        for s_idx in 0..stream_count {
            let mut selected = windows::core::BOOL(0);
            let mut sd = None;
            if unsafe { pd.GetStreamDescriptorByIndex(s_idx, &mut selected, &mut sd) }.is_err() {
                continue;
            }
            let Some(sd) = sd else {
                continue;
            };
            let Ok(handler) = (unsafe { sd.GetMediaTypeHandler() }) else {
                continue;
            };
            if unsafe { handler.GetMajorType() }.ok() != Some(MFMediaType_Video) {
                continue;
            }
            let mt_count = unsafe { handler.GetMediaTypeCount() }.unwrap_or(0);
            for m_idx in 0..mt_count {
                let Ok(mt) = (unsafe { handler.GetMediaTypeByIndex(m_idx) }) else {
                    continue;
                };
                let Ok(packed_size) = (unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }) else {
                    continue;
                };
                let (w, h) = unpack_u32_pair(packed_size);
                let fps = unsafe { mt.GetUINT64(&MF_MT_FRAME_RATE) }
                    .map(|p| {
                        let (n, d) = unpack_u32_pair(p);
                        if d > 0 { n as f64 / d as f64 } else { 0.0 }
                    })
                    .unwrap_or(30.0);
                raw_modes.push((w, h, fps));
            }
            if !raw_modes.is_empty() {
                break;
            }
        }
    }

    unsafe {
        let _ = source.Shutdown();
    }

    collect_camera_presets(&raw_modes)
}

/// Normaliza y ordena los modos crudos de una cámara en presets limpios seleccionables
/// (ej. `720p30`, `720p60`, `1080p30`, `1080p60`).
pub fn collect_camera_presets(raw_modes: &[(u32, u32, f64)]) -> Vec<CameraPreset> {
    // Agrupar por (altura, fps_bucket) quedándonos con el mayor ancho (p. ej. 1280×720 antes que 960×720).
    let mut by_tier: BTreeMap<(u32, u32), u32> = BTreeMap::new();

    for &(w, h, fps) in raw_modes {
        if w < 640 || h < 360 || h > 1080 || w < h {
            continue;
        }
        let fps_bucket = if (50.0..=65.0).contains(&fps) {
            60
        } else if (24.0..=35.0).contains(&fps) {
            30
        } else {
            continue;
        };
        by_tier
            .entry((h, fps_bucket))
            .and_modify(|existing_w| *existing_w = (*existing_w).max(w))
            .or_insert(w);
    }

    // Si la cámara soporta tiers HD/FHD estándar (720p / 1080p), mostrar esos para un ciclado limpio
    // (720p30 → 720p60 → 1080p30 → 1080p60). Si es una cámara VGA/antigua, conservar sus tiers disponibles.
    let has_hd = by_tier.keys().any(|&(h, _)| h == 720 || h == 1080);

    let mut presets: Vec<CameraPreset> = by_tier
        .into_iter()
        .filter(|&((h, _), _)| !has_hd || h == 720 || h == 1080)
        .map(|((height, fps), width)| CameraPreset { width, height, fps })
        .collect();

    presets.sort_by_key(|p| (p.height, p.width, p.fps));
    presets
}

fn get_activate_string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    let len = unsafe { activate.GetStringLength(key) }.ok()?;
    let mut buf = vec![0u16; len as usize + 1];
    unsafe { activate.GetString(key, &mut buf, None) }.ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Copia un frame `MFVideoFormat_RGB32` (BGRX) a un buffer `BGRA8` contiguo con alfa opaco (`0xFF`),
/// corrigiendo strides negativos (imágenes invertidas verticalmente) y aplicando espejo horizontal si se solicita.
#[cfg(test)]
pub fn copy_rgb32_to_bgra(
    src: &[u8],
    width: u32,
    height: u32,
    default_stride: i32,
    mirror_x: bool,
) -> Vec<u8> {
    let mut dst = Vec::new();
    copy_rgb32_to_bgra_into(&mut dst, src, width, height, default_stride, mirror_x);
    dst
}

/// Variante sin asignaciones por frame que reutiliza la capacidad de `dst`.
pub fn copy_rgb32_to_bgra_into(
    dst: &mut Vec<u8>,
    src: &[u8],
    width: u32,
    height: u32,
    default_stride: i32,
    mirror_x: bool,
) {
    let w = width as usize;
    let h = height as usize;
    let row_bytes = w * 4;
    let row_stride = default_stride.unsigned_abs().max(row_bytes as u32) as usize;
    let flip_y = default_stride < 0;

    if src.len() < row_stride.saturating_mul(h) || w == 0 || h == 0 {
        dst.clear();
        return;
    }

    dst.resize(row_bytes * h, 0);
    for y in 0..h {
        let src_y = if flip_y { h - 1 - y } else { y };
        let src_row = &src[src_y * row_stride..src_y * row_stride + row_bytes];
        let dst_row = &mut dst[y * row_bytes..(y + 1) * row_bytes];

        if mirror_x {
            for (x, d) in dst_row.chunks_exact_mut(4).enumerate() {
                let sx = (w - 1 - x) * 4;
                d[0] = src_row[sx];
                d[1] = src_row[sx + 1];
                d[2] = src_row[sx + 2];
                d[3] = 255;
            }
        } else {
            for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                d[0] = s[0];
                d[1] = s[1];
                d[2] = s[2];
                d[3] = 255;
            }
        }
    }
}

#[inline]
const fn unpack_u32_pair(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_rgb32_with_alpha_mirror_and_negative_stride() {
        // 2×2 imagen BGRX (alfa 0 en origen):
        // Fila 0: [P00=(1,2,3,0), P10=(4,5,6,0)]
        // Fila 1: [P01=(7,8,9,0), P11=(10,11,12,0)]
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, // row 0
            7, 8, 9, 0, 10, 11, 12, 0, // row 1
        ];

        // Normal (stride positivo, sin espejo): conserva orden y fuerza alfa=255.
        let normal = copy_rgb32_to_bgra(&src, 2, 2, 8, false);
        assert_eq!(
            normal,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );

        // Espejo horizontal (mirror_x = true): intercambia izquierda/derecha en cada fila.
        let mirrored = copy_rgb32_to_bgra(&src, 2, 2, 8, true);
        assert_eq!(
            mirrored,
            vec![4, 5, 6, 255, 1, 2, 3, 255, 10, 11, 12, 255, 7, 8, 9, 255]
        );

        // Stride negativo (bottom-up) + espejo horizontal: invierte filas y columnas.
        let flipped = copy_rgb32_to_bgra(&src, 2, 2, -8, true);
        assert_eq!(
            flipped,
            vec![10, 11, 12, 255, 7, 8, 9, 255, 4, 5, 6, 255, 1, 2, 3, 255]
        );
    }

    #[test]
    fn prefers_720p_30fps_native_camera_format() {
        let s_720_nv12 = score_native_camera_format(1280, 720, 30.0, Some(MFVideoFormat_NV12), None);
        let s_720_mjpg = score_native_camera_format(1280, 720, 30.0, Some(MFVideoFormat_MJPG), None);
        let s_1080_mjpg = score_native_camera_format(1920, 1080, 30.0, Some(MFVideoFormat_MJPG), None);
        let s_4k_mjpg = score_native_camera_format(3840, 2160, 30.0, Some(MFVideoFormat_MJPG), None);
        assert!(s_720_nv12 > s_720_mjpg);
        assert!(s_720_mjpg > s_1080_mjpg);
        assert!(s_1080_mjpg > s_4k_mjpg);

        // Con preset explícito 1080p60, debe ganar 1080p60 sobre 720p30.
        let pref = Some(CameraPreset {
            width: 1920,
            height: 1080,
            fps: 60,
        });
        let s_1080_60 = score_native_camera_format(1920, 1080, 60.0, Some(MFVideoFormat_MJPG), pref);
        let s_720_30 = score_native_camera_format(1280, 720, 30.0, Some(MFVideoFormat_NV12), pref);
        assert!(s_1080_60 > s_720_30);
    }

    #[test]
    fn collects_and_cycles_supported_camera_presets() {
        let modes = [
            (640, 480, 30.0),
            (1280, 720, 30.0),
            (1280, 720, 59.94),
            (1920, 1080, 30.0),
            (1920, 1080, 60.0),
            (3840, 2160, 30.0),
        ];
        let presets = collect_camera_presets(&modes);
        let labels: Vec<String> = presets.iter().map(|p| p.label()).collect();
        assert_eq!(labels, vec!["720p30", "720p60", "1080p30", "1080p60"]);

        let dev = CameraDeviceInfo {
            friendly_name: "TestCam".to_string(),
            symbolic_link: "usb#test".to_string(),
            supported_presets: presets,
        };
        let p0 = dev.resolve_preset(None).unwrap();
        assert_eq!(p0.label(), "720p30");
        let p1 = dev.next_preset(Some(p0)).unwrap();
        assert_eq!(p1.label(), "720p60");
        let p2 = dev.next_preset(Some(p1)).unwrap();
        assert_eq!(p2.label(), "1080p30");
        let p3 = dev.next_preset(Some(p2)).unwrap();
        assert_eq!(p3.label(), "1080p60");
        let p4 = dev.next_preset(Some(p3)).unwrap();
        assert_eq!(p4.label(), "720p30");

        // Si solo tiene 1 formato disponible, next_preset no cambia.
        let single = CameraDeviceInfo {
            friendly_name: "BasicCam".to_string(),
            symbolic_link: "usb#basic".to_string(),
            supported_presets: vec![CameraPreset {
                width: 1280,
                height: 720,
                fps: 30,
            }],
        };
        assert_eq!(single.next_preset(None).unwrap().label(), "720p30");
    }

    #[test]
    fn enumerates_system_cameras_without_hanging() {
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        // Simular el hilo UI de winit/eframe (COM STA).
        let ole_ok = unsafe { OleInitialize(None) }.is_ok();
        let t0 = std::time::Instant::now();
        let cams = list_cameras();
        if ole_ok {
            unsafe { OleUninitialize() };
        }
        println!(
            "list_cameras() on STA returned {} camera(s) in {:.0} ms:",
            cams.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
        for c in &cams {
            let presets: Vec<String> = c.supported_presets.iter().map(|p| p.label()).collect();
            println!("  - {} -> {:?}", c.friendly_name, presets);
        }
    }
}
