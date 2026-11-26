use std::mem::ManuallyDrop;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQuality,
    CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample, IMFTransform,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFShutdown,
    MFStartup, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFMediaType_Video, MFSampleExtension_CleanPoint, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR,
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncCommonRateControlMode_Quality,
    eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, IUnknown, Interface};

use crate::config::{EncoderPreference, VideoConfig};
use crate::d3d::SharedD3DDevice;
use crate::encode::{EncodedVideoPacket, annexb_contains_idr};

/// Resumen de encoders de video H.264 disponibles en el sistema.
#[derive(Debug, Clone, Default)]
pub struct DetectedEncoders {
    pub adapter_name: Option<String>,
    pub hw_name: Option<String>,
    pub hw_short: Option<String>,
    pub sw_name: Option<String>,
}

/// Enumera los encoders de video H.264 (Hardware GPU y Software CPU) en Media Foundation,
/// priorizando el encoder hardware que coincide con el adaptador DXGI activo (ej. NVIDIA NVENC
/// en PCs con CPU AMD Ryzen con iGPU + GPU NVIDIA dedicada).
pub fn detect_system_encoders() -> DetectedEncoders {
    let mut out = DetectedEncoders::default();
    out.adapter_name = crate::d3d::primary_adapter_name();
    let preferred_vendor = out.adapter_name.as_deref().and_then(vendor_token);

    if unsafe { MFStartup(MF_VERSION, 0) }.is_err() {
        return out;
    }

    if let Ok(hw_candidates) = enumerate_mft_candidates(true, preferred_vendor) {
        if let Some((_, hw)) = hw_candidates.into_iter().next() {
            out.hw_short = Some(short_hw_label(&hw));
            out.hw_name = Some(hw);
        }
    }
    if let Ok(sw_candidates) = enumerate_mft_candidates(false, None) {
        out.sw_name = sw_candidates.into_iter().next().map(|(_, name)| name);
    }

    unsafe {
        let _ = MFShutdown();
    }
    out
}

fn short_hw_label(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.contains("nvidia") || lower.contains("nvenc") {
        "NVENC".to_string()
    } else if lower.contains("amd") || lower.contains("amf") || lower.contains("vce") {
        "AMD AMF".to_string()
    } else if lower.contains("intel") || lower.contains("qsv") {
        "INTEL QSV".to_string()
    } else {
        "GPU HW".to_string()
    }
}

fn vendor_token(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("nvidia")
        || lower.contains("nvenc")
        || lower.contains("geforce")
        || lower.contains("rtx")
        || lower.contains("gtx")
        || lower.contains("quadro")
        || lower.contains("tesla")
    {
        Some("nvidia")
    } else if lower.contains("amd")
        || lower.contains("radeon")
        || lower.contains("amf")
        || lower.contains("vce")
    {
        Some("amd")
    } else if lower.contains("intel")
        || lower.contains("qsv")
        || lower.contains("arc")
        || lower.contains("uhd")
        || lower.contains("iris")
    {
        Some("intel")
    } else {
        None
    }
}

/// Encoder de video H.264 por Hardware vía Windows Media Foundation
/// (NVIDIA NVENC / AMD AMF / Intel QSV), con emparejamiento de adaptador D3D11 para GPUs
/// híbridas, Zero-Copy sobre texturas D3D11 `NV12` y fallback automático a software.
pub struct H264Encoder {
    mft: IMFTransform,
    codec_api: Option<ICodecAPI>,
    event_gen: Option<IMFMediaEventGenerator>,
    _dxgi_manager: Option<IMFDXGIDeviceManager>,
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub is_hardware: bool,
    pub is_async: bool,
    mft_provides_output_samples: bool,
    output_buffer_size: u32,
    reusable_out_sample: Option<IMFSample>,
    pending_input_requests: usize,
    frame_duration_100ns: i64,
    gop_duration_100ns: i64,
    last_forced_kf_ts: Option<i64>,
    mf_started: bool,
}

