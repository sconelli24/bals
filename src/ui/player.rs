use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use eframe::egui::{self, Color32};
use tracing::{debug, warn};
use wasapi::{
    AudioClient, AudioRenderClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8X8_UNORM, DXGI_FORMAT_NV12,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample,
    IMFSourceReader, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX, MF_PD_DURATION,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_ANY_STREAM,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFAudioFormat_PCM,
    MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video, MFNominalRange_0_255,
    MFShutdown, MFStartup, MFVideoFormat_NV12, MFVideoFormat_RGB32, MFVideoTransferMatrix_BT601,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{GUID, HSTRING, IUnknown, Interface};

use crate::trimmer::ClipMetadata;

const PLAYER_MAX_W: u32 = 960;
const PLAYER_MAX_H: u32 = 540;
const AUDIO_SAMPLE_RATE: usize = 48_000;
const AUDIO_CHANNELS: usize = 2;
const AUDIO_BYTES_PER_FRAME: usize = AUDIO_CHANNELS * 2; // 16-bit stereo = 4 bytes
const PREROLL_TOL_100NS: i64 = 150_000; // 15 ms
const MAX_POOLED_BUFFERS: usize = 4;

/// Pool ping-pong de buffers de video reciclados entre el decodificador y la UI/GPU
/// para sostener reproducción continua con 0 alocaciones por frame.
#[derive(Clone, Default)]
pub struct FrameBufferPool {
    nv12: Arc<Mutex<Vec<Vec<u8>>>>,
    rgba: Arc<Mutex<Vec<Vec<Color32>>>>,
}

impl FrameBufferPool {
    pub fn take_nv12(&self, needed: usize) -> Vec<u8> {
        let mut buf = self
            .nv12
            .lock()
            .ok()
            .and_then(|mut q| q.pop())
            .unwrap_or_default();
        buf.resize(needed, 0);
        buf
    }

    pub fn recycle_nv12(&self, mut buf: Vec<u8>) {
        if buf.capacity() == 0 {
            return;
        }
        buf.clear();
        if let Ok(mut q) = self.nv12.lock() {
            if q.len() < MAX_POOLED_BUFFERS {
                q.push(buf);
            }
        }
    }

    pub fn take_rgba(&self, pixels: usize) -> Vec<Color32> {
        let mut buf = self
            .rgba
            .lock()
            .ok()
            .and_then(|mut q| q.pop())
            .unwrap_or_default();
        buf.resize(pixels, Color32::BLACK);
        buf
    }

    pub fn recycle_rgba(&self, mut buf: Vec<Color32>) {
        if buf.capacity() == 0 {
            return;
        }
        buf.clear();
        if let Ok(mut q) = self.rgba.lock() {
            if q.len() < MAX_POOLED_BUFFERS {
                q.push(buf);
            }
        }
    }

    pub fn recycle_pixels(&self, pixels: FramePixels) {
        match pixels {
            FramePixels::Nv12 { data, .. } => self.recycle_nv12(data),
            FramePixels::Rgba(data) => self.recycle_rgba(data),
        }
    }
}

/// Píxeles decodificados del frame actual (NV12 compacto para shader GL o fallback RGBA).
pub enum FramePixels {
    Nv12 {
        data: Vec<u8>,
        is_bt709: bool,
        full_range: bool,
    },
    Rgba(Vec<Color32>),
}

/// Frame decodificado listo para renderizar en el Monitor CRT de la UI.
pub struct PlayerFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: FramePixels,
    pub time_secs: f64,
    pub clip_path: PathBuf,
    pub seek_gen: u64,
}

enum PlayerCmd {
    LoadClip {
        path: PathBuf,
        start_secs: f64,
        seq: u64,
    },
    #[allow(dead_code)]
    Play,
    Pause,
    TogglePlay,
    Seek {
        time_secs: f64,
        seq: u64,
    },
    Step {
        delta_secs: f64,
        seq: u64,
    },
    SetLoop {
        enabled: bool,
        start_secs: f64,
        end_secs: f64,
    },
    ReleaseClip {
        ack: Option<Sender<()>>,
    },
    Shutdown,
}

/// Reproductor nativo liviano (Media Foundation D3D11/DXVA + NV12/GL + WASAPI, 0 dependencias externas)
/// para previsualizar clips y marcar `[IN]` / `[OUT]` con precisión dentro del Monitor CRT.
pub struct ClipPlayer {
    cmd_tx: Sender<PlayerCmd>,
    latest_frame: Arc<Mutex<Option<PlayerFrame>>>,
    latest_metadata: Arc<Mutex<Option<(PathBuf, ClipMetadata)>>>,
    buffer_pool: FrameBufferPool,
    playing: Arc<AtomicBool>,
    loading: Arc<AtomicBool>,
    has_audio: Arc<AtomicBool>,
    hw_accel: Arc<AtomicBool>,
    nv12_active: Arc<AtomicBool>,
    position_bits: Arc<AtomicU64>,
    seek_gen: Arc<AtomicU64>,
    volume_bits: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ClipPlayer {
    pub fn start(
        egui_ctx: egui::Context,
        initial_volume: f32,
        initial_muted: bool,
        prefer_nv12: bool,
    ) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<PlayerCmd>();
        let latest_frame = Arc::new(Mutex::new(None));
        let latest_metadata = Arc::new(Mutex::new(None));
        let buffer_pool = FrameBufferPool::default();
        let playing = Arc::new(AtomicBool::new(false));
        let loading = Arc::new(AtomicBool::new(false));
        let has_audio = Arc::new(AtomicBool::new(false));
        let hw_accel = Arc::new(AtomicBool::new(false));
        let nv12_active = Arc::new(AtomicBool::new(false));
        let position_bits = Arc::new(AtomicU64::new(0.0f64.to_bits()));
        let seek_gen = Arc::new(AtomicU64::new(0));
        let volume_bits = Arc::new(AtomicU32::new(initial_volume.clamp(0.0, 1.0).to_bits()));
        let muted = Arc::new(AtomicBool::new(initial_muted));

        let frame_bg = Arc::clone(&latest_frame);
        let meta_bg = Arc::clone(&latest_metadata);
        let pool_bg = buffer_pool.clone();
        let playing_bg = Arc::clone(&playing);
        let loading_bg = Arc::clone(&loading);
        let audio_bg = Arc::clone(&has_audio);
        let hw_bg = Arc::clone(&hw_accel);
        let nv12_bg = Arc::clone(&nv12_active);
        let pos_bg = Arc::clone(&position_bits);
        let vol_bg = Arc::clone(&volume_bits);
        let mute_bg = Arc::clone(&muted);

        let handle = thread::Builder::new()
            .name("bals-clip-player".to_string())
            .spawn(move || {
                let ro_ok = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();
                let mf_ok = unsafe { MFStartup(MF_VERSION, 0) }.is_ok();
                let _ = wasapi::initialize_mta();

                player_thread_loop(
                    cmd_rx,
                    egui_ctx,
                    prefer_nv12,
                    frame_bg,
                    meta_bg,
                    pool_bg,
                    playing_bg,
                    loading_bg,
                    audio_bg,
                    hw_bg,
                    nv12_bg,
                    pos_bg,
                    vol_bg,
                    mute_bg,
                );

                if mf_ok {
                    unsafe {
                        let _ = MFShutdown();
                    }
                }
                if ro_ok {
                    unsafe {
                        RoUninitialize();
                    }
                }
            })
            .ok();

        Self {
            cmd_tx,
            latest_frame,
            latest_metadata,
            buffer_pool,
            playing,
            loading,
            has_audio,
            hw_accel,
            nv12_active,
            position_bits,
            seek_gen,
            volume_bits,
            muted,
            handle,
        }
    }

