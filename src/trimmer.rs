use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, ICodecAPI, IMF2DBuffer, IMFAttributes, IMFMediaBuffer, IMFMediaType,
    IMFSample, IMFSinkWriter, IMFSourceReader, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
    MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_PD_DURATION,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_ANY_STREAM,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_MEDIASOURCE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_TRANSCODE_CONTAINERTYPE, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_PCM,
    MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video,
    MFNominalRange_16_235, MFSampleExtension_CleanPoint, MFShutdown, MFStartup,
    MFTranscodeContainerType_MPEG4, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_CBR,
    eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, HSTRING, Interface};

/// Resultado de un recorte o compresión de clip MP4.
#[derive(Debug, Clone)]
pub struct TrimStats {
    pub output_path: PathBuf,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub duration_secs: f64,
    pub elapsed_ms: f64,
}

/// Metadatos básicos de un clip `.mp4` obtenidos sin decodificar frames (~2 ms).
#[derive(Debug, Clone, Copy)]
pub struct ClipMetadata {
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

/// Frame decodificado en formato RGBA8 listo para renderizar en el Monitor CRT de la UI.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub actual_time_secs: f64,
}

/// Consulta rápidamente duración, resolución y FPS reales de un `.mp4` sin decodificar (~2 ms).
pub fn probe_mp4_metadata(path: &Path) -> Result<ClipMetadata> {
    unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup (probe_metadata) falló")?;
    let res = probe_metadata_inner(path);
    unsafe {
        let _ = MFShutdown();
    }
    res
}

/// Consulta rápidamente la duración real de un archivo `.mp4` en segundos (~2 ms)
/// usando `IMFSourceReader` sin decodificar frames.
#[allow(dead_code)]
pub fn probe_mp4_duration_secs(path: &Path) -> Result<f64> {
    Ok(probe_mp4_metadata(path)?.duration_secs)
}

fn probe_metadata_inner(path: &Path) -> Result<ClipMetadata> {
    let url = HSTRING::from(path.as_os_str());
    let reader: IMFSourceReader = unsafe { MFCreateSourceReaderFromURL(&url, None) }
        .with_context(|| format!("No se pudo abrir {} con IMFSourceReader", path.display()))?;

    let prop: PROPVARIANT = unsafe {
        reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
    }
    .context("No se pudo leer MF_PD_DURATION")?;

    let duration_100ns: u64 = u64::try_from(&prop)
        .or_else(|_| i64::try_from(&prop).map(|v| v.max(0) as u64))
        .context("MF_PD_DURATION no es un entero válido")?;

    let mut width = 0u32;
    let mut height = 0u32;
    let mut fps = 0.0f32;

    for idx in 0..6u32 {
        let Ok(native_type): windows::core::Result<IMFMediaType> =
            (unsafe { reader.GetNativeMediaType(idx, 0) })
        else {
            break;
        };
        if unsafe { native_type.GetGUID(&MF_MT_MAJOR_TYPE) }.ok() == Some(MFMediaType_Video) {
            if let Ok(packed) = unsafe { native_type.GetUINT64(&MF_MT_FRAME_SIZE) } {
                let (w, h) = unpack_u32_pair(packed);
                width = w;
                height = h;
            }
            if let Ok(packed_fps) = unsafe { native_type.GetUINT64(&MF_MT_FRAME_RATE) } {
                let (num, den) = unpack_u32_pair(packed_fps);
                if num > 0 && den > 0 {
                    fps = num as f32 / den as f32;
                }
            }
            break;
        }
    }

    Ok(ClipMetadata {
        duration_secs: duration_100ns as f64 / 10_000_000.0,
        width,
        height,
        fps,
    })
}

/// Sesión persistente de vista previa CRT que mantiene `IMFStartup` y el último `IMFSourceReader`
/// abiertos para hacer *scrubbing* instantáneo al arrastrar `[IN]` / `[OUT]`.
#[allow(dead_code)]
pub struct PreviewSession {
    mf_started: bool,
    open_clip: Option<OpenPreviewReader>,
}

#[allow(dead_code)]
struct OpenPreviewReader {
    path: PathBuf,
    max_w: u32,
    max_h: u32,
    reader: IMFSourceReader,
    video_idx: u32,
    actual_w: u32,
    actual_h: u32,
    default_stride: i32,
}

#[allow(dead_code)]
impl PreviewSession {
    pub fn new() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup (PreviewSession) falló")?;
        Ok(Self {
            mf_started: true,
            open_clip: None,
        })
    }

    pub fn probe_metadata(&self, path: &Path) -> Result<ClipMetadata> {
        probe_metadata_inner(path)
    }

    pub fn extract_frame(
        &mut self,
        path: &Path,
        time_secs: f64,
        max_w: u32,
        max_h: u32,
    ) -> Result<PreviewFrame> {
        let reuse = self
            .open_clip
            .as_ref()
            .is_some_and(|c| c.path == path && c.max_w == max_w && c.max_h == max_h);
        if !reuse {
            self.open_clip = None;
            let opened = open_preview_reader(path, max_w, max_h)?;
            self.open_clip = Some(opened);
        }
        let clip = self.open_clip.as_ref().unwrap();
        read_preview_frame_from_reader(
            &clip.reader,
            clip.video_idx,
            clip.actual_w,
            clip.actual_h,
            clip.default_stride,
            time_secs,
        )
    }
}

impl Drop for PreviewSession {
    fn drop(&mut self) {
        self.open_clip = None;
        if self.mf_started {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
}

/// Extrae un único frame de vista previa RGBA8 en `time_secs` (escalado a `max_w x max_h`)
/// usando Media Foundation nativo sin dependencias externas.
#[allow(dead_code)]
pub fn extract_mp4_frame_rgba(
    path: &Path,
    time_secs: f64,
    max_w: u32,
    max_h: u32,
) -> Result<PreviewFrame> {
    unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup (preview) falló")?;
    let res = extract_frame_inner(path, time_secs, max_w, max_h);
    unsafe {
        let _ = MFShutdown();
    }
    res
}

#[allow(dead_code)]
fn extract_frame_inner(
    path: &Path,
    time_secs: f64,
    max_w: u32,
    max_h: u32,
) -> Result<PreviewFrame> {
    let opened = open_preview_reader(path, max_w, max_h)?;
    read_preview_frame_from_reader(
        &opened.reader,
        opened.video_idx,
        opened.actual_w,
        opened.actual_h,
        opened.default_stride,
        time_secs,
    )
}

fn open_preview_reader(path: &Path, max_w: u32, max_h: u32) -> Result<OpenPreviewReader> {
    let mut attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attrs, 2) }?;
    let attrs = attrs.context("MFCreateAttributes (preview) devolvió None")?;
    unsafe {
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    }

