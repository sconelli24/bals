use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use tracing::{debug, warn};
use wasapi::{AudioClient, AudioRenderClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaBuffer, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SOURCE_READER_ANY_STREAM, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFAudioFormat_PCM,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Audio,
    MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{GUID, HSTRING};

use crate::trimmer::ClipMetadata;

const PLAYER_MAX_W: u32 = 960;
const PLAYER_MAX_H: u32 = 540;
const AUDIO_SAMPLE_RATE: usize = 48_000;
const AUDIO_CHANNELS: usize = 2;
const AUDIO_BYTES_PER_FRAME: usize = AUDIO_CHANNELS * 2; // 16-bit stereo = 4 bytes
const PREROLL_TOL_100NS: i64 = 150_000; // 15 ms

/// Frame decodificado listo para renderizar en el Monitor CRT de la UI.
pub struct PlayerFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub time_secs: f64,
    pub clip_path: PathBuf,
    pub seek_gen: u64,
}

enum PlayerCmd {
    LoadClip { path: PathBuf, start_secs: f64, seq: u64 },
    #[allow(dead_code)]
    Play,
    Pause,
    TogglePlay,
    Seek { time_secs: f64, seq: u64 },
    Step { delta_secs: f64, seq: u64 },
    SetLoop { enabled: bool, start_secs: f64, end_secs: f64 },
    Shutdown,
}