    pub fn buffer_pool(&self) -> &FrameBufferPool {
        &self.buffer_pool
    }

    pub fn load_clip(&self, path: PathBuf, start_secs: f64) {
        let seq = self.seek_gen.fetch_add(1, Ordering::AcqRel) + 1;
        self.playing.store(false, Ordering::Release);
        self.loading.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(PlayerCmd::LoadClip {
            path,
            start_secs,
            seq,
        });
    }

    #[allow(dead_code)]
    pub fn play(&self) {
        let _ = self.cmd_tx.send(PlayerCmd::Play);
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(PlayerCmd::Pause);
    }

    pub fn toggle_play(&self) {
        let _ = self.cmd_tx.send(PlayerCmd::TogglePlay);
    }

    pub fn seek(&self, time_secs: f64) {
        let seq = self.seek_gen.fetch_add(1, Ordering::AcqRel) + 1;
        self.loading.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(PlayerCmd::Seek { time_secs, seq });
    }

    pub fn step_secs(&self, delta_secs: f64) {
        let seq = self.seek_gen.fetch_add(1, Ordering::AcqRel) + 1;
        self.loading.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(PlayerCmd::Step { delta_secs, seq });
    }

    pub fn set_loop_range(&self, enabled: bool, start_secs: f64, end_secs: f64) {
        let _ = self.cmd_tx.send(PlayerCmd::SetLoop {
            enabled,
            start_secs,
            end_secs,
        });
    }

    pub fn set_volume(&self, vol: f32) {
        self.volume_bits
            .store(vol.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Libera el lector MF, superficie D3D11 y sesión WASAPI en segundo plano sin bloquear la UI.
    pub fn release_clip(&self) {
        self.seek_gen.fetch_add(1, Ordering::AcqRel);
        self.playing.store(false, Ordering::Release);
        self.loading.store(false, Ordering::Release);
        let _ = self.cmd_tx.send(PlayerCmd::ReleaseClip { ack: None });
    }

    /// Libera sincrónicamente el clip abierto (útil antes de borrar el `.mp4` en Windows).
    pub fn release_clip_sync(&self) {
        self.seek_gen.fetch_add(1, Ordering::AcqRel);
        self.playing.store(false, Ordering::Release);
        self.loading.store(false, Ordering::Release);
        let (ack_tx, ack_rx) = bounded::<()>(1);
        if self
            .cmd_tx
            .send(PlayerCmd::ReleaseClip { ack: Some(ack_tx) })
            .is_ok()
        {
            let _ = ack_rx.recv_timeout(Duration::from_millis(500));
        }
    }

    pub fn take_frame(&self) -> Option<PlayerFrame> {
        let frame = self.latest_frame.lock().ok()?.take()?;
        if frame.seek_gen < self.seek_gen.load(Ordering::Acquire) {
            self.buffer_pool.recycle_pixels(frame.pixels);
            return None;
        }
        Some(frame)
    }

    pub fn take_metadata(&self) -> Option<(PathBuf, ClipMetadata)> {
        self.latest_metadata.lock().ok()?.take()
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    pub fn is_loading(&self) -> bool {
        self.loading.load(Ordering::Acquire)
    }

    pub fn has_audio(&self) -> bool {
        self.has_audio.load(Ordering::Acquire)
    }

    pub fn is_hw_accelerated(&self) -> bool {
        self.hw_accel.load(Ordering::Acquire)
    }

    pub fn is_nv12(&self) -> bool {
        self.nv12_active.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub fn position_secs(&self) -> f64 {
        f64::from_bits(self.position_bits.load(Ordering::Acquire))
    }
}

impl Drop for ClipPlayer {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(PlayerCmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Ok(mut slot) = self.latest_frame.lock() {
            if let Some(f) = slot.take() {
                self.buffer_pool.recycle_pixels(f.pixels);
            }
        }
    }
}

struct StagingSurface {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

struct PlayerD3dContext {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dxgi_manager: IMFDXGIDeviceManager,
    staging: Option<StagingSurface>,
}

impl PlayerD3dContext {
    fn new() -> Result<Self> {
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut achieved = D3D_FEATURE_LEVEL::default();

        let flags = D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        if unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                flags,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut achieved),
                Some(&mut context),
            )
        }
        .is_err()
        {
            unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    windows::Win32::Foundation::HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut achieved),
                    Some(&mut context),
                )
            }?;
        }

        let device = device.context("D3D11CreateDevice (player) devolvió device = None")?;
        let context = context.context("D3D11CreateDevice (player) devolvió context = None")?;

        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }

        let mut reset_token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut mgr) }
            .context("MFCreateDXGIDeviceManager (player) falló")?;
        let dxgi_manager = mgr.context("MFCreateDXGIDeviceManager (player) devolvió None")?;
        unsafe { dxgi_manager.ResetDevice(&device, reset_token) }
            .context("IMFDXGIDeviceManager::ResetDevice (player) falló")?;

        Ok(Self {
            device,
            context,
            dxgi_manager,
            staging: None,
        })
    }

    fn ensure_staging(
        &mut self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<&ID3D11Texture2D> {
        let reuse = self
            .staging
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height && s.format == format);
        if !reuse {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .context("CreateTexture2D staging (player) falló")?;
            self.staging = Some(StagingSurface {
                texture: tex.context("CreateTexture2D staging devolvió None")?,
                width,
                height,
                format,
            });
        }
        Ok(&self.staging.as_ref().expect("staging initialized").texture)
    }

    fn read_dxgi_frame(
        &mut self,
        dxgi_buf: &IMFDXGIBuffer,
        actual_w: u32,
        actual_h: u32,
        is_bt709: bool,
        full_range: bool,
        pool: &FrameBufferPool,
    ) -> Option<FramePixels> {
        let mut tex_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { dxgi_buf.GetResource(&ID3D11Texture2D::IID, &mut tex_ptr) }.ok()?;
        if tex_ptr.is_null() {
            return None;
        }
        let src_tex = unsafe { ID3D11Texture2D::from_raw(tex_ptr) };
        let subresource = unsafe { dxgi_buf.GetSubresourceIndex() }.ok()?;

        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src_tex.GetDesc(&mut src_desc) };

        if src_desc.Format != DXGI_FORMAT_NV12
            && src_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            && src_desc.Format != DXGI_FORMAT_B8G8R8X8_UNORM
        {
            return None;
        }

        let staging = self
            .ensure_staging(src_desc.Width, src_desc.Height, src_desc.Format)
            .ok()?
            .clone();

        unsafe {
            self.context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                &src_tex,
                subresource,
                None,
            );
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .ok()?;

        let w = (actual_w as usize).min(src_desc.Width as usize) & !1;
        let h = (actual_h as usize).min(src_desc.Height as usize) & !1;
        let pitch = mapped.RowPitch as usize;

        let out = if mapped.pData.is_null() || w < 2 || h < 2 {
            None
        } else if src_desc.Format == DXGI_FORMAT_NV12 {
            let tex_h = src_desc.Height as usize;
            let total_bytes = pitch * (tex_h + tex_h / 2);
            let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, total_bytes) };
            copy_nv12_planes(src, pitch, tex_h, w, h, pool).map(|data| FramePixels::Nv12 {
                data,
                is_bt709,
                full_range,
            })
        } else {
            let total_bytes = pitch * h;
            let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, total_bytes) };
            Some(FramePixels::Rgba(copy_bgra_to_color32(src, pitch, false, w, h, pool)))
        };

        unsafe {
            self.context.Unmap(&staging, 0);
        }
        out
    }
}