impl H264Encoder {
    pub fn new(d3d: &SharedD3DDevice, config: VideoConfig) -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, 0) }.context("MFStartup falló")?;

        match Self::init_with_fallbacks(d3d, config) {
            Ok(enc) => Ok(enc),
            Err(err) => {
                unsafe {
                    let _ = MFShutdown();
                }
                Err(err)
            }
        }
    }

    /// Intenta activar el backend H.264 elegido (GPU hardware con fallback seguro a CPU, o solo CPU).
    fn init_with_fallbacks(d3d: &SharedD3DDevice, config: VideoConfig) -> Result<Self> {
        let attempts: &[bool] = match config.encoder_pref {
            EncoderPreference::HardwareGpu => &[true, false],
            EncoderPreference::SoftwareCpu => &[false],
        };

        let mut last_err = None;
        for &hardware in attempts {
            match Self::init_inner(d3d, config, hardware) {
                Ok(enc) => return Ok(enc),
                Err(err) => {
                    warn!(
                        hardware,
                        "No se pudo iniciar encoder H.264 ({err:#}); probando siguiente opción"
                    );
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No se encontró ningún encoder H.264")))
    }

    fn init_inner(d3d: &SharedD3DDevice, config: VideoConfig, hardware: bool) -> Result<Self> {
        let preferred_vendor = vendor_token(&d3d.adapter_name);
        let candidates = enumerate_mft_candidates(hardware, preferred_vendor)?;

        let mut last_err = None;
        for (activate, name) in candidates {
            match Self::try_activate_candidate(d3d, config, hardware, activate, name) {
                Ok(enc) => return Ok(enc),
                Err(err) => {
                    debug!("Candidato MFT descartado: {err:#}");
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Ningún MFT candidato pudo inicializarse")))
    }

    fn try_activate_candidate(
        d3d: &SharedD3DDevice,
        config: VideoConfig,
        is_hardware: bool,
        activate: IMFActivate,
        name: String,
    ) -> Result<Self> {
        let mft: IMFTransform = unsafe { activate.ActivateObject() }
            .with_context(|| format!("ActivateObject falló para {name}"))?;

        let attrs = unsafe { mft.GetAttributes() }.context("GetAttributes en MFT falló")?;
        let is_async = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if is_async {
            unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .context("No se pudo desbloquear el MFT asíncrono")?;
        }

        // Vincular ID3D11Device compartido vía IMFDXGIDeviceManager para Zero-Copy en VRAM.
        let d3d11_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0;
        let mut dxgi_manager = None;
        if is_hardware || d3d11_aware {
            let mut reset_token = 0u32;
            let mut mgr: Option<IMFDXGIDeviceManager> = None;
            unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut mgr) }
                .context("MFCreateDXGIDeviceManager falló")?;
            let mgr = mgr.context("MFCreateDXGIDeviceManager devolvió None")?;
            unsafe { mgr.ResetDevice(&d3d.device, reset_token) }
                .context("IMFDXGIDeviceManager::ResetDevice falló")?;

            let mgr_unk: IUnknown = mgr.cast()?;
            unsafe {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr_unk.as_raw() as usize)
            }
            .with_context(|| format!("MFT_MESSAGE_SET_D3D_MANAGER falló en {name}"))?;
            dxgi_manager = Some(mgr);
        }

        // Configurar tipo de salida (H.264) y entrada (NV12).
        let (width, height) = config.resolution.dimensions();
        let fps = config.fps.as_u32();

        let output_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType (out) falló")?;
        unsafe {
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)?;
            output_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))?;
            output_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1))?;
            output_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
            output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            mft.SetOutputType(0, &output_type, 0)
        }
        .with_context(|| format!("SetOutputType (H.264) falló en {name}"))?;

        let input_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType (in) falló")?;
        unsafe {
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))?;
            input_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1))?;
            input_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
            mft.SetInputType(0, &input_type, 0)
        }
        .with_context(|| format!("SetInputType (NV12) falló en {name}"))?;

        // Configurar parámetros óptimos de calidad/velocidad según el vendor del encoder.
        configure_codec_api(&mft, config, &name);

        let out_info = unsafe { mft.GetOutputStreamInfo(0) }
            .context("GetOutputStreamInfo falló")?;
        let mft_provides_output_samples = (out_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
            != 0;
        let output_buffer_size = out_info.cbSize.max(512 * 1024);

        let event_gen = if is_async {
            Some(
                mft.cast::<IMFMediaEventGenerator>()
                    .context("El MFT asíncrono no implementa IMFMediaEventGenerator")?,
            )
        } else {
            None
        };

        unsafe {
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        info!(
            encoder = %name,
            adapter = %d3d.adapter_name,
            hardware = is_hardware,
            async_mft = is_async,
            zero_copy = dxgi_manager.is_some(),
            resolution = %config.resolution,
            fps = %config.fps,
            bitrate_mbps = format!("{:.1}", config.bitrate_bps as f64 / 1_000_000.0),
            gop_frames = config.gop_frames(),
            "Encoder H.264 Media Foundation iniciado"
        );

        let codec_api = mft.cast::<ICodecAPI>().ok();

        Ok(Self {
            mft,
            codec_api,
            event_gen,
            _dxgi_manager: dxgi_manager,
            name,
            is_hardware,
            is_async,
            mft_provides_output_samples,
            output_buffer_size,
            reusable_out_sample: None,
            pending_input_requests: 0,
            frame_duration_100ns: config.fps.frame_duration_100ns(),
            gop_duration_100ns: config.gop_duration_100ns(),
            last_forced_kf_ts: None,
            mf_started: true,
        })
    }

    /// Etiqueta corta del encoder H.264 activo para mostrar en el HUD de la consola retro.
    pub fn hardware_label(&self) -> String {
        if !self.is_hardware {
            return "CPU SW · H.264".to_string();
        }
        format!("{} · H.264", short_hw_label(&self.name))
    }

    /// Solicita explícitamente un Keyframe (IDR) al encoder para el próximo frame.
    pub fn force_keyframe(&self) {
        if let Some(api) = &self.codec_api {
            let var = VARIANT::from(1u32);
            if let Err(err) = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var) } {
                debug!("CODECAPI_AVEncVideoForceKeyFrame no soportado: {err}");
            }
        }
    }

    /// Envía una textura `ID3D11Texture2D` (`NV12`) en VRAM al encoder (Zero-Copy).
    pub fn encode_nv12_texture(
        &mut self,
        nv12_texture: &ID3D11Texture2D,
        rel_pts_100ns: i64,
        qpc_origin_100ns: i64,
    ) -> Result<Vec<EncodedVideoPacket>> {
        match self.last_forced_kf_ts {
            None => {
                self.force_keyframe();
                self.last_forced_kf_ts = Some(rel_pts_100ns);
            }
            Some(prev) if rel_pts_100ns.saturating_sub(prev) >= self.gop_duration_100ns => {
                self.force_keyframe();
                self.last_forced_kf_ts = Some(rel_pts_100ns);
            }
            _ => {}
        }

        let sample = create_sample_from_texture(nv12_texture, rel_pts_100ns, self.frame_duration_100ns)?;
        let mut packets = Vec::new();

        if self.is_async {
            while self.pending_input_requests == 0 {
                self.poll_async_event(false, &mut packets)?;
            }
            self.pending_input_requests -= 1;

            unsafe { self.mft.ProcessInput(0, &sample, 0) }
                .context("ProcessInput (async MFT) falló")?;

            while self.poll_async_event(true, &mut packets)? {}
        } else {
            unsafe { self.mft.ProcessInput(0, &sample, 0) }
                .context("ProcessInput (sync MFT) falló")?;

            loop {
                match self.pull_output_packet()? {
                    Some(pkt) => packets.push(pkt),
                    None => break,
                }
            }
        }

        for pkt in &mut packets {
            pkt.pts_100ns = qpc_origin_100ns.saturating_add(pkt.pts_100ns);
            pkt.duration_100ns = self.frame_duration_100ns;
        }

        Ok(packets)
    }

    /// Drena todos los frames pendientes dentro del encoder al detener una sesión.
    pub fn flush(&mut self, qpc_origin_100ns: i64) -> Result<Vec<EncodedVideoPacket>> {
        let mut packets = Vec::new();
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }

        if let Some(event_gen) = self.event_gen.clone() {
            loop {
                let event = match unsafe {
                    event_gen.GetEvent(
                        windows::Win32::Media::MediaFoundation::MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0),
                    )
                } {
                    Ok(ev) => ev,
                    Err(_) => break,
                };
                let ev_type = unsafe { event.GetType() }.unwrap_or(0);
                if ev_type == METransformHaveOutput.0 as u32 {
                    if let Some(pkt) = self.pull_output_packet()? {
                        packets.push(pkt);
                    }
                } else if ev_type == METransformDrainComplete.0 as u32 {
                    break;
                }
            }
        } else {
            while let Some(pkt) = self.pull_output_packet()? {
                packets.push(pkt);
            }
        }

        for pkt in &mut packets {
            pkt.pts_100ns = qpc_origin_100ns.saturating_add(pkt.pts_100ns);
            pkt.duration_100ns = self.frame_duration_100ns;
        }

        Ok(packets)
    }

    fn poll_async_event(
        &mut self,
        non_blocking: bool,
        packets: &mut Vec<EncodedVideoPacket>,
    ) -> Result<bool> {
        let Some(event_gen) = &self.event_gen else {
            return Ok(false);
        };

        let flags = if non_blocking {
            MF_EVENT_FLAG_NO_WAIT
        } else {
            windows::Win32::Media::MediaFoundation::MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)
        };

        let event = match unsafe { event_gen.GetEvent(flags) } {
            Ok(ev) => ev,
            Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(err) => return Err(err).context("IMFMediaEventGenerator::GetEvent falló"),
        };

        let ev_type = unsafe { event.GetType() }.context("IMFMediaEvent::GetType falló")?;
        if ev_type == METransformNeedInput.0 as u32 {
            self.pending_input_requests += 1;
        } else if ev_type == METransformHaveOutput.0 as u32 {
            if let Some(pkt) = self.pull_output_packet()? {
                packets.push(pkt);
            }
        } else if ev_type == METransformDrainComplete.0 as u32 {
            debug!("METransformDrainComplete recibido");
        }

        Ok(true)
    }

    fn pull_output_packet(&mut self) -> Result<Option<EncodedVideoPacket>> {
        let provided_sample = if self.mft_provides_output_samples {
            None
        } else if let Some(cached) = self.reusable_out_sample.take() {
            if let Ok(buf) = unsafe { cached.GetBufferByIndex(0) } {
                let _ = unsafe { buf.SetCurrentLength(0) };
            }
            Some(cached)
        } else {
            let sample = unsafe { MFCreateSample() }.context("MFCreateSample (out) falló")?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_buffer_size) }
                .context("MFCreateMemoryBuffer falló")?;
            unsafe { sample.AddBuffer(&buffer) }?;
            Some(sample)
        };

        let mut output_buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;

        let hr = unsafe { self.mft.ProcessOutput(0, &mut output_buffers, &mut status) };

        let out_sample = unsafe { ManuallyDrop::take(&mut output_buffers[0].pSample) };
        let _out_events = unsafe { ManuallyDrop::take(&mut output_buffers[0].pEvents) };

        match hr {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                if !self.mft_provides_output_samples {
                    self.reusable_out_sample = out_sample;
                }
                return Ok(None);
            }
            Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                if !self.mft_provides_output_samples {
                    self.reusable_out_sample = out_sample;
                }
                debug!("MFT reportó MF_E_TRANSFORM_STREAM_CHANGE; refrescando tipo de salida");
                if let Ok(new_type) = unsafe { self.mft.GetOutputAvailableType(0, 0) } {
                    let _ = unsafe { self.mft.SetOutputType(0, &new_type, 0) };
                }
                return Ok(None);
            }
            Err(err) => return Err(err).context("IMFTransform::ProcessOutput falló"),
        }

        let Some(sample) = out_sample else {
            return Ok(None);
        };

        let pts_100ns = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let duration_100ns = unsafe { sample.GetSampleDuration() }.unwrap_or(self.frame_duration_100ns);
        let clean_point = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;

        let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }
            .context("IMFSample::ConvertToContiguousBuffer falló")?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut current_len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut current_len)) }
            .context("IMFMediaBuffer::Lock falló")?;

        let data: Arc<[u8]> = if !ptr.is_null() && current_len > 0 {
            Arc::from(unsafe { std::slice::from_raw_parts(ptr, current_len as usize) })
        } else {
            Arc::from([])
        };

        unsafe {
            let _ = buffer.Unlock();
        }
        drop(buffer);

        if !self.mft_provides_output_samples {
            self.reusable_out_sample = Some(sample);
        }

        if data.is_empty() {
            return Ok(None);
        }

        let is_keyframe = clean_point || annexb_contains_idr(&data);

        Ok(Some(EncodedVideoPacket {
            data,
            pts_100ns,
            duration_100ns,
            is_keyframe,
        }))
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        if self.mf_started {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
}