    let url = HSTRING::from(path.as_os_str());
    let reader: IMFSourceReader = unsafe { MFCreateSourceReaderFromURL(&url, Some(&attrs)) }
        .with_context(|| format!("No se pudo abrir {} para vista previa", path.display()))?;

    let mut video_idx: Option<u32> = None;
    let mut src_w = 1280u32;
    let mut src_h = 720u32;

    for idx in 0..8u32 {
        let Ok(native_type): windows::core::Result<IMFMediaType> =
            (unsafe { reader.GetNativeMediaType(idx, 0) })
        else {
            break;
        };
        let major = unsafe { native_type.GetGUID(&MF_MT_MAJOR_TYPE) }.ok();
        if major == Some(MFMediaType_Video) && video_idx.is_none() {
            video_idx = Some(idx);
            if let Ok(packed) = unsafe { native_type.GetUINT64(&MF_MT_FRAME_SIZE) } {
                let (w, h) = unpack_u32_pair(packed);
                if w > 0 && h > 0 {
                    src_w = w;
                    src_h = h;
                }
            }
            unsafe { reader.SetStreamSelection(idx, true) }?;
        } else {
            let _ = unsafe { reader.SetStreamSelection(idx, false) };
        }
    }

    let video_idx = video_idx.context("El archivo no tiene pista de video")?;

    // Calcular dimensiones de preview manteniendo proporción y alineadas a pares.
    let scale = (max_w as f64 / src_w as f64)
        .min(max_h as f64 / src_h as f64)
        .min(1.0);
    let out_w = (((src_w as f64 * scale).round() as u32) & !1).max(64);
    let out_h = (((src_h as f64 * scale).round() as u32) & !1).max(36);

    let rgb_type = unsafe { MFCreateMediaType() }?;
    unsafe {
        rgb_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        rgb_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        rgb_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(out_w, out_h))?;
        reader.SetCurrentMediaType(video_idx, None, &rgb_type)?;
    }

    // Consultar dimensiones y stride reales negociados tras SetCurrentMediaType.
    let current_type = unsafe { reader.GetCurrentMediaType(video_idx) }?;
    let (actual_w, actual_h) = unsafe { current_type.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map(unpack_u32_pair)
        .unwrap_or((out_w, out_h));
    let default_stride = unsafe { current_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|u| u as i32)
        .unwrap_or((actual_w * 4) as i32);

    Ok(OpenPreviewReader {
        path: path.to_path_buf(),
        max_w,
        max_h,
        reader,
        video_idx,
        actual_w,
        actual_h,
        default_stride,
    })
}

fn read_preview_frame_from_reader(
    reader: &IMFSourceReader,
    video_idx: u32,
    actual_w: u32,
    actual_h: u32,
    default_stride: i32,
    time_secs: f64,
) -> Result<PreviewFrame> {
    let target_100ns = (time_secs.max(0.0) * 10_000_000.0) as i64;
    let pos = PROPVARIANT::from(target_100ns.max(0));
    let _ = unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &pos) };

    let mut chosen_sample: Option<(IMFSample, i64)> = None;
    for _ in 0..90 {
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample_opt: Option<IMFSample> = None;
        unsafe {
            reader.ReadSample(
                video_idx,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample_opt),
            )
        }
        .context("ReadSample (preview) falló")?;

        if let Some(sample) = sample_opt {
            chosen_sample = Some((sample, ts));
            if ts + 250_000 >= target_100ns {
                break;
            }
        }
        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
    }

    let (sample, actual_ts) = chosen_sample.context("No se pudo decodificar ningún frame")?;
    let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }?;

    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut cur_len = 0u32;
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut cur_len)) }?;

    let w = actual_w as usize;
    let h = actual_h as usize;
    let row_stride = default_stride.unsigned_abs().max((w * 4) as u32) as usize;
    let flip_y = default_stride < 0;
    let mut rgba = vec![0u8; w * h * 4];

    if !ptr.is_null() && cur_len as usize >= row_stride * h {
        let src = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize) };
        for y in 0..h {
            let src_y = if flip_y { h - 1 - y } else { y };
            let src_row = &src[src_y * row_stride..src_y * row_stride + w * 4];
            let dst_row = &mut rgba[y * w * 4..(y + 1) * w * 4];
            for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                // RGB32 en Windows es BGRA/BGRX little-endian.
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
        }
    }

    unsafe {
        let _ = buffer.Unlock();
    }

    Ok(PreviewFrame {
        width: actual_w,
        height: actual_h,
        rgba,
        actual_time_secs: actual_ts.max(0) as f64 / 10_000_000.0,
    })
}

/// Recorta un clip `.mp4` entre `[start_secs, end_secs]` en modo **passthrough (sin recodificar)**
/// usando `IMFSourceReader` → `IMFSinkWriter`.
///
/// Conserva la calidad original al 100% (H.264 + AAC) y tarda pocos milisegundos.
/// Nota: el inicio se alinea automáticamente al Keyframe (IDR) más cercano <= `start_secs`.
pub fn trim_mp4_passthrough(
    input_path: &Path,
    output_path: &Path,
    start_secs: f64,
    end_secs: f64,
) -> Result<TrimStats> {
    if end_secs <= start_secs + 0.1 {
        bail!("El tiempo final debe ser mayor al inicial (al menos 0.1 s)");
    }

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let t0 = Instant::now();
    unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup (trimmer) falló")?;
    let res = trim_inner(input_path, output_path, start_secs, end_secs);
    unsafe {
        let _ = MFShutdown();
    }

    let input_bytes = std::fs::metadata(input_path).map(|m| m.len()).unwrap_or(0);
    let duration_secs = res?;
    let output_bytes = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    info!(
        input = %input_path.display(),
        output = %output_path.display(),
        start_secs = format!("{start_secs:.2}"),
        end_secs = format!("{end_secs:.2}"),
        duration_secs = format!("{duration_secs:.2}"),
        size_kb = output_bytes / 1024,
        elapsed_ms = format!("{elapsed_ms:.1}"),
        "Clip MP4 recortado sin recodificar"
    );

    Ok(TrimStats {
        output_path: output_path.to_path_buf(),
        input_bytes,
        output_bytes,
        duration_secs,
        elapsed_ms,
    })
}