struct WasapiAudioOut {
    client: AudioClient,
    render: AudioRenderClient,
    pcm_bytes: VecDeque<u8>,
    running: bool,
}

impl WasapiAudioOut {
    fn new() -> Result<Self> {
        let enumerator =
            DeviceEnumerator::new().map_err(|e| anyhow::anyhow!("DeviceEnumerator: {e}"))?;
        let device = enumerator
            .get_default_device(Direction::Render)
            .map_err(|e| anyhow::anyhow!("Sin dispositivo de salida: {e}"))?;
        let mut client = device
            .get_iaudioclient()
            .map_err(|e| anyhow::anyhow!("get_iaudioclient: {e}"))?;

        let format = WaveFormat::new(
            16,
            16,
            SampleType::Int,
            AUDIO_SAMPLE_RATE,
            AUDIO_CHANNELS,
            None,
        );
        let mode = StreamMode::PollingShared {
            autoconvert: true,
            buffer_duration_hns: 1_000_000, // 100 ms
        };
        client
            .initialize_client(&format, Direction::Render, mode, None)
            .map_err(|e| anyhow::anyhow!("initialize_client (player render): {e}"))?;

        let render = client
            .get_audiorenderclient()
            .map_err(|e| anyhow::anyhow!("get_audiorenderclient: {e}"))?;

        Ok(Self {
            client,
            render,
            pcm_bytes: VecDeque::with_capacity(AUDIO_SAMPLE_RATE * AUDIO_BYTES_PER_FRAME),
            running: false,
        })
    }

    fn start(&mut self) {
        if !self.running && self.client.start_stream().is_ok() {
            self.running = true;
        }
    }

    fn stop_and_clear(&mut self) {
        if self.running {
            let _ = self.client.stop_stream();
            self.running = false;
        }
        let _ = self.client.reset_stream();
        self.pcm_bytes.clear();
    }

    fn push_pcm_i16(&mut self, bytes: &[u8], volume: f32, muted: bool) {
        if muted || volume <= 0.001 {
            self.pcm_bytes.extend(std::iter::repeat_n(0u8, bytes.len()));
            return;
        }
        if (volume - 1.0).abs() < 0.01 {
            self.pcm_bytes.extend(bytes);
            return;
        }
        for pair in bytes.chunks_exact(2) {
            let s = i16::from_le_bytes([pair[0], pair[1]]) as f32 * volume;
            let clamped = s.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            self.pcm_bytes.extend(clamped.to_le_bytes());
        }
    }

    fn pump(&mut self) {
        if !self.running || self.pcm_bytes.is_empty() {
            return;
        }
        let avail_frames = self.client.get_available_space_in_frames().unwrap_or(0) as usize;
        let queued_frames = self.pcm_bytes.len() / AUDIO_BYTES_PER_FRAME;
        let to_write = avail_frames.min(queued_frames);
        if to_write > 0 {
            let _ = self
                .render
                .write_to_device_from_deque(to_write, &mut self.pcm_bytes, None);
        }
    }
}

impl Drop for WasapiAudioOut {
    fn drop(&mut self) {
        self.stop_and_clear();
    }
}

struct OpenPlayerClip {
    path: PathBuf,
    reader: IMFSourceReader,
    video_idx: u32,
    audio_idx: Option<u32>,
    actual_w: u32,
    actual_h: u32,
    default_stride: i32,
    hw_accel: bool,
    output_is_nv12: bool,
    is_bt709: bool,
    full_range: bool,
    metadata: ClipMetadata,
}

impl OpenPlayerClip {
    fn open(path: &Path, d3d_ctx: Option<&PlayerD3dContext>, prefer_nv12: bool) -> Result<Self> {
        let mut attempts = Vec::with_capacity(4);
        if let Some(d3d) = d3d_ctx {
            if prefer_nv12 {
                attempts.push((Some(d3d), true));
            }
            attempts.push((Some(d3d), false));
        }
        if prefer_nv12 {
            attempts.push((None, true));
        }
        attempts.push((None, false));

        let mut last_err = None;
        for (d3d_opt, want_nv12) in attempts {
            match Self::open_with_mode(path, d3d_opt, want_nv12) {
                Ok(clip) => {
                    debug!(
                        path = %path.display(),
                        hw_accel = clip.hw_accel,
                        nv12 = clip.output_is_nv12,
                        out_w = clip.actual_w,
                        out_h = clip.actual_h,
                        "Reproductor abierto"
                    );
                    return Ok(clip);
                }
                Err(err) => {
                    debug!(
                        hw = d3d_opt.is_some(),
                        nv12 = want_nv12,
                        "Intento de apertura fallback: {err:#}"
                    );
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No se pudo abrir {}", path.display())))
    }

    fn open_with_mode(
        path: &Path,
        d3d_ctx: Option<&PlayerD3dContext>,
        want_nv12: bool,
    ) -> Result<Self> {
        let mut attrs: Option<IMFAttributes> = None;
        let attr_cap = if d3d_ctx.is_some() { 3 } else { 2 };
        unsafe { MFCreateAttributes(&mut attrs, attr_cap) }?;
        let attrs = attrs.context("MFCreateAttributes (player) devolvió None")?;
        unsafe {
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            if let Some(d3d) = d3d_ctx {
                let mgr_unk: IUnknown = d3d.dxgi_manager.cast()?;
                attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &mgr_unk)?;
            }
        }

        let url = HSTRING::from(path.as_os_str());
        let reader: IMFSourceReader = unsafe { MFCreateSourceReaderFromURL(&url, Some(&attrs)) }
            .with_context(|| format!("No se pudo abrir {} en reproductor", path.display()))?;

        let duration_100ns = unsafe {
            reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        }
        .ok()
        .and_then(|prop: PROPVARIANT| {
            u64::try_from(&prop)
                .or_else(|_| i64::try_from(&prop).map(|v| v.max(0) as u64))
                .ok()
        })
        .unwrap_or(0);

        let mut video_idx: Option<u32> = None;
        let mut audio_idx: Option<u32> = None;
        let mut src_w = 1280u32;
        let mut src_h = 720u32;
        let mut fps = 0.0f32;
        let mut is_bt709: Option<bool> = None;
        let mut full_range = false;

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
                if let Ok(packed_fps) = unsafe { native_type.GetUINT64(&MF_MT_FRAME_RATE) } {
                    let (num, den) = unpack_u32_pair(packed_fps);
                    if num > 0 && den > 0 {
                        fps = num as f32 / den as f32;
                    }
                }
                if let Ok(matrix) = unsafe { native_type.GetUINT32(&MF_MT_YUV_MATRIX) } {
                    is_bt709 = Some(matrix != MFVideoTransferMatrix_BT601.0 as u32);
                }
                if let Ok(range) = unsafe { native_type.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) } {
                    full_range = range == MFNominalRange_0_255.0 as u32;
                }
                unsafe { reader.SetStreamSelection(idx, true) }?;
            } else if major == Some(MFMediaType_Audio) && audio_idx.is_none() {
                audio_idx = Some(idx);
                let _ = unsafe { reader.SetStreamSelection(idx, true) };
            } else {
                let _ = unsafe { reader.SetStreamSelection(idx, false) };
            }
        }