/// Enumera candidatos MFT priorizando el fabricante del adaptador D3D11 activo
/// (crítico en laptops/PCs híbridas Intel + NVIDIA para enlazar NVENC con la GPU NVIDIA).
fn enumerate_mft_candidates(
    hardware: bool,
    preferred_vendor: Option<&'static str>,
) -> Result<Vec<(IMFActivate, String)>> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };

    let mut raw_activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;

    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output_info),
            &mut raw_activates,
            &mut count,
        )
    }
    .context("MFTEnumEx falló")?;

    if count == 0 || raw_activates.is_null() {
        bail!("MFTEnumEx devolvió 0 encoders H.264 (hardware={hardware})");
    }

    let mut list: Vec<(IMFActivate, String)> = Vec::with_capacity(count as usize);
    let slice = unsafe { std::slice::from_raw_parts_mut(raw_activates, count as usize) };
    for slot in slice.iter_mut() {
        if let Some(activate) = slot.take() {
            let friendly = get_friendly_name(&activate).unwrap_or_else(|| "H.264 MFT".to_string());
            list.push((activate, friendly));
        }
    }

    unsafe {
        CoTaskMemFree(Some(raw_activates as *const _));
    }

    // Priorizar el MFT que coincide con el fabricante del adaptador D3D11 (NVIDIA / AMD / Intel).
    if hardware {
        if let Some(want) = preferred_vendor {
            list.sort_by_key(|(_, name)| usize::from(vendor_token(name) != Some(want)));
        }
        let names: Vec<&str> = list.iter().map(|(_, n)| n.as_str()).collect();
        debug!(
            preferred_vendor = ?preferred_vendor,
            candidates = ?names,
            "Encoders H.264 Hardware enumerados"
        );
    }

    Ok(list)
}