/// Comprime un clip `.mp4` de forma suave y de alta fidelidad (~35% menos peso):
/// preserva la resolución nativa y los FPS originales (ej. 1080p/2K a 60 FPS) con audio AAC 160 kbps.
pub fn compress_mp4_balanced(
    input_path: &Path,
    output_path: &Path,
    start_secs: f64,
    end_secs: f64,
) -> Result<TrimStats> {
    if end_secs <= start_secs + 0.2 {
        bail!("El rango a comprimir debe durar al menos 0.2 segundos");
    }
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let input_bytes = std::fs::metadata(input_path).map(|m| m.len()).unwrap_or(0);
    let meta = probe_mp4_metadata(input_path).unwrap_or(ClipMetadata {
        duration_secs: (end_secs - start_secs).max(1.0),
        width: 1920,
        height: 1080,
        fps: 60.0,
    });

    let t0 = Instant::now();
    unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup (balanced compressor) falló")?;

    let clip_dur = (end_secs - start_secs).max(0.5);
    let audio_bps = 160_000u32;

    // Objetivo: ~35% menos peso (65% del tamaño proporcional del rango seleccionado).
    let range_ratio = (clip_dur / meta.duration_secs.max(clip_dur)).clamp(0.05, 1.0);
    let range_input_bytes = (input_bytes as f64 * range_ratio).max(512_000.0);
    let target_bytes = (range_input_bytes * 0.65) as u64;

    let raw_video_bps = ((target_bytes as f64 * 8.0 * 0.94) / clip_dur) - audio_bps as f64;
    let video_bps = (raw_video_bps as u32).clamp(1_500_000, 18_000_000);
    let expected_max_bytes = (range_input_bytes * 0.78) as u64;

    let mut pass_res = compress_pass_with_opts(
        input_path,
        output_path,
        video_bps,
        audio_bps,
        start_secs,
        end_secs,
        true,
        true, // preservar FPS originales (60 fps)
        None, // preservar resolución nativa sin reescalar
    );

    if pass_res.is_err() {
        warn!("Compresión GPU falló; reintentando en software CBR");
        pass_res = compress_pass_with_opts(
            input_path,
            output_path,
            video_bps,
            audio_bps,
            start_secs,
            end_secs,
            false,
            true,
            None,
        );
    }

    // Si el driver GPU excedió el objetivo (~35% menos), rehacer con CBR estricto en software.
    if pass_res.is_ok() {
        let actual_bytes = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
        if actual_bytes > expected_max_bytes {
            warn!(
                target_mb = format!("{:.1}", target_bytes as f64 / (1024.0 * 1024.0)),
                actual_mb = format!("{:.1}", actual_bytes as f64 / (1024.0 * 1024.0)),
                "Compresión GPU excedió el ~35% objetivo; aplicando pasada CBR precisa"
            );
            let _ = std::fs::remove_file(output_path);
            pass_res = compress_pass_with_opts(
                input_path,
                output_path,
                video_bps,
                audio_bps,
                start_secs,
                end_secs,
                false,
                true,
                None,
            );
        }
    }

    unsafe {
        let _ = MFShutdown();
    }

    let duration_secs = pass_res?;
    let output_bytes = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    info!(
        input = %input_path.display(),
        output = %output_path.display(),
        in_mb = format!("{:.2}", input_bytes as f64 / (1024.0 * 1024.0)),
        out_mb = format!("{:.2}", output_bytes as f64 / (1024.0 * 1024.0)),
        video_mbps = format!("{:.1}", video_bps as f64 / 1_000_000.0),
        elapsed_ms = format!("{elapsed_ms:.0}"),
        "Clip MP4 comprimido (~35% menos peso)"
    );

    Ok(TrimStats {
        output_path: output_path.to_path_buf(),
        input_bytes,
        output_bytes,
        duration_secs,
        elapsed_ms,
    })
}

/// Comprime un clip `.mp4` (o el rango `[start_secs, end_secs]`) usando Media Foundation con
/// **rate-control CBR estricto vía `ICodecAPI`** + reducción inteligente de FPS/resolución y
/// **verificación automática del tamaño final**: si el driver GPU excede el límite, aplica
/// automáticamente una pasada correctiva para garantizar quedar por debajo de `target_mb`.
#[allow(dead_code)]
pub fn compress_mp4_to_target_mb(
    input_path: &Path,
    output_path: &Path,
    target_mb: f64,
    start_secs: f64,
    end_secs: f64,
) -> Result<TrimStats> {
    if end_secs <= start_secs + 0.2 {
        bail!("El rango a comprimir debe durar al menos 0.2 segundos");
    }
    if target_mb < 1.0 {
        bail!("El tamaño objetivo debe ser al menos 1 MB");
    }
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let input_bytes = std::fs::metadata(input_path).map(|m| m.len()).unwrap_or(0);
    let t0 = Instant::now();
    unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup (compressor) falló")?;

    let target_bytes = (target_mb * 1024.0 * 1024.0) as u64;
    let clip_dur = (end_secs - start_secs).max(0.5);
    let audio_bps: u32 = if (target_mb * 8.0) / clip_dur < 1.3 {
        64_000
    } else {
        96_000
    };
    let initial_video_bps = calc_target_video_bps(target_mb, clip_dur, audio_bps);

    // Pasada 1: GPU Hardware con CBR explícito vía ICodecAPI.
    let mut pass_res = compress_pass(
        input_path,
        output_path,
        initial_video_bps,
        audio_bps,
        start_secs,
        end_secs,
        true,
    );

    // Si la pasada por hardware falló en este driver, reintentar directamente en software CBR.
    if pass_res.is_err() {
        warn!("Compresión por hardware falló; reintentando con encoder CBR por software");
        pass_res = compress_pass(
            input_path,
            output_path,
            initial_video_bps,
            audio_bps,
            start_secs,
            end_secs,
            false,
        );
    }

    // Verificación estricta de tamaño: algunos drivers GPU (p. ej. AMD AMF a bitrates bajos)
    // tienen un piso de bitrate interno y superan `target_mb`. Si excede el límite, ejecutamos
    // una pasada correctiva con encoder CBR estricto en software para garantizar `< target_mb`.
    if pass_res.is_ok() {
        let actual_bytes = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
        if actual_bytes > target_bytes {
            let ratio = (target_bytes as f64 / actual_bytes as f64) * 0.86;
            let corrected_bps = ((initial_video_bps as f64) * ratio)
                .clamp(220_000.0, initial_video_bps as f64 * 0.90)
                as u32;
            warn!(
                target_mb = format!("{target_mb:.1}"),
                actual_mb = format!("{:.2}", actual_bytes as f64 / (1024.0 * 1024.0)),
                corrected_kbps = corrected_bps / 1000,
                "Pasada GPU excedió el límite de tamaño; aplicando pasada correctiva CBR estricta"
            );
            let _ = std::fs::remove_file(output_path);
            pass_res = compress_pass(
                input_path,
                output_path,
                corrected_bps,
                audio_bps.min(96_000),
                start_secs,
                end_secs,
                false,
            );
        }
    }

    unsafe {
        let _ = MFShutdown();
    }

    let duration_secs = pass_res?;
    let output_bytes = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    info!(
        input = %input_path.display(),
        output = %output_path.display(),
        target_mb = format!("{target_mb:.1}"),
        actual_mb = format!("{:.2}", output_bytes as f64 / (1024.0 * 1024.0)),
        duration_secs = format!("{duration_secs:.2}"),
        elapsed_ms = format!("{elapsed_ms:.0}"),
        "Clip MP4 comprimido con verificación de tamaño"
    );

    Ok(TrimStats {
        output_path: output_path.to_path_buf(),
        input_bytes,
        output_bytes,
        duration_secs,
        elapsed_ms,
    })
}