        let video_idx = video_idx.context("El archivo no tiene pista de video")?;
        let is_bt709 = is_bt709.unwrap_or(src_h >= 720);

        let scale = (PLAYER_MAX_W as f64 / src_w as f64)
            .min(PLAYER_MAX_H as f64 / src_h as f64)
            .min(1.0);
        let out_w = (((src_w as f64 * scale).round() as u32) & !1).max(64);
        let out_h = (((src_h as f64 * scale).round() as u32) & !1).max(36);

        let subtype = if want_nv12 {
            MFVideoFormat_NV12
        } else {
            MFVideoFormat_RGB32
        };

        let out_type = unsafe { MFCreateMediaType() }?;
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(out_w, out_h))?;
            reader.SetCurrentMediaType(video_idx, None, &out_type)?;
        }

        let current_type = unsafe { reader.GetCurrentMediaType(video_idx) }?;
        let (actual_w, actual_h) = unsafe { current_type.GetUINT64(&MF_MT_FRAME_SIZE) }
            .map(unpack_u32_pair)
            .unwrap_or((out_w, out_h));
        let default_stride = unsafe { current_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .map(|u| u as i32)
            .unwrap_or(if want_nv12 {
                actual_w as i32
            } else {
                (actual_w * 4) as i32
            });

        // Negociar salida de audio PCM 48 kHz 16-bit estéreo (best-effort; si falla, modo solo video).
        if let Some(a_idx) = audio_idx {
            let pcm_ok = unsafe { MFCreateMediaType() }.ok().is_some_and(|pcm_type| {
                let block_align = AUDIO_BYTES_PER_FRAME as u32;
                let avg_bytes = AUDIO_SAMPLE_RATE as u32 * block_align;
                unsafe {
                    pcm_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).is_ok()
                        && pcm_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).is_ok()
                        && pcm_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).is_ok()
                        && pcm_type
                            .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE as u32)
                            .is_ok()
                        && pcm_type
                            .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS as u32)
                            .is_ok()
                        && pcm_type
                            .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)
                            .is_ok()
                        && pcm_type
                            .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes)
                            .is_ok()
                        && reader.SetCurrentMediaType(a_idx, None, &pcm_type).is_ok()
                }
            });
            if !pcm_ok {
                let _ = unsafe { reader.SetStreamSelection(a_idx, false) };
                audio_idx = None;
            }
        }

        let metadata = ClipMetadata {
            duration_secs: duration_100ns as f64 / 10_000_000.0,
            width: src_w,
            height: src_h,
            fps,
        };

        Ok(Self {
            path: path.to_path_buf(),
            reader,
            video_idx,
            audio_idx,
            actual_w,
            actual_h,
            default_stride,
            hw_accel: d3d_ctx.is_some(),
            output_is_nv12: want_nv12,
            is_bt709,
            full_range,
            metadata,
        })
    }

    fn refresh_video_layout(&mut self) {
        if let Ok(current_type) = unsafe { self.reader.GetCurrentMediaType(self.video_idx) } {
            if let Ok(packed) = unsafe { current_type.GetUINT64(&MF_MT_FRAME_SIZE) } {
                let (w, h) = unpack_u32_pair(packed);
                if w > 0 && h > 0 {
                    self.actual_w = w;
                    self.actual_h = h;
                }
            }
            if let Ok(stride) = unsafe { current_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
                self.default_stride = stride as i32;
            }
            if let Ok(sub) = unsafe { current_type.GetGUID(&MF_MT_SUBTYPE) } {
                self.output_is_nv12 = sub == MFVideoFormat_NV12;
            }
            if let Ok(matrix) = unsafe { current_type.GetUINT32(&MF_MT_YUV_MATRIX) } {
                self.is_bt709 = matrix != MFVideoTransferMatrix_BT601.0 as u32;
            }
            if let Ok(range) = unsafe { current_type.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) } {
                self.full_range = range == MFNominalRange_0_255.0 as u32;
            }
        }
    }

    fn extract_frame(
        &mut self,
        sample: &IMFSample,
        d3d_ctx: Option<&mut PlayerD3dContext>,
        pool: &FrameBufferPool,
    ) -> Option<FramePixels> {
        if self.hw_accel {
            if let Some(d3d) = d3d_ctx {
                if let Ok(buf) = unsafe { sample.GetBufferByIndex(0) } {
                    if let Ok(dxgi_buf) = buf.cast::<IMFDXGIBuffer>() {
                        if let Some(pixels) = d3d.read_dxgi_frame(
                            &dxgi_buf,
                            self.actual_w,
                            self.actual_h,
                            self.is_bt709,
                            self.full_range,
                            pool,
                        ) {
                            return Some(pixels);
                        }
                    }
                }
            }
        }

        let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }.ok()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut cur_len)) }.ok()?;

        let w = (self.actual_w as usize) & !1;
        let h = (self.actual_h as usize) & !1;

        let pixels = if !ptr.is_null() && w >= 2 && h >= 2 {
            let src = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize) };
            if self.output_is_nv12 {
                let row_stride = self.default_stride.unsigned_abs().max(w as u32) as usize;
                let aligned_h = (h + 15) & !15;
                let total_rows = cur_len as usize / row_stride.max(1);
                let plane_h = if total_rows >= aligned_h * 3 / 2 {
                    aligned_h
                } else {
                    h
                };
                copy_nv12_planes(src, row_stride, plane_h, w, h, pool).map(|data| FramePixels::Nv12 {
                    data,
                    is_bt709: self.is_bt709,
                    full_range: self.full_range,
                })
            } else {
                let row_stride = self.default_stride.unsigned_abs().max((w * 4) as u32) as usize;
                let flip_y = self.default_stride < 0;
                if src.len() >= row_stride * h {
                    Some(FramePixels::Rgba(copy_bgra_to_color32(
                        src, row_stride, flip_y, w, h, pool,
                    )))
                } else {
                    None
                }
            }
        } else {
            None
        };

        unsafe {
            let _ = buffer.Unlock();
        }
        pixels
    }

    fn seek_video_only(
        &mut self,
        time_secs: f64,
        d3d_ctx: Option<&mut PlayerD3dContext>,
        pool: &FrameBufferPool,
    ) -> Option<(FramePixels, i64)> {
        let target_100ns = (time_secs.max(0.0) * 10_000_000.0) as i64;
        let pos = PROPVARIANT::from(target_100ns.max(0));
        let _ = unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &pos) };

        let mut chosen: Option<(IMFSample, i64)> = None;
        for _ in 0..600 {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample_opt: Option<IMFSample> = None;
            if unsafe {
                self.reader.ReadSample(
                    self.video_idx,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample_opt),
                )
            }
            .is_err()
            {
                break;
            }
            if (flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0 {
                self.refresh_video_layout();
            }
            if let Some(sample) = sample_opt {
                chosen = Some((sample, ts));
                if ts + PREROLL_TOL_100NS >= target_100ns {
                    break;
                }
            }
            if (flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32))
                != 0
            {
                break;
            }
        }

        let (sample, ts) = chosen?;
        let pixels = self.extract_frame(&sample, d3d_ctx, pool)?;
        Some((pixels, ts))
    }

    fn seek_av(&self, time_secs: f64) {
        let target_100ns = (time_secs.max(0.0) * 10_000_000.0) as i64;
        let pos = PROPVARIANT::from(target_100ns.max(0));
        let _ = unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &pos) };
    }
}