fn get_friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut len = 0u32;
    unsafe { activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) }
        .ok()
        .map(|l| len = l)?;
    let mut buf = vec![0u16; len as usize + 1];
    unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None) }.ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Configura parámetros óptimos de codificación H.264 según el fabricante del ASIC:
/// - **AMD AMF (VCE/VCN):** usa el fast-path en tiempo real (`AVLowLatencyMode = true`,
///   `QualityVsSpeed = 30` preset *Speed*, y `PeakConstrainedVBR` con techo 1.35×) para sostener
///   60 FPS bloqueados en 1080p sin saturar VCE/VCN ni generar tirones en GPUs Polaris/Vega/Win10.
/// - **NVIDIA NVENC / Intel QSV / CPU SW:** mantiene `AVLowLatencyMode = false`, `QualityVsSpeed = 70`
///   y **QVBR** (`Quality` + `CommonQuality` + techo `MaxBitRate`) para máxima nitidez por MB.
fn configure_codec_api(mft: &IMFTransform, config: VideoConfig, encoder_name: &str) {
    let Ok(codec_api) = mft.cast::<ICodecAPI>() else {
        debug!("El MFT no expone ICodecAPI; se usarán sus valores por defecto");
        return;
    };

    let set_u32 = |guid: &GUID, val: u32, label: &str| {
        let var = VARIANT::from(val);
        if let Err(err) = unsafe { codec_api.SetValue(guid, &var) } {
            debug!("ICodecAPI {label} no soportado por este driver: {err}");
        }
    };

    let set_bool = |guid: &GUID, val: bool, label: &str| {
        let var = VARIANT::from(val);
        if let Err(err) = unsafe { codec_api.SetValue(guid, &var) } {
            debug!("ICodecAPI {label} no soportado por este driver: {err}");
        }
    };

    let is_amd = vendor_token(encoder_name) == Some("amd");
    let max_bitrate = config.bitrate_bps.saturating_mul(135) / 100;

    set_u32(&CODECAPI_AVEncMPVDefaultBPictureCount, 0, "BPictureCount=0");
    set_u32(&CODECAPI_AVEncMPVGOPSize, config.gop_frames(), "GOPSize");

    if is_amd {
        // Fast-path en tiempo real para AMD AMF (evita buffering interno y picos >16.6 ms/frame).
        set_bool(&CODECAPI_AVLowLatencyMode, true, "LowLatencyMode=true (AMD)");
        set_u32(&CODECAPI_AVEncCommonQualityVsSpeed, 30, "QualityVsSpeed=30 (AMD Speed)");

        let vbr_mode = VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32);
        if unsafe { codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &vbr_mode) }.is_err() {
            set_u32(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_CBR.0 as u32,
                "RateControlMode=CBR (AMD)",
            );
        }
        set_u32(&CODECAPI_AVEncCommonMeanBitRate, config.bitrate_bps, "MeanBitRate (AMD)");
        set_u32(&CODECAPI_AVEncCommonMaxBitRate, max_bitrate, "MaxBitRate (AMD)");
        debug!(
            mean_mbps = format!("{:.1}", config.bitrate_bps as f64 / 1_000_000.0),
            max_mbps = format!("{:.1}", max_bitrate as f64 / 1_000_000.0),
            "Perfil AMD AMF fast-path activado (LowLatency + Speed + PeakConstrainedVBR)"
        );
        return;
    }

    // 1. Modo grabación local para NVENC / QSV / SW: máxima nitidez por MB.
    set_bool(&CODECAPI_AVLowLatencyMode, false, "LowLatencyMode=false");
    set_u32(&CODECAPI_AVEncCommonQualityVsSpeed, 70, "QualityVsSpeed=70");

    // 2. Rate-control QVBR (Calidad constante con techo MaxBitRate) -> clips livianos en escenas
    //    quietas y nítidos en acción rápida sin disparar el consumo de RAM del Replay Buffer.
    let quality_var = VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32);
    let q_val = VARIANT::from(config.qvbr_quality_level());

    let qvbr_ok = unsafe { codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &quality_var) }.is_ok()
        && unsafe { codec_api.SetValue(&CODECAPI_AVEncCommonQuality, &q_val) }.is_ok();

    if qvbr_ok {
        set_u32(&CODECAPI_AVEncCommonMeanBitRate, config.bitrate_bps, "MeanBitRate");
        set_u32(&CODECAPI_AVEncCommonMaxBitRate, max_bitrate, "MaxBitRate(QVBR)");
        debug!(
            qvbr_quality = config.qvbr_quality_level(),
            max_mbps = format!("{:.1}", max_bitrate as f64 / 1_000_000.0),
            "Rate-control QVBR activado"
        );
    } else {
        let vbr_mode = VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32);
        if unsafe { codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &vbr_mode) }.is_err() {
            set_u32(
                &CODECAPI_AVEncCommonRateControlMode,
                eAVEncCommonRateControlMode_CBR.0 as u32,
                "RateControlMode=CBR",
            );
        }
        set_u32(&CODECAPI_AVEncCommonMeanBitRate, config.bitrate_bps, "MeanBitRate");
        set_u32(&CODECAPI_AVEncCommonMaxBitRate, max_bitrate, "MaxBitRate");
    }
}

fn create_sample_from_texture(
    texture: &ID3D11Texture2D,
    timestamp_100ns: i64,
    duration_100ns: i64,
) -> Result<IMFSample> {
    let sample = unsafe { MFCreateSample() }.context("MFCreateSample falló")?;
    let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false) }
        .context("MFCreateDXGISurfaceBuffer falló")?;

    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(timestamp_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;
    }

    Ok(sample)
}

#[inline]
const fn pack_u32_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}