fn calc_target_video_bps(target_mb: f64, clip_dur_secs: f64, audio_bps: u32) -> u32 {
    // 14% de margen de seguridad para contenedor MP4 y picos de keyframes IDR.
    let total_budget_bits = target_mb * 1024.0 * 1024.0 * 8.0 * 0.86;
    let raw_video_bps = (total_budget_bits / clip_dur_secs.max(0.5)) - audio_bps as f64;
    (raw_video_bps.max(250_000.0) as u32).clamp(250_000, 12_000_000)
}

fn compress_pass(
    input_path: &Path,
    output_path: &Path,
    video_bitrate_bps: u32,
    audio_bps: u32,
    start_secs: f64,
    end_secs: f64,
    use_hardware_encoder: bool,
) -> Result<f64> {
    compress_pass_with_opts(
        input_path,
        output_path,
        video_bitrate_bps,
        audio_bps,
        start_secs,
        end_secs,
        use_hardware_encoder,
        false,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn compress_pass_with_opts(
    input_path: &Path,
    output_path: &Path,
    video_bitrate_bps: u32,
    audio_bps: u32,
    start_secs: f64,
    end_secs: f64,
    use_hardware_encoder: bool,
    preserve_fps: bool,
    max_height_override: Option<u32>,
) -> Result<f64> {
    let mut reader_attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut reader_attrs, 2) }?;
    let reader_attrs = reader_attrs.context("MFCreateAttributes (reader) devolvió None")?;
    unsafe {
        reader_attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        reader_attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    }

    let in_url = HSTRING::from(input_path.as_os_str());
    let reader: IMFSourceReader =
        unsafe { MFCreateSourceReaderFromURL(&in_url, Some(&reader_attrs)) }
            .with_context(|| format!("No se pudo abrir {}", input_path.display()))?;

    // Descubrir pistas nativas de video y audio.
    let mut video_in_idx: Option<u32> = None;
    let mut audio_in_idx: Option<u32> = None;
    let mut src_w = 1920u32;
    let mut src_h = 1080u32;
    let mut src_fps_num = 60u32;
    let mut src_fps_den = 1u32;

    for idx in 0..8u32 {
        let Ok(native_type): windows::core::Result<IMFMediaType> =
            (unsafe { reader.GetNativeMediaType(idx, 0) })
        else {
            break;
        };
        let Ok(major) = (unsafe { native_type.GetGUID(&MF_MT_MAJOR_TYPE) }) else {
            continue;
        };

        if major == MFMediaType_Video && video_in_idx.is_none() {
            video_in_idx = Some(idx);
            if let Ok(packed_size) = unsafe { native_type.GetUINT64(&MF_MT_FRAME_SIZE) } {
                let (w, h) = unpack_u32_pair(packed_size);
                if w > 0 && h > 0 {
                    src_w = w;
                    src_h = h;
                }
            }
            if let Ok(packed_fps) = unsafe { native_type.GetUINT64(&MF_MT_FRAME_RATE) } {
                let (n, d) = unpack_u32_pair(packed_fps);
                if n > 0 && d > 0 {
                    src_fps_num = n;
                    src_fps_den = d;
                }
            }
            unsafe { reader.SetStreamSelection(idx, true) }?;
        } else if major == MFMediaType_Audio && audio_in_idx.is_none() {
            audio_in_idx = Some(idx);
            unsafe { reader.SetStreamSelection(idx, true) }?;
        } else {
            let _ = unsafe { reader.SetStreamSelection(idx, false) };
        }
    }

    let video_in_idx = video_in_idx.context("El archivo MP4 no contiene pista de video")?;

    // Cuando el presupuesto es ajustado (<3.5 Mbps) y NO estamos en modo equilibrado (`preserve_fps`),
    // reducimos de 60 fps a 30 fps para duplicar bits por cuadro.
    let src_fps = src_fps_num as f64 / src_fps_den.max(1) as f64;
    let (out_fps_num, out_fps_den) = if !preserve_fps && src_fps > 35.0 && video_bitrate_bps < 3_500_000 {
        (30u32, 1u32)
    } else {
        (src_fps_num, src_fps_den)
    };
    let out_fps = (out_fps_num as f64 / out_fps_den.max(1) as f64).max(1.0);
    let min_frame_step_100ns = if out_fps < src_fps * 0.85 {
        ((10_000_000.0 / out_fps) * 0.90) as i64
    } else {
        0
    };

    // Escalado inteligente (o tope limpio en modo equilibrado).
    let (out_w, out_h) = if let Some(max_h) = max_height_override {
        if src_h > max_h {
            let scaled_w = ((src_w as f64 * max_h as f64) / src_h as f64).round() as u32;
            ((scaled_w & !1).max(320), (max_h & !1).max(240))
        } else {
            (src_w & !1, src_h & !1)
        }
    } else if preserve_fps {
        (src_w & !1, src_h & !1)
    } else {
        choose_compressed_dimensions(src_w, src_h, video_bitrate_bps)
    };

    // Configurar lector de video para entregar NV12 escalado en GPU.
    let reader_nv12 = unsafe { MFCreateMediaType() }?;
    unsafe {
        reader_nv12.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        reader_nv12.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        reader_nv12.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(out_w, out_h))?;
        reader.SetCurrentMediaType(video_in_idx, None, &reader_nv12)?;
    }

    // Consultar stride y altura alineada reales que el decodificador entrega en NV12
    // (ej. 1920x1088 con 8 filas de padding entre plano Y y plano UV).
    let (mut reader_stride, mut reader_aligned_h) = query_nv12_layout(&reader, video_in_idx, out_w, out_h);

    let mut writer_attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut writer_attrs, 2) }?;
    let writer_attrs = writer_attrs.context("MFCreateAttributes (writer) devolvió None")?;
    unsafe {
        writer_attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        writer_attrs.SetUINT32(
            &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
            u32::from(use_hardware_encoder),
        )?;
    }

    let out_url = HSTRING::from(output_path.as_os_str());
    let writer: IMFSinkWriter =
        unsafe { MFCreateSinkWriterFromURL(&out_url, None, Some(&writer_attrs)) }
            .with_context(|| format!("No se pudo crear {}", output_path.display()))?;

    // Configurar pista de salida H.264 con matriz BT.709 y rango de video estándar.
    let h264_out = unsafe { MFCreateMediaType() }?;
    unsafe {
        h264_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        h264_out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        h264_out.SetUINT32(&MF_MT_AVG_BITRATE, video_bitrate_bps)?;
        h264_out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        h264_out.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(out_w, out_h))?;
        h264_out.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(out_fps_num, out_fps_den))?;
        h264_out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
        h264_out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        let _ = h264_out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = h264_out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    }

    let video_out_idx = unsafe { writer.AddStream(&h264_out) }
        .context("IMFSinkWriter::AddStream (H264 compress) falló")?;

    let mut enc_params: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut enc_params, 5) }?;
    let enc_params = enc_params.context("MFCreateAttributes (enc_params) devolvió None")?;
    let gop_frames = (out_fps.round() as u32).max(15) * 2;
    unsafe {
        let _ = enc_params.SetUINT32(
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
        );
        let _ = enc_params.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, video_bitrate_bps);
        let _ = enc_params.SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, video_bitrate_bps);
        let _ = enc_params.SetUINT32(&CODECAPI_AVEncMPVGOPSize, gop_frames);
        let _ = enc_params.SetUINT32(&CODECAPI_AVEncMPVDefaultBPictureCount, 0);
    }

    // Entrada NV12 estrictamente empaquetada (stride == out_w, sin filas de padding entre Y y UV).
    let nv12_in = unsafe { MFCreateMediaType() }?;
    unsafe {
        nv12_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        nv12_in.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        nv12_in.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        nv12_in.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(out_w, out_h))?;
        nv12_in.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(out_fps_num, out_fps_den))?;
        nv12_in.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
        nv12_in.SetUINT32(&MF_MT_DEFAULT_STRIDE, out_w)?;
        let _ = nv12_in.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = nv12_in.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
        writer.SetInputMediaType(video_out_idx, &nv12_in, Some(&enc_params))?;
    }

    // Reforzar los mismos valores vía ICodecAPI directo sobre el MFT interno del SinkWriter.
    let mut raw_api: *mut std::ffi::c_void = std::ptr::null_mut();
    if unsafe {
        writer.GetServiceForStream(
            video_out_idx,
            &GUID::zeroed(),
            &ICodecAPI::IID,
            &mut raw_api,
        )
    }
    .is_ok()
        && !raw_api.is_null()
    {
        let codec_api = unsafe { ICodecAPI::from_raw(raw_api) };
        let _ = unsafe {
            codec_api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            )
        };
        let _ = unsafe {
            codec_api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(video_bitrate_bps),
            )
        };
        let _ = unsafe {
            codec_api.SetValue(
                &CODECAPI_AVEncCommonMaxBitRate,
                &VARIANT::from(video_bitrate_bps),
            )
        };
        let _ = unsafe {
            codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop_frames))
        };
        let _ = unsafe {
            codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32))
        };
    }

    // Configurar audio: recodificar a AAC 96/128/160 kbps (valores válidos para MF AAC MFT).
    let aac_bytes_per_sec = match audio_bps {
        ..=112_000 => 12_000u32,
        112_001..=144_000 => 16_000u32,
        _ => 20_000u32,
    };
    let audio_out_idx = if let Some(a_in) = audio_in_idx {
        let pcm_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            pcm_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            pcm_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            pcm_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            pcm_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)?;
            pcm_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            pcm_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            pcm_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 48_000 * 4)?;
            reader.SetCurrentMediaType(a_in, None, &pcm_type)?;
        }

        let aac_out = unsafe { MFCreateMediaType() }?;
        unsafe {
            aac_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            aac_out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            aac_out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            aac_out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)?;
            aac_out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            aac_out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, aac_bytes_per_sec)?;
            aac_out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            aac_out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
        }

        let out_idx = unsafe { writer.AddStream(&aac_out) }
            .context("IMFSinkWriter::AddStream (AAC 96k) falló")?;
        unsafe { writer.SetInputMediaType(out_idx, &pcm_type, None) }
            .context("IMFSinkWriter::SetInputMediaType (PCM) falló")?;
        Some(out_idx)
    } else {
        None
    };

    let start_100ns = (start_secs.max(0.0) * 10_000_000.0) as i64;
    let end_100ns = (end_secs * 10_000_000.0) as i64;

    if start_100ns > 0 {
        let pos = PROPVARIANT::from(start_100ns);
        let _ = unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &pos) };
    }

    unsafe { writer.BeginWriting() }.context("IMFSinkWriter::BeginWriting (compress) falló")?;

    let nominal_out_dur = (10_000_000.0 / out_fps).round() as i64;
    let mut base_pts: Option<i64> = None;
    let mut last_v_pts: i64 = -1;
    let mut last_a_pts: i64 = -1;
    let mut max_rel_pts: i64 = 0;
    let mut video_done = false;
    let mut audio_done = audio_out_idx.is_none();

    while !(video_done && audio_done) {
        let mut actual_stream = 0u32;
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample_opt: Option<IMFSample> = None;

        unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_ANY_STREAM.0 as u32,
                0,
                Some(&mut actual_stream),
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample_opt),
            )
        }
        .context("IMFSourceReader::ReadSample (compress) falló")?;

        if (flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0
            && actual_stream == video_in_idx
        {
            let (new_stride, new_h) = query_nv12_layout(&reader, video_in_idx, out_w, out_h);
            reader_stride = new_stride;
            reader_aligned_h = new_h;
        }

        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            if actual_stream == video_in_idx {
                video_done = true;
            } else if Some(actual_stream) == audio_in_idx {
                audio_done = true;
            } else {
                break;
            }
            continue;
        }

        let Some(sample) = sample_opt else {
            continue;
        };

        let is_video = actual_stream == video_in_idx;
        if timestamp < start_100ns {
            continue;
        }
        if timestamp > end_100ns {
            if is_video {
                video_done = true;
            } else {
                audio_done = true;
            }
            continue;
        }

        let origin = match base_pts {
            Some(b) => b,
            None => {
                if !is_video {
                    continue;
                }
                base_pts = Some(timestamp);
                timestamp
            }
        };

        if timestamp < origin {
            continue;
        }

        let mut rel_pts = timestamp - origin;
        let dur = unsafe { sample.GetSampleDuration() }.unwrap_or(nominal_out_dur);

        if is_video {
            // Decimación limpia a 30 fps cuando el presupuesto de bitrate lo requiere.
            if min_frame_step_100ns > 0 && last_v_pts >= 0 && (rel_pts - last_v_pts) < min_frame_step_100ns {
                continue;
            }
            if rel_pts <= last_v_pts {
                rel_pts = last_v_pts + 1;
            }
            let out_dur = if min_frame_step_100ns > 0 {
                nominal_out_dur
            } else {
                dur.max(nominal_out_dur)
            };
            // Reempaquetar NV12 eliminando padding de stride/altura (ej. 1088 -> 1080)
            // para evitar barras verdes superiores y corrimientos de color en el plano UV.
            let packed_sample = repack_nv12_tight(
                &sample,
                out_w as usize,
                out_h as usize,
                reader_stride,
                reader_aligned_h,
                rel_pts,
                out_dur,
            )?;
            unsafe { writer.WriteSample(video_out_idx, &packed_sample) }
                .context("IMFSinkWriter::WriteSample (video compress) falló")?;
            last_v_pts = rel_pts;
            max_rel_pts = max_rel_pts.max(rel_pts + out_dur);
        } else if let Some(a_out) = audio_out_idx {
            if rel_pts <= last_a_pts {
                rel_pts = last_a_pts + 1;
            }
            unsafe { sample.SetSampleTime(rel_pts) }?;
            unsafe { writer.WriteSample(a_out, &sample) }
                .context("IMFSinkWriter::WriteSample (audio compress) falló")?;
            last_a_pts = rel_pts;
        }
    }

    if base_pts.is_none() {
        bail!("No se pudieron leer frames en el rango seleccionado para comprimir");
    }

    unsafe { writer.Finalize() }.context("IMFSinkWriter::Finalize (compress) falló")?;

    Ok(max_rel_pts as f64 / 10_000_000.0)
}