/// Reproductor nativo liviano (Windows Media Foundation + WASAPI, 0 dependencias externas)
/// para previsualizar clips y marcar `[IN]` / `[OUT]` con precisión dentro del Monitor CRT.
pub struct ClipPlayer {
    cmd_tx: Sender<PlayerCmd>,
    latest_frame: Arc<Mutex<Option<PlayerFrame>>>,
    latest_metadata: Arc<Mutex<Option<(PathBuf, ClipMetadata)>>>,
    playing: Arc<AtomicBool>,
    loading: Arc<AtomicBool>,
    has_audio: Arc<AtomicBool>,
    position_bits: Arc<AtomicU64>,
    seek_gen: Arc<AtomicU64>,
    volume_bits: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ClipPlayer {
    pub fn start(initial_volume: f32, initial_muted: bool) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<PlayerCmd>();
        let latest_frame = Arc::new(Mutex::new(None));
        let latest_metadata = Arc::new(Mutex::new(None));
        let playing = Arc::new(AtomicBool::new(false));
        let loading = Arc::new(AtomicBool::new(false));
        let has_audio = Arc::new(AtomicBool::new(false));
        let position_bits = Arc::new(AtomicU64::new(0.0f64.to_bits()));
        let seek_gen = Arc::new(AtomicU64::new(0));
        let volume_bits = Arc::new(AtomicU32::new(initial_volume.clamp(0.0, 1.0).to_bits()));
        let muted = Arc::new(AtomicBool::new(initial_muted));

        let frame_bg = Arc::clone(&latest_frame);
        let meta_bg = Arc::clone(&latest_metadata);
        let playing_bg = Arc::clone(&playing);
        let loading_bg = Arc::clone(&loading);
        let audio_bg = Arc::clone(&has_audio);
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
                    frame_bg,
                    meta_bg,
                    playing_bg,
                    loading_bg,
                    audio_bg,
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
            playing,
            loading,
            has_audio,
            position_bits,
            seek_gen,
            volume_bits,
            muted,
            handle,
        }
    }

    pub fn load_clip(&self, path: PathBuf, start_secs: f64) {
        let seq = self.seek_gen.fetch_add(1, Ordering::AcqRel) + 1;
        self.playing.store(false, Ordering::Release);
        self.loading.store(true, Ordering::Release);
        let _ = self
            .cmd_tx
            .send(PlayerCmd::LoadClip { path, start_secs, seq });
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
        let _ = self
            .cmd_tx
            .send(PlayerCmd::Step { delta_secs, seq });
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

    pub fn take_frame(&self) -> Option<PlayerFrame> {
        let frame = self.latest_frame.lock().ok()?.take()?;
        if frame.seek_gen < self.seek_gen.load(Ordering::Acquire) {
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

        let format = WaveFormat::new(16, 16, SampleType::Int, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS, None);
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
        if !self.running {
            if self.client.start_stream().is_ok() {
                self.running = true;
            }
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

    #[allow(dead_code)]
    fn queued_ms(&self) -> usize {
        (self.pcm_bytes.len() / AUDIO_BYTES_PER_FRAME) * 1000 / AUDIO_SAMPLE_RATE
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
    metadata: ClipMetadata,
    rgba_scratch: Vec<u8>,
}

impl OpenPlayerClip {
    fn open(path: &Path) -> Result<Self> {
        let mut attrs: Option<IMFAttributes> = None;
        unsafe { MFCreateAttributes(&mut attrs, 2) }?;
        let attrs = attrs.context("MFCreateAttributes (player) devolvió None")?;
        unsafe {
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
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
                unsafe { reader.SetStreamSelection(idx, true) }?;
            } else if major == Some(MFMediaType_Audio) && audio_idx.is_none() {
                audio_idx = Some(idx);
                let _ = unsafe { reader.SetStreamSelection(idx, true) };
            } else {
                let _ = unsafe { reader.SetStreamSelection(idx, false) };
            }
        }

        let video_idx = video_idx.context("El archivo no tiene pista de video")?;

        let scale = (PLAYER_MAX_W as f64 / src_w as f64)
            .min(PLAYER_MAX_H as f64 / src_h as f64)
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

        let current_type = unsafe { reader.GetCurrentMediaType(video_idx) }?;
        let (actual_w, actual_h) = unsafe { current_type.GetUINT64(&MF_MT_FRAME_SIZE) }
            .map(unpack_u32_pair)
            .unwrap_or((out_w, out_h));
        let default_stride = unsafe { current_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .map(|u| u as i32)
            .unwrap_or((actual_w * 4) as i32);

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
            metadata,
            rgba_scratch: vec![0u8; (actual_w * actual_h * 4) as usize],
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
        }
    }

    fn sample_to_rgba(&mut self, sample: &IMFSample) -> Option<Vec<u8>> {
        let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }.ok()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len = 0u32;
        unsafe { buffer.Lock(&mut ptr, None, Some(&mut cur_len)) }.ok()?;

        let w = self.actual_w as usize;
        let h = self.actual_h as usize;
        let row_stride = self.default_stride.unsigned_abs().max((w * 4) as u32) as usize;
        let flip_y = self.default_stride < 0;
        let needed = w * h * 4;
        if self.rgba_scratch.len() != needed {
            self.rgba_scratch.resize(needed, 0);
        }

        let ok = if !ptr.is_null() && cur_len as usize >= row_stride * h {
            let src = unsafe { std::slice::from_raw_parts(ptr, cur_len as usize) };
            for y in 0..h {
                let src_y = if flip_y { h - 1 - y } else { y };
                let src_row = &src[src_y * row_stride..src_y * row_stride + w * 4];
                let dst_row = &mut self.rgba_scratch[y * w * 4..(y + 1) * w * 4];
                for (s, d) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 255;
                }
            }
            true
        } else {
            false
        };

        unsafe {
            let _ = buffer.Unlock();
        }

        if ok {
            Some(self.rgba_scratch.clone())
        } else {
            None
        }
    }

    fn seek_video_only(&mut self, time_secs: f64) -> Option<(Vec<u8>, i64)> {
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
        let rgba = self.sample_to_rgba(&sample)?;
        Some((rgba, ts))
    }

    fn seek_av(&self, time_secs: f64) {
        let target_100ns = (time_secs.max(0.0) * 10_000_000.0) as i64;
        let pos = PROPVARIANT::from(target_100ns.max(0));
        let _ = unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &pos) };
    }
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
    rgba: Vec<u8>,
    pts_100ns: i64,
    seek_gen: u64,
    latest_frame: &Mutex<Option<PlayerFrame>>,
    pos_bits: &AtomicU64,
) {
    let t = pts_100ns.max(0) as f64 / 10_000_000.0;
    pos_bits.store(t.to_bits(), Ordering::Release);
    if let Ok(mut slot) = latest_frame.lock() {
        *slot = Some(PlayerFrame {
            width: clip_ref.actual_w,
            height: clip_ref.actual_h,
            rgba,
            time_secs: t,
            clip_path: clip_ref.path.clone(),
            seek_gen,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn start_playback_with_preroll(
    clip_ref: &mut OpenPlayerClip,
    start_100ns: i64,
    loop_enabled: bool,
    loop_end_100ns: i64,
    active_gen: u64,
    current_pos_100ns: &mut i64,
    is_playing: &mut bool,
    play_origin_wall: &mut Instant,
    play_origin_pts: &mut i64,
    next_video: &mut Option<(Vec<u8>, u32, u32, i64)>,
    audio_out: &mut Option<WasapiAudioOut>,
    latest_frame: &Mutex<Option<PlayerFrame>>,
    playing_flag: &AtomicBool,
    pos_bits: &AtomicU64,
    volume_bits: &AtomicU32,
    muted_flag: &AtomicBool,
) {
    *next_video = None;
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
        if let Some(rgba) = clip_ref.sample_to_rgba(&sample) {
            let aligned_pts = first_pts.max(0);
            *current_pos_100ns = aligned_pts;
            *play_origin_pts = aligned_pts;
            publish_player_frame(
                clip_ref,
                rgba,
                aligned_pts,
                active_gen,
                latest_frame,
                pos_bits,
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
    latest_frame: Arc<Mutex<Option<PlayerFrame>>>,
    latest_metadata: Arc<Mutex<Option<(PathBuf, ClipMetadata)>>>,
    playing_flag: Arc<AtomicBool>,
    loading_flag: Arc<AtomicBool>,
    has_audio_flag: Arc<AtomicBool>,
    pos_bits: Arc<AtomicU64>,
    volume_bits: Arc<AtomicU32>,
    muted_flag: Arc<AtomicBool>,
) {
    let mut clip: Option<OpenPlayerClip> = None;
    let mut audio_out: Option<WasapiAudioOut> = None;

    let mut is_playing = false;
    let mut current_pos_100ns: i64 = 0;
    let mut active_gen: u64 = 0;
    let mut play_origin_wall = Instant::now();
    let mut play_origin_pts: i64 = 0;
    let mut next_video: Option<(Vec<u8>, u32, u32, i64)> = None;

    let mut loop_enabled = false;
    let mut loop_start_secs = 0.0f64;
    let mut loop_end_secs = f64::MAX;

    let stop_playback = |is_playing: &mut bool,
                         next_video: &mut Option<(Vec<u8>, u32, u32, i64)>,
                         audio_out: &mut Option<WasapiAudioOut>| {
        *is_playing = false;
        *next_video = None;
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
                        loading_flag.store(true, Ordering::Release);

                        match OpenPlayerClip::open(&path) {
                            Ok(mut opened) => {
                                has_audio_flag.store(opened.audio_idx.is_some(), Ordering::Release);
                                if let Ok(mut mslot) = latest_metadata.lock() {
                                    *mslot = Some((path.clone(), opened.metadata));
                                }
                                let dur = opened.metadata.duration_secs.max(0.0);
                                let target_t = start_secs.clamp(0.0, dur);
                                if let Some((rgba, ts)) = opened.seek_video_only(target_t) {
                                    current_pos_100ns = ts.max(0);
                                    publish_player_frame(
                                        &opened,
                                        rgba,
                                        current_pos_100ns,
                                        active_gen,
                                        &latest_frame,
                                        &pos_bits,
                                    );
                                } else {
                                    current_pos_100ns = 0;
                                    pos_bits.store(0.0f64.to_bits(), Ordering::Release);
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
                    } else if let Some((rgba, ts)) = c.seek_video_only(clamped) {
                        current_pos_100ns = ts.max(0);
                        publish_player_frame(
                            c,
                            rgba,
                            current_pos_100ns,
                            active_gen,
                            &latest_frame,
                            &pos_bits,
                        );
                    } else {
                        current_pos_100ns = clamped_100ns;
                        pos_bits.store(clamped.to_bits(), Ordering::Release);
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
                    if let Some(rgba) = c.sample_to_rgba(&sample) {
                        next_video = Some((rgba, c.actual_w, c.actual_h, ts));
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

        if let Some((rgba, _w, _h, pts)) = next_video.take() {
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
                    next_video = Some((rgba, c.actual_w, c.actual_h, pts));
                    continue;
                }
            }

            current_pos_100ns = pts.max(0);
            publish_player_frame(
                c,
                rgba,
                current_pos_100ns,
                active_gen,
                &latest_frame,
                &pos_bits,
            );
        }

        if hit_eof {
            stop_playback(&mut is_playing, &mut next_video, &mut audio_out);
        }
    }

    drop(audio_out);
    drop(clip);
}

#[inline]
const fn unpack_u32_pair(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

#[inline]
const fn pack_u32_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}