fn copy_nv12_planes(
    src: &[u8],
    pitch: usize,
    plane_h: usize,
    w: usize,
    h: usize,
    pool: &FrameBufferPool,
) -> Option<Vec<u8>> {
    let w = w & !1;
    let h = h & !1;
    if w < 2 || h < 2 || pitch < w || plane_h < h {
        return None;
    }
    let uv_h = h / 2;
    let uv_offset = plane_h * pitch;
    if src.len() < uv_offset + uv_h * pitch {
        return None;
    }

    let y_bytes = w * h;
    let uv_bytes = w * uv_h;
    let mut dst = pool.take_nv12(y_bytes + uv_bytes);

    if pitch == w {
        dst[..y_bytes].copy_from_slice(&src[..y_bytes]);
        dst[y_bytes..y_bytes + uv_bytes].copy_from_slice(&src[uv_offset..uv_offset + uv_bytes]);
    } else {
        for y in 0..h {
            let s = &src[y * pitch..y * pitch + w];
            dst[y * w..(y + 1) * w].copy_from_slice(s);
        }
        for y in 0..uv_h {
            let s = &src[uv_offset + y * pitch..uv_offset + y * pitch + w];
            let base = y_bytes + y * w;
            dst[base..base + w].copy_from_slice(s);
        }
    }

    Some(dst)
}

fn copy_bgra_to_color32(
    src: &[u8],
    pitch: usize,
    flip_y: bool,
    w: usize,
    h: usize,
    pool: &FrameBufferPool,
) -> Vec<Color32> {
    let mut dst = pool.take_rgba(w * h);
    for y in 0..h {
        let src_y = if flip_y { h - 1 - y } else { y };
        let src_row = &src[src_y * pitch..src_y * pitch + w * 4];
        let dst_row = &mut dst[y * w..(y + 1) * w];
        for (s, d) in src_row.chunks_exact(4).zip(dst_row.iter_mut()) {
            *d = Color32::from_rgb(s[2], s[1], s[0]);
        }
    }
    dst
}

fn push_audio_sample(
    sample: &IMFSample,
    audio_out: &mut WasapiAudioOut,
    volume_bits: &AtomicU32,
    muted_flag: &AtomicBool,
    pump_now: bool,
) {
    if let Ok(buf) = unsafe { sample.ConvertToContiguousBuffer() } {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        if unsafe { buf.Lock(&mut ptr, None, Some(&mut len)) }.is_ok() {
            if !ptr.is_null() && len > 0 {
                let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
                let vol = f32::from_bits(volume_bits.load(Ordering::Relaxed));
                let muted = muted_flag.load(Ordering::Relaxed);
                audio_out.push_pcm_i16(bytes, vol, muted);
                if pump_now {
                    audio_out.pump();
                }
            }
            unsafe {
                let _ = buf.Unlock();
            }
        }
    }
}

fn publish_player_frame(
    clip_ref: &OpenPlayerClip,
    pixels: FramePixels,
    pts_100ns: i64,
    seek_gen: u64,
    latest_frame: &Mutex<Option<PlayerFrame>>,
    pool: &FrameBufferPool,
    pos_bits: &AtomicU64,
    egui_ctx: &egui::Context,
) {
    let t = pts_100ns.max(0) as f64 / 10_000_000.0;
    pos_bits.store(t.to_bits(), Ordering::Release);
    if let Ok(mut slot) = latest_frame.lock() {
        if let Some(prev) = slot.replace(PlayerFrame {
            width: clip_ref.actual_w,
            height: clip_ref.actual_h,
            pixels,
            time_secs: t,
            clip_path: clip_ref.path.clone(),
            seek_gen,
        }) {
            pool.recycle_pixels(prev.pixels);
        }
    } else {
        pool.recycle_pixels(pixels);
    }
    egui_ctx.request_repaint();
}