/// Consulta el stride real y la altura física (alineada a macrobloques, ej. 1088 para 1080p)
/// que entrega el decodificador NV12 de `IMFSourceReader`.
fn query_nv12_layout(
    reader: &IMFSourceReader,
    stream_idx: u32,
    fallback_w: u32,
    fallback_h: u32,
) -> (usize, usize) {
    let Ok(mt) = (unsafe { reader.GetCurrentMediaType(stream_idx) }) else {
        return (fallback_w as usize, fallback_h as usize);
    };
    let (w, h) = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map(unpack_u32_pair)
        .unwrap_or((fallback_w, fallback_h));
    let stride = unsafe { mt.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|u| (u as i32).unsigned_abs() as usize)
        .unwrap_or(w as usize)
        .max(fallback_w as usize);
    (stride, (h as usize).max(fallback_h as usize))
}

/// Copia un `IMFSample` NV12 (posiblemente con `IMF2DBuffer`, stride > width o altura alineada a 16
/// como `1920x1088`) a un `IMFSample` NV12 estrictamente contiguo (`width x height` sin padding).
fn repack_nv12_tight(
    src_sample: &IMFSample,
    width: usize,
    height: usize,
    hint_stride: usize,
    hint_aligned_h: usize,
    pts_100ns: i64,
    dur_100ns: i64,
) -> Result<IMFSample> {
    let dst_y_bytes = width * height;
    let dst_uv_rows = height / 2;
    let dst_total = dst_y_bytes + width * dst_uv_rows;

    let out_sample = unsafe { MFCreateSample() }.context("MFCreateSample (nv12 repack) falló")?;
    let out_buf = unsafe { MFCreateMemoryBuffer(dst_total as u32) }
        .context("MFCreateMemoryBuffer (nv12 repack) falló")?;

    let mut dst_ptr: *mut u8 = std::ptr::null_mut();
    unsafe { out_buf.Lock(&mut dst_ptr, None, None) }?;

    let copy_res: Result<()> = (|| {
        let src_buf: IMFMediaBuffer = unsafe { src_sample.ConvertToContiguousBuffer() }
            .context("ConvertToContiguousBuffer (nv12) falló")?;

        // Preferir IMF2DBuffer::Lock2D para obtener el pitch exacto de hardware; fallback a Lock.
        if let Ok(buf2d) = src_buf.cast::<IMF2DBuffer>() {
            let mut scan0: *mut u8 = std::ptr::null_mut();
            let mut pitch: i32 = 0;
            if unsafe { buf2d.Lock2D(&mut scan0, &mut pitch) }.is_ok() {
                let res = (|| {
                    let src_stride = pitch.unsigned_abs().max(width as u32) as usize;
                    let total_len = unsafe { buf2d.GetContiguousLength() }
                        .unwrap_or((src_stride * hint_aligned_h * 3 / 2) as u32)
                        as usize;
                    // Calcular la altura física exacta del plano Y: total_len == src_stride * plane_h * 3 / 2.
                    let inferred_h = if src_stride > 0 {
                        (total_len * 2) / (src_stride * 3)
                    } else {
                        hint_aligned_h
                    };
                    let plane_h = inferred_h.max(hint_aligned_h).max(height);
                    let src_uv_base = src_stride * plane_h;

                    unsafe {
                        // Copiar plano Y (height filas de `width` bytes).
                        for row in 0..height {
                            std::ptr::copy_nonoverlapping(
                                scan0.add(row * src_stride),
                                dst_ptr.add(row * width),
                                width,
                            );
                        }
                        // Copiar plano UV saltando las filas de padding entre `height` y `plane_h`.
                        for row in 0..dst_uv_rows {
                            std::ptr::copy_nonoverlapping(
                                scan0.add(src_uv_base + row * src_stride),
                                dst_ptr.add(dst_y_bytes + row * width),
                                width,
                            );
                        }
                    }
                    Ok(())
                })();
                unsafe {
                    let _ = buf2d.Unlock2D();
                }
                return res;
            }
        }

        let mut src_ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        unsafe { src_buf.Lock(&mut src_ptr, None, Some(&mut cur_len)) }?;
        let res = (|| {
            let src_stride = hint_stride.max(width);
            let inferred_h = (cur_len as usize * 2) / (src_stride * 3);
            let plane_h = inferred_h.max(hint_aligned_h).max(height);
            let src_uv_base = src_stride * plane_h;
            if (cur_len as usize) < src_uv_base + (dst_uv_rows.saturating_sub(1)) * src_stride + width {
                // Fallback seguro si el buffer es exactamente contiguo.
                let copy_len = (cur_len as usize).min(dst_total);
                unsafe {
                    std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, copy_len);
                }
            } else {
                unsafe {
                    for row in 0..height {
                        std::ptr::copy_nonoverlapping(
                            src_ptr.add(row * src_stride),
                            dst_ptr.add(row * width),
                            width,
                        );
                    }
                    for row in 0..dst_uv_rows {
                        std::ptr::copy_nonoverlapping(
                            src_ptr.add(src_uv_base + row * src_stride),
                            dst_ptr.add(dst_y_bytes + row * width),
                            width,
                        );
                    }
                }
            }
            Ok(())
        })();
        unsafe {
            let _ = src_buf.Unlock();
        }
        res
    })();

    unsafe {
        let _ = out_buf.Unlock();
    }
    copy_res?;

    unsafe {
        out_buf.SetCurrentLength(dst_total as u32)?;
        out_sample.AddBuffer(&out_buf)?;
        out_sample.SetSampleTime(pts_100ns)?;
        out_sample.SetSampleDuration(dur_100ns)?;
    }
    Ok(out_sample)
}

fn choose_compressed_dimensions(src_w: u32, src_h: u32, video_bitrate_bps: u32) -> (u32, u32) {
    let target_h = if video_bitrate_bps < 850_000 && src_h > 480 {
        480
    } else if video_bitrate_bps < 1_500_000 && src_h > 540 {
        540
    } else if video_bitrate_bps < 3_200_000 && src_h > 720 {
        720
    } else if video_bitrate_bps < 6_000_000 && src_h > 1080 {
        1080
    } else {
        src_h
    };

    if target_h >= src_h {
        return (src_w & !1, src_h & !1);
    }

    let scaled_w = ((src_w as f64 * target_h as f64) / src_h as f64).round() as u32;
    ((scaled_w & !1).max(320), (target_h & !1).max(240))
}

fn trim_inner(
    input_path: &Path,
    output_path: &Path,
    start_secs: f64,
    end_secs: f64,
) -> Result<f64> {
    let in_url = HSTRING::from(input_path.as_os_str());
    let reader: IMFSourceReader = unsafe { MFCreateSourceReaderFromURL(&in_url, None) }
        .with_context(|| format!("No se pudo abrir {}", input_path.display()))?;

    let mut attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attrs, 2) }?;
    let attrs = attrs.context("MFCreateAttributes devolvió None")?;
    unsafe {
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    }

    let out_url = HSTRING::from(output_path.as_os_str());
    let writer: IMFSinkWriter = unsafe { MFCreateSinkWriterFromURL(&out_url, None, Some(&attrs)) }
        .with_context(|| format!("No se pudo crear {}", output_path.display()))?;

    let mut stream_map: HashMap<u32, u32> = HashMap::new();
    let mut video_in_idx: Option<u32> = None;
    let mut audio_in_idx: Option<u32> = None;

    for idx in 0..8u32 {
        let Ok(native_type): windows::core::Result<IMFMediaType> =
            (unsafe { reader.GetNativeMediaType(idx, 0) })
        else {
            break;
        };

        let Ok(major) = (unsafe { native_type.GetGUID(&MF_MT_MAJOR_TYPE) }) else {
            continue;
        };

        let should_include = if major == MFMediaType_Video && video_in_idx.is_none() {
            video_in_idx = Some(idx);
            true
        } else if major == MFMediaType_Audio && audio_in_idx.is_none() {
            audio_in_idx = Some(idx);
            true
        } else {
            false
        };

        if should_include {
            unsafe { reader.SetStreamSelection(idx, true) }?;
            let out_idx = unsafe { writer.AddStream(&native_type) }
                .context("IMFSinkWriter::AddStream en trimmer falló")?;
            unsafe { writer.SetInputMediaType(out_idx, &native_type, None) }
                .context("IMFSinkWriter::SetInputMediaType en trimmer falló")?;
            stream_map.insert(idx, out_idx);
        } else {
            let _ = unsafe { reader.SetStreamSelection(idx, false) };
        }
    }

    let video_in_idx = video_in_idx.context("El archivo MP4 no contiene pista de video")?;

    let start_100ns = (start_secs.max(0.0) * 10_000_000.0) as i64;
    let end_100ns = (end_secs * 10_000_000.0) as i64;

    if start_100ns > 0 {
        let pos = PROPVARIANT::from(start_100ns);
        unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &pos) }
            .context("IMFSourceReader::SetCurrentPosition falló")?;
    }

    unsafe { writer.BeginWriting() }.context("IMFSinkWriter::BeginWriting (trimmer) falló")?;

    let mut base_pts: Option<i64> = None;
    let mut last_written_pts: HashMap<u32, i64> = HashMap::new();
    let mut max_rel_pts: i64 = 0;
    let mut video_done = false;
    let mut audio_done = audio_in_idx.is_none();

    while !(video_done && audio_done) {
        let mut actual_stream = 0u32;
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample_opt: Option<IMFSample> = None;

        unsafe {
            reader.ReadSample(
                MF_SOURCE_READER_ANY_STREAM.0 as u32,
                0,
                Some(&mut actual_stream),
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample_opt),
            )
        }
        .context("IMFSourceReader::ReadSample falló")?;

        if (flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            if actual_stream == video_in_idx {
                video_done = true;
            } else if Some(actual_stream) == audio_in_idx {
                audio_done = true;
            } else {
                break;
            }
            continue;
        }

        let Some(sample) = sample_opt else {
            continue;
        };
        let Some(&out_stream) = stream_map.get(&actual_stream) else {
            continue;
        };

        let is_video = actual_stream == video_in_idx;

        if timestamp > end_100ns {
            if is_video {
                video_done = true;
            } else {
                audio_done = true;
            }
            continue;
        }

        let origin = match base_pts {
            Some(b) => b,
            None => {
                if !is_video {
                    continue;
                }
                let clean =
                    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
                if !clean {
                    continue;
                }
                base_pts = Some(timestamp);
                timestamp
            }
        };

        if timestamp < origin {
            continue;
        }

        let mut rel_pts = timestamp - origin;
        if let Some(&prev) = last_written_pts.get(&out_stream) {
            if rel_pts <= prev {
                rel_pts = prev + 1;
            }
        }

        let dur = unsafe { sample.GetSampleDuration() }.unwrap_or(0);
        unsafe { sample.SetSampleTime(rel_pts) }?;
        unsafe { writer.WriteSample(out_stream, &sample) }
            .context("IMFSinkWriter::WriteSample (trimmer) falló")?;

        last_written_pts.insert(out_stream, rel_pts);
        if is_video {
            max_rel_pts = max_rel_pts.max(rel_pts + dur.max(0));
        }
    }

    if base_pts.is_none() {
        bail!("No se encontró ningún keyframe de video en el rango seleccionado");
    }

    unsafe { writer.Finalize() }.context("IMFSinkWriter::Finalize (trimmer) falló")?;

    Ok(max_rel_pts as f64 / 10_000_000.0)
}