#[allow(clippy::too_many_arguments)]
fn start_playback_with_preroll(
    clip_ref: &mut OpenPlayerClip,
    d3d_ctx: Option<&mut PlayerD3dContext>,
    pool: &FrameBufferPool,
    egui_ctx: &egui::Context,
    start_100ns: i64,
    loop_enabled: bool,
    loop_end_100ns: i64,
    active_gen: u64,
    current_pos_100ns: &mut i64,
    is_playing: &mut bool,
    play_origin_wall: &mut Instant,
    play_origin_pts: &mut i64,
    next_video: &mut Option<(FramePixels, u32, u32, i64)>,
    audio_out: &mut Option<WasapiAudioOut>,
    latest_frame: &Mutex<Option<PlayerFrame>>,
    playing_flag: &AtomicBool,
    pos_bits: &AtomicU64,
    volume_bits: &AtomicU32,
    muted_flag: &AtomicBool,
) {
    if let Some((old_px, _, _, _)) = next_video.take() {
        pool.recycle_pixels(old_px);
    }
    if let Some(a) = audio_out.as_mut() {
        a.stop_and_clear();
    }
    if clip_ref.audio_idx.is_some() && audio_out.is_none() {
        *audio_out = WasapiAudioOut::new().ok();
    }

    clip_ref.seek_av(start_100ns as f64 / 10_000_000.0);

    let mut chosen_video: Option<(IMFSample, i64)> = None;
    let mut fallback_video: Option<(IMFSample, i64)> = None;

    for _ in 0..1500 {
        let mut actual_stream = 0u32;
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample_opt: Option<IMFSample> = None;

        let read_res = unsafe {
            clip_ref.reader.ReadSample(
                MF_SOURCE_READER_ANY_STREAM.0 as u32,
                0,
                Some(&mut actual_stream),
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample_opt),
            )
        };
        if read_res.is_err() {
            break;
        }
        if (flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0
            && actual_stream == clip_ref.video_idx
        {
            clip_ref.refresh_video_layout();
        }
        if let Some(sample) = sample_opt {
            if actual_stream == clip_ref.video_idx {
                if loop_enabled && ts >= loop_end_100ns {
                    break;
                }
                if ts + PREROLL_TOL_100NS >= start_100ns {
                    chosen_video = Some((sample, ts));
                    break;
                }
                fallback_video = Some((sample, ts));
            } else if Some(actual_stream) == clip_ref.audio_idx
                && ts + PREROLL_TOL_100NS >= start_100ns
                && (!loop_enabled || ts < loop_end_100ns)
            {
                if let Some(a) = audio_out.as_mut() {
                    push_audio_sample(&sample, a, volume_bits, muted_flag, false);
                }
            }
        }
        if (flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32))
            != 0
        {
            break;
        }
    }

    let picked = chosen_video.or(fallback_video);
    if let Some((sample, first_pts)) = picked {
        if let Some(pixels) = clip_ref.extract_frame(&sample, d3d_ctx, pool) {
            let aligned_pts = first_pts.max(0);
            *current_pos_100ns = aligned_pts;
            *play_origin_pts = aligned_pts;
            publish_player_frame(
                clip_ref,
                pixels,
                aligned_pts,
                active_gen,
                latest_frame,
                pool,
                pos_bits,
                egui_ctx,
            );
        } else {
            let aligned_pts = start_100ns.max(0);
            *current_pos_100ns = aligned_pts;
            *play_origin_pts = aligned_pts;
            pos_bits.store(
                (aligned_pts as f64 / 10_000_000.0).to_bits(),
                Ordering::Release,
            );
        }
    } else {
        let aligned_pts = start_100ns.max(0);
        *current_pos_100ns = aligned_pts;
        *play_origin_pts = aligned_pts;
        pos_bits.store(
            (aligned_pts as f64 / 10_000_000.0).to_bits(),
            Ordering::Release,
        );
    }

    if let Some(a) = audio_out.as_mut() {
        a.start();
        a.pump();
    }
    *play_origin_wall = Instant::now();
    *is_playing = true;
    playing_flag.store(true, Ordering::Release);
}