/// Construye un nombre de salida único (`clip_<suffix>.mp4`, o `_<suffix>2.mp4` si ya existe).
pub fn suggested_output_with_suffix(input: &Path, suffix: &str) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());

    let first = parent.join(format!("{stem}_{suffix}.mp4"));
    if !first.exists() {
        return first;
    }

    for i in 2..1000 {
        let candidate = parent.join(format!("{stem}_{suffix}{i}.mp4"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

pub fn suggested_trim_output(input: &Path) -> PathBuf {
    suggested_output_with_suffix(input, "trim")
}

#[inline]
const fn pack_u32_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

#[inline]
const fn unpack_u32_pair(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_sample_mp4() -> Option<PathBuf> {
        let clips_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("clips");
        let entries = std::fs::read_dir(&clips_dir).ok()?;
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "mp4"))
    }

    #[test]
    fn trims_existing_sample_clip_if_present() {
        let Some(mp4) = find_sample_mp4() else {
            return;
        };

        let dur = probe_mp4_duration_secs(&mp4).expect("probe_mp4_duration_secs falló");
        assert!(dur > 1.0, "duración inesperada: {dur}");

        let out = std::env::temp_dir().join("bals_trim_unit_test.mp4");
        let _ = std::fs::remove_file(&out);

        let end = (dur - 0.5).max(1.5).min(dur);
        let stats = trim_mp4_passthrough(&mp4, &out, 0.5, end).expect("trim_mp4_passthrough falló");
        assert!(stats.output_bytes > 1024);
        assert!(stats.duration_secs > 0.5);

        let probed_out = probe_mp4_duration_secs(&out).expect("probe en archivo recortado falló");
        assert!(probed_out > 0.5 && probed_out <= dur + 0.5);

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn compresses_existing_sample_clip_under_target() {
        let Some(mp4) = find_sample_mp4() else {
            return;
        };

        let dur = probe_mp4_duration_secs(&mp4).expect("probe falló");
        let out = std::env::temp_dir().join("bals_compress_unit_test.mp4");
        let _ = std::fs::remove_file(&out);

        let stats = compress_mp4_to_target_mb(&mp4, &out, 2.0, 0.0, dur)
            .expect("compress_mp4_to_target_mb falló");
        assert!(stats.output_bytes > 10_000);
        // Debe quedar por debajo del objetivo (2.0 MB + tolerancia pequeña de VBR/contenedor).
        assert!(
            stats.output_bytes < 2_500_000,
            "archivo comprimido demasiado grande: {} bytes",
            stats.output_bytes
        );

        let probed_out = probe_mp4_duration_secs(&out).expect("probe en archivo comprimido falló");
        assert!(probed_out > 1.0);

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn probes_metadata_and_extracts_preview_frame() {
        let Some(mp4) = find_sample_mp4() else {
            return;
        };

        let meta = probe_mp4_metadata(&mp4).expect("probe_mp4_metadata falló");
        assert!(meta.duration_secs > 1.0);
        assert!(meta.width >= 320 && meta.height >= 200);

        let frame = extract_mp4_frame_rgba(&mp4, 0.5, 384, 216)
            .expect("extract_mp4_frame_rgba falló");
        assert!(frame.width >= 64 && frame.width <= 384);
        assert!(frame.height >= 36 && frame.height <= 216);
        assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
    }

    #[test]
    fn compresses_heavy_clip_under_discord_limit_if_present() {
        let Ok(profile) = std::env::var("USERPROFILE") else {
            return;
        };
        let candidate = PathBuf::from(profile)
            .join("Videos")
            .join("Bals")
            .join("bals_20261125_002613.mp4");
        if !candidate.exists() {
            return;
        }

        let meta = probe_mp4_metadata(&candidate).expect("probe_mp4_metadata falló");
        let out = std::env::temp_dir().join("bals_discord_real_test.mp4");
        let _ = std::fs::remove_file(&out);

        let stats = compress_mp4_to_target_mb(&candidate, &out, 9.5, 0.0, meta.duration_secs)
            .expect("compress_mp4_to_target_mb falló");
        let out_mb = stats.output_bytes as f64 / (1024.0 * 1024.0);
        println!(
            "REAL DISCORD TEST: {:.2} MB -> {:.2} MB in {:.0} ms (dur {:.1}s)",
            stats.input_bytes as f64 / (1024.0 * 1024.0),
            out_mb,
            stats.elapsed_ms,
            stats.duration_secs
        );
        assert!(
            out_mb <= 9.9,
            "El archivo comprimido superó el límite de Discord (<10 MB): {out_mb:.2} MB"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn compresses_heavy_clip_balanced_if_present() {
        let clips_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("clips");
        let candidate = std::fs::read_dir(&clips_dir)
            .ok()
            .and_then(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("mp4")))
                    .max_by_key(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
            })
            .or_else(find_sample_mp4);
        let Some(candidate) = candidate else {
            return;
        };

        let meta = probe_mp4_metadata(&candidate).expect("probe_mp4_metadata falló");
        let out = std::env::temp_dir().join("bals_balanced_real_test.mp4");
        let _ = std::fs::remove_file(&out);

        let stats = compress_mp4_balanced(&candidate, &out, 0.0, meta.duration_secs.min(12.0))
            .expect("compress_mp4_balanced falló");
        let out_meta = probe_mp4_metadata(&out).expect("probe salida falló");
        let in_mb = stats.input_bytes as f64 / (1024.0 * 1024.0);
        let out_mb = stats.output_bytes as f64 / (1024.0 * 1024.0);

        // Verificar fidelidad de color y ausencia de barra verde superior comparando frames reales.
        let probe_t = (meta.duration_secs.min(12.0) * 0.4).max(0.5);
        let orig_f = extract_mp4_frame_rgba(&candidate, probe_t, 320, 180).expect("orig frame");
        let comp_f = extract_mp4_frame_rgba(&out, probe_t, 320, 180).expect("comp frame");
        assert_eq!(orig_f.width, comp_f.width);
        assert_eq!(orig_f.height, comp_f.height);
        let top_bytes = (orig_f.width as usize) * 16 * 4;
        let top_mae: f64 = orig_f.rgba[..top_bytes]
            .iter()
            .zip(&comp_f.rgba[..top_bytes])
            .map(|(&a, &b)| (a as i32 - b as i32).abs() as f64)
            .sum::<f64>()
            / top_bytes as f64;
        let full_mae: f64 = orig_f
            .rgba
            .iter()
            .zip(&comp_f.rgba)
            .map(|(&a, &b)| (a as i32 - b as i32).abs() as f64)
            .sum::<f64>()
            / orig_f.rgba.len() as f64;

        println!(
            "REAL BALANCED TEST ({}): {in_mb:.2} MB -> {out_mb:.2} MB ({}x{} @{:.0}fps) in {:.0} ms | top_mae={top_mae:.1} full_mae={full_mae:.1}",
            candidate.file_name().unwrap_or_default().to_string_lossy(),
            out_meta.width,
            out_meta.height,
            out_meta.fps,
            stats.elapsed_ms
        );
        assert!(stats.output_bytes > 50_000);
        assert_eq!(out_meta.width, meta.width);
        assert_eq!(out_meta.height, meta.height);
        assert!(
            top_mae < 18.0 && full_mae < 18.0,
            "Desviación de color / barra verde detectada: top_mae={top_mae:.1}, full_mae={full_mae:.1}"
        );
        let _ = std::fs::remove_file(&out);
    }
}