#[allow(clippy::too_many_arguments)]
fn player_thread_loop(
    cmd_rx: Receiver<PlayerCmd>,
    egui_ctx: egui::Context,
    prefer_nv12: bool,
    latest_frame: Arc<Mutex<Option<PlayerFrame>>>,
    latest_metadata: Arc<Mutex<Option<(PathBuf, ClipMetadata)>>>,
    pool: FrameBufferPool,
    playing_flag: Arc<AtomicBool>,
    loading_flag: Arc<AtomicBool>,
    has_audio_flag: Arc<AtomicBool>,
    hw_accel_flag: Arc<AtomicBool>,
    nv12_flag: Arc<AtomicBool>,
    pos_bits: Arc<AtomicU64>,
    volume_bits: Arc<AtomicU32>,
    muted_flag: Arc<AtomicBool>,
) {
    let mut d3d_ctx: Option<PlayerD3dContext> = None;
    let mut d3d_init_tried = false;
    let mut clip: Option<OpenPlayerClip> = None;
    let mut audio_out: Option<WasapiAudioOut> = None;

    let mut is_playing = false;
    let mut current_pos_100ns: i64 = 0;
    let mut active_gen: u64 = 0;
    let mut play_origin_wall = Instant::now();
    let mut play_origin_pts: i64 = 0;
    let mut next_video: Option<(FramePixels, u32, u32, i64)> = None;

    let mut loop_enabled = false;
    let mut loop_start_secs = 0.0f64;
    let mut loop_end_secs = f64::MAX;

    let stop_playback = |is_playing: &mut bool,
                         next_video: &mut Option<(FramePixels, u32, u32, i64)>,
                         audio_out: &mut Option<WasapiAudioOut>| {
        *is_playing = false;
        if let Some((old_px, _, _, _)) = next_video.take() {
            pool.recycle_pixels(old_px);
        }
        playing_flag.store(false, Ordering::Release);
        if let Some(a) = audio_out.as_mut() {
            a.stop_and_clear();
        }
    };

    loop {
        // 1. Obtener siguiente comando (espera bloqueante si está pausado; non-blocking si reproduce).
        let first_cmd = if is_playing {
            match cmd_rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match cmd_rx.recv() {
                Ok(c) => Some(c),
                Err(_) => break,
            }
        };

        // 2. Colapsar múltiples Seek/LoadClip en ráfaga para scrubbing instantáneo.
        if let Some(cmd) = first_cmd {
            let mut batch = vec![cmd];
            while let Ok(more) = cmd_rx.try_recv() {
                batch.push(more);
            }

            let mut should_exit = false;
            let mut deferred_seek: Option<f64> = None;
            let mut desired_play: Option<bool> = None;

            for item in batch {
                match item {
                    PlayerCmd::Shutdown => {
                        should_exit = true;
                        break;
                    }
                    PlayerCmd::ReleaseClip { ack } => {
                        deferred_seek = None;
                        desired_play = None;
                        stop_playback(&mut is_playing, &mut next_video, &mut audio_out);
                        clip = None;
                        audio_out = None;
                        if let Some(d3d) = d3d_ctx.as_mut() {
                            d3d.staging = None;
                        }
                        has_audio_flag.store(false, Ordering::Release);
                        hw_accel_flag.store(false, Ordering::Release);
                        nv12_flag.store(false, Ordering::Release);
                        loading_flag.store(false, Ordering::Release);
                        if let Some(tx) = ack {
                            let _ = tx.send(());
                        }
                    }
                    PlayerCmd::LoadClip {
                        path,
                        start_secs,
                        seq,
                    } => {
                        active_gen = active_gen.max(seq);
                        deferred_seek = None;
                        desired_play = None;
                        stop_playback(&mut is_playing, &mut next_video, &mut audio_out);
                        clip = None;
                        has_audio_flag.store(false, Ordering::Release);
                        hw_accel_flag.store(false, Ordering::Release);
                        nv12_flag.store(false, Ordering::Release);
                        loading_flag.store(true, Ordering::Release);

                        if !d3d_init_tried {
                            d3d_init_tried = true;
                            match PlayerD3dContext::new() {
                                Ok(ctx) => d3d_ctx = Some(ctx),
                                Err(err) => {
                                    warn!("Sin contexto D3D11 en reproductor; usando decodificación CPU: {err:#}");
                                }
                            }
                        }

                        match OpenPlayerClip::open(&path, d3d_ctx.as_ref(), prefer_nv12) {
                            Ok(mut opened) => {
                                has_audio_flag.store(opened.audio_idx.is_some(), Ordering::Release);
                                hw_accel_flag.store(opened.hw_accel, Ordering::Release);
                                nv12_flag.store(opened.output_is_nv12, Ordering::Release);
                                if let Ok(mut mslot) = latest_metadata.lock() {
                                    *mslot = Some((path.clone(), opened.metadata));
                                }
                                let dur = opened.metadata.duration_secs.max(0.0);
                                let target_t = start_secs.clamp(0.0, dur);
                                if let Some((pixels, ts)) =
                                    opened.seek_video_only(target_t, d3d_ctx.as_mut(), &pool)
                                {
                                    current_pos_100ns = ts.max(0);
                                    publish_player_frame(
                                        &opened,
                                        pixels,
                                        current_pos_100ns,
                                        active_gen,
                                        &latest_frame,
                                        &pool,
                                        &pos_bits,
                                        &egui_ctx,
                                    );
                                } else {
                                    current_pos_100ns = 0;
                                    pos_bits.store(0.0f64.to_bits(), Ordering::Release);
                                    egui_ctx.request_repaint();
                                }
                                clip = Some(opened);
                            }
                            Err(err) => {
                                debug!("No se pudo abrir clip en reproductor: {err:#}");
                            }
                        }
                        loading_flag.store(false, Ordering::Release);
                    }
                    PlayerCmd::Play => {
                        desired_play = Some(true);
                    }
                    PlayerCmd::Pause => {
                        desired_play = Some(false);
                    }
                    PlayerCmd::TogglePlay => {
                        let currently_playing = desired_play.unwrap_or(is_playing);
                        desired_play = Some(!currently_playing);
                    }
                    PlayerCmd::Seek { time_secs, seq } => {
                        active_gen = active_gen.max(seq);
                        deferred_seek = Some(time_secs);
                    }
                    PlayerCmd::Step { delta_secs, seq } => {
                        active_gen = active_gen.max(seq);
                        let base = deferred_seek.unwrap_or_else(|| {
                            current_pos_100ns.max(0) as f64 / 10_000_000.0
                        });
                        deferred_seek = Some(base + delta_secs);
                    }
                    PlayerCmd::SetLoop {
                        enabled,
                        start_secs,
                        end_secs,
                    } => {
                        loop_enabled = enabled;
                        loop_start_secs = start_secs.max(0.0);
                        loop_end_secs = end_secs.max(loop_start_secs + 0.1);
                    }
                }
            }

            if should_exit {
                break;
            }

            if let Some(target_t) = deferred_seek {
                if let Some(c) = clip.as_mut() {
                    let resume_after = desired_play.unwrap_or(is_playing);
                    if is_playing {
                        stop_playback(&mut is_playing, &mut next_video, &mut audio_out);
                    }
                    let clamped = target_t.clamp(0.0, c.metadata.duration_secs.max(0.0));
                    let clamped_100ns = (clamped * 10_000_000.0) as i64;
                    if resume_after {
                        let loop_end_100ns = (loop_end_secs * 10_000_000.0) as i64;
                        start_playback_with_preroll(
                            c,
                            d3d_ctx.as_mut(),
                            &pool,
                            &egui_ctx,
                            clamped_100ns,
                            loop_enabled,
                            loop_end_100ns,
                            active_gen,
                            &mut current_pos_100ns,
                            &mut is_playing,
                            &mut play_origin_wall,
                            &mut play_origin_pts,
                            &mut next_video,
                            &mut audio_out,
                            &latest_frame,
                            &playing_flag,
                            &pos_bits,
                            &volume_bits,
                            &muted_flag,
                        );
                        desired_play = None;
                    } else if let Some((pixels, ts)) =
                        c.seek_video_only(clamped, d3d_ctx.as_mut(), &pool)
                    {
                        current_pos_100ns = ts.max(0);
                        publish_player_frame(
                            c,
                            pixels,
                            current_pos_100ns,
                            active_gen,
                            &latest_frame,
                            &pool,
                            &pos_bits,
                            &egui_ctx,
                        );
                    } else {
                        current_pos_100ns = clamped_100ns;
                        pos_bits.store(clamped.to_bits(), Ordering::Release);
                        egui_ctx.request_repaint();
                    }
                }
                loading_flag.store(false, Ordering::Release);
            }

            if let Some(want_play) = desired_play {
                if !want_play {
                    if is_playing {
                        stop_playback(&mut is_playing, &mut next_video, &mut audio_out);
                    }
                } else if let Some(c) = clip.as_mut() {
                    if !is_playing {
                        let dur_100ns = (c.metadata.duration_secs * 10_000_000.0) as i64;
                        let mut start_100ns = current_pos_100ns;
                        if loop_enabled {
                            let ls = (loop_start_secs * 10_000_000.0) as i64;
                            let le = (loop_end_secs * 10_000_000.0) as i64;
                            if start_100ns < ls || start_100ns >= le.saturating_sub(500_000) {
                                start_100ns = ls;
                            }
                        } else if dur_100ns > 500_000 && start_100ns >= dur_100ns - 500_000 {
                            start_100ns = 0;
                        }
                        let loop_end_100ns = (loop_end_secs * 10_000_000.0) as i64;
                        start_playback_with_preroll(
                            c,
                            d3d_ctx.as_mut(),
                            &pool,
                            &egui_ctx,
                            start_100ns,
                            loop_enabled,
                            loop_end_100ns,
                            active_gen,
                            &mut current_pos_100ns,
                            &mut is_playing,
                            &mut play_origin_wall,
                            &mut play_origin_pts,
                            &mut next_video,
                            &mut audio_out,
                            &latest_frame,
                            &playing_flag,
                            &pos_bits,
                            &volume_bits,
                            &muted_flag,
                        );
                    }
                }
            }
        }

        if !is_playing {
            continue;
        }

        // 3. Paso de reproducción: alimentar WASAPI + decodificar/presentar el siguiente frame.
        if let Some(a) = audio_out.as_mut() {
            a.pump();
        }

        let Some(c) = clip.as_mut() else {
            stop_playback(&mut is_playing, &mut next_video, &mut audio_out);
            continue;
        };

        // Decodificar hasta tener el próximo frame de video (encolando paquetes de audio en el camino).
        let mut hit_eof = false;
        let mut hit_loop_end = false;
        let loop_end_100ns = (loop_end_secs * 10_000_000.0) as i64;

        while next_video.is_none() {
            let mut actual_stream = 0u32;
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample_opt: Option<IMFSample> = None;

            let read_res = unsafe {
                c.reader.ReadSample(
                    MF_SOURCE_READER_ANY_STREAM.0 as u32,
                    0,
                    Some(&mut actual_stream),
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample_opt),
                )
            };

            if let Err(err) = read_res {
                warn!("ReadSample en reproductor falló: {err}");
                hit_eof = true;
                break;
            }

            if (flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0
                && actual_stream == c.video_idx
            {
                c.refresh_video_layout();
                nv12_flag.store(c.output_is_nv12, Ordering::Release);
            }

            if let Some(sample) = sample_opt {
                if actual_stream == c.video_idx {
                    if ts + PREROLL_TOL_100NS < play_origin_pts {
                        continue;
                    }
                    if loop_enabled && ts >= loop_end_100ns {
                        hit_loop_end = true;
                        break;
                    }
                    if let Some(pixels) = c.extract_frame(&sample, d3d_ctx.as_mut(), &pool) {
                        next_video = Some((pixels, c.actual_w, c.actual_h, ts));
                    }
                } else if Some(actual_stream) == c.audio_idx
                    && (!loop_enabled || ts < loop_end_100ns)
                    && ts + 200_000 >= play_origin_pts
                {
                    if let Some(a) = audio_out.as_mut() {
                        push_audio_sample(&sample, a, &volume_bits, &muted_flag, true);
                    }
                }
            }

            if (flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 | MF_SOURCE_READERF_ERROR.0 as u32))
                != 0
            {
                if loop_enabled {
                    hit_loop_end = true;
                } else {
                    hit_eof = true;
                }
                break;
            }
        }

        if hit_loop_end {
            let ls_100ns = (loop_start_secs * 10_000_000.0) as i64;
            start_playback_with_preroll(
                c,
                d3d_ctx.as_mut(),
                &pool,
                &egui_ctx,
                ls_100ns,
                loop_enabled,
                loop_end_100ns,
                active_gen,
                &mut current_pos_100ns,
                &mut is_playing,
                &mut play_origin_wall,
                &mut play_origin_pts,
                &mut next_video,
                &mut audio_out,
                &latest_frame,
                &playing_flag,
                &pos_bits,
                &volume_bits,
                &muted_flag,
            );
            continue;
        }

        if let Some((pixels, _w, _h, pts)) = next_video.take() {
            let rel_100ns = (pts - play_origin_pts).max(0);
            let target_elapsed = Duration::from_nanos(rel_100ns as u64 * 100);
            let now_elapsed = play_origin_wall.elapsed();

            if target_elapsed > now_elapsed {
                let wait = target_elapsed - now_elapsed;
                if wait > Duration::from_millis(2) {
                    // Dormir hasta 5 ms por paso para mantener WASAPI alimentado y comandos instantáneos.
                    let step_wait = wait.min(Duration::from_millis(5));
                    thread::sleep(step_wait);
                    if let Some(a) = audio_out.as_mut() {
                        a.pump();
                    }
                    // Devolver el frame para presentarlo tras la espera (si no llegaron comandos).
                    next_video = Some((pixels, c.actual_w, c.actual_h, pts));
                    continue;
                }
            }

            current_pos_100ns = pts.max(0);
            publish_player_frame(
                c,
                pixels,
                current_pos_100ns,
                active_gen,
                &latest_frame,
                &pool,
                &pos_bits,
                &egui_ctx,
            );
        }

        if hit_eof {
            stop_playback(&mut is_playing, &mut next_video, &mut audio_out);
        }
    }

    if let Some((old_px, _, _, _)) = next_video.take() {
        pool.recycle_pixels(old_px);
    }
    drop(audio_out);
    drop(clip);
    drop(d3d_ctx);
}

#[inline]
const fn unpack_u32_pair(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

#[inline]
const fn pack_u32_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_padded_nv12_planes_tightly() {
        let w = 8usize;
        let h = 4usize;
        let pitch = 16usize;
        let plane_h = 6usize; // simula padding macrobloque de textura D3D11
        let mut src = vec![0xFFu8; pitch * (plane_h + plane_h / 2)];

        for y in 0..h {
            for x in 0..w {
                src[y * pitch + x] = (y * 10 + x) as u8;
            }
        }
        let uv_off = plane_h * pitch;
        for y in 0..(h / 2) {
            for x in 0..w {
                src[uv_off + y * pitch + x] = (100 + y * 10 + x) as u8;
            }
        }

        let pool = FrameBufferPool::default();
        let out = copy_nv12_planes(&src, pitch, plane_h, w, h, &pool).expect("NV12 válido");
        assert_eq!(out.len(), w * h + w * (h / 2));
        assert_eq!(&out[0..w], &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(&out[3 * w..4 * w], &[30, 31, 32, 33, 34, 35, 36, 37]);
        assert_eq!(&out[w * h..w * h + w], &[100, 101, 102, 103, 104, 105, 106, 107]);
        assert_eq!(
            &out[w * h + w..w * h + 2 * w],
            &[110, 111, 112, 113, 114, 115, 116, 117]
        );
    }

    #[test]
    fn opens_sample_clip_with_gpu_nv12_if_present() {
        let clips_dir = crate::config::AppConfig::load_or_create().clips_dir;
        let Ok(entries) = std::fs::read_dir(&clips_dir) else {
            return;
        };
        let Some(mp4) = entries
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")))
        else {
            return;
        };

        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        unsafe { MFStartup(MF_VERSION, 0) }.expect("MFStartup");

        let mut d3d_ctx = PlayerD3dContext::new().ok();
        let pool = FrameBufferPool::default();
        let mut opened =
            OpenPlayerClip::open(&mp4, d3d_ctx.as_ref(), true).expect("Abrir clip sample");
        let (pixels, _ts) = opened
            .seek_video_only(0.2, d3d_ctx.as_mut(), &pool)
            .expect("Extraer frame sample");

        match pixels {
            FramePixels::Nv12 { data, .. } => {
                assert!(
                    data.len() >= (opened.actual_w * opened.actual_h * 3 / 2) as usize,
                    "buffer NV12 completo"
                );
            }
            FramePixels::Rgba(v) => {
                assert_eq!(v.len(), (opened.actual_w * opened.actual_h) as usize);
            }
        }
        println!(
            "Sample player pipeline: hw_accel={}, nv12={}, {}x{}",
            opened.hw_accel, opened.output_is_nv12, opened.actual_w, opened.actual_h
        );

        drop(opened);
        drop(d3d_ctx);
        unsafe {
            let _ = MFShutdown();
            RoUninitialize();
        }
    }
}
