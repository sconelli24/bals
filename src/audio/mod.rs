pub mod aac_mft;
pub mod capture;
pub mod meter;

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, TrySendError, bounded};
use tracing::{debug, info, warn};

use crate::audio::aac_mft::AacEncoder;
use crate::audio::capture::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, CaptureKind, WasapiStream};
use crate::clock;
use crate::config::AudioConfig;

/// Paquete AAC-LC codificado (payload raw) con marca de tiempo en el mismo reloj QPC que el video.
#[derive(Debug, Clone)]
pub struct EncodedAudioPacket {
    pub data: Arc<[u8]>,
    /// Timestamp absoluto de presentación en ticks de 100 ns (`clock::now_100ns()`).
    pub pts_100ns: i64,
    /// Duración del paquete en ticks de 100 ns (~213_333 para 1024 muestras @ 48 kHz).
    pub duration_100ns: i64,
}

/// Metadatos del flujo AAC necesarios para que el muxer MP4 escriba la pista de audio en passthrough.
#[derive(Debug, Clone)]
pub struct AudioStreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate_bps: u32,
    /// `MF_MT_USER_DATA` (`HEAACWAVEINFO` + `AudioSpecificConfig`) generado por el AAC MFT.
    pub user_data: Vec<u8>,
}

/// Pipeline completo de audio:
/// Captura juego/sistema (WASAPI Loopback) + micrófono opcional → mezcla PCM 48 kHz con
/// soft-clipping → codifica AAC-LC vía Media Foundation → emite paquetes sincronizados.
pub struct AudioPipeline {
    pub rx: Receiver<EncodedAudioPacket>,
    pub stream_info: AudioStreamInfo,
    game_volume_bits: Arc<AtomicU32>,
    mic_volume_bits: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    mixer_handle: Option<JoinHandle<()>>,
    _game_stream: WasapiStream,
    _mic_stream: Option<WasapiStream>,
}

impl AudioPipeline {
    pub fn start(config: AudioConfig) -> Result<Self> {
        let game_stream = WasapiStream::start(CaptureKind::GameOrSystem(config.source))
            .context("No se pudo iniciar la captura de audio del juego/sistema")?;

        let mic_stream = if config.mic_enabled {
            match WasapiStream::start(CaptureKind::Microphone) {
                Ok(mic) => Some(mic),
                Err(err) => {
                    warn!("Micrófono no disponible ({err:#}); grabando solo audio del juego/sistema");
                    None
                }
            }
        } else {
            None
        };

        let game_queue = Arc::clone(&game_stream.samples);
        let mic_queue = mic_stream.as_ref().map(|m| Arc::clone(&m.samples));
        let game_volume_bits = Arc::new(AtomicU32::new(config.game_volume.to_bits()));
        let mic_volume_bits = Arc::new(AtomicU32::new(config.mic_volume.to_bits()));

        let (tx, rx) = bounded::<EncodedAudioPacket>(256);
        let (info_tx, info_rx) = crossbeam_channel::bounded::<Result<AudioStreamInfo>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let g_vol_clone = Arc::clone(&game_volume_bits);
        let m_vol_clone = Arc::clone(&mic_volume_bits);

        let mixer_handle = thread::Builder::new()
            .name("bals-audio-mixer-aac".to_string())
            .spawn(move || {
                mixer_aac_thread(
                    config,
                    game_queue,
                    mic_queue,
                    g_vol_clone,
                    m_vol_clone,
                    tx,
                    info_tx,
                    stop_clone,
                );
            })
            .context("No se pudo iniciar el hilo mezclador/encoder AAC")?;

        let stream_info = match info_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(res) => res?,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::SeqCst);
                anyhow::bail!("Tiempo de espera agotado al inicializar el encoder AAC");
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("El hilo encoder AAC terminó antes de inicializar");
            }
        };

        info!(
            mic_active = mic_stream.is_some(),
            bitrate_kbps = stream_info.bitrate_bps / 1000,
            "Pipeline de audio de Bals activo"
        );

        Ok(Self {
            rx,
            stream_info,
            game_volume_bits,
            mic_volume_bits,
            stop,
            mixer_handle: Some(mixer_handle),
            _game_stream: game_stream,
            _mic_stream: mic_stream,
        })
    }

    /// Ajusta volúmenes de juego y micrófono en vivo sin reiniciar la grabación ni perder el Replay Buffer.
    pub fn set_volumes(&self, game_volume: f32, mic_volume: f32) {
        self.game_volume_bits
            .store(game_volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
        self.mic_volume_bits
            .store(mic_volume.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.mixer_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn mixer_aac_thread(
    config: AudioConfig,
    game_queue: Arc<std::sync::Mutex<std::collections::VecDeque<f32>>>,
    mic_queue: Option<Arc<std::sync::Mutex<std::collections::VecDeque<f32>>>>,
    game_volume_bits: Arc<AtomicU32>,
    mic_volume_bits: Arc<AtomicU32>,
    tx: crossbeam_channel::Sender<EncodedAudioPacket>,
    info_tx: crossbeam_channel::Sender<Result<AudioStreamInfo>>,
    stop: Arc<AtomicBool>,
) {
    let mut encoder = match AacEncoder::new(config) {
        Ok(enc) => {
            let info = AudioStreamInfo {
                sample_rate: enc.sample_rate,
                channels: enc.channels,
                bitrate_bps: enc.bitrate_bps,
                user_data: enc.user_data.clone(),
            };
            let _ = info_tx.send(Ok(info));
            enc
        }
        Err(err) => {
            let _ = info_tx.send(Err(err));
            return;
        }
    };

    // Un bloque AAC-LC exacto = 1024 frames estéreo = 2048 muestras f32 = 21.333 ms @ 48 kHz.
    const FRAMES_PER_CHUNK: usize = 1024;
    const SAMPLES_PER_CHUNK: usize = FRAMES_PER_CHUNK * AUDIO_CHANNELS;
    let chunk_dur_100ns = (FRAMES_PER_CHUNK as i64 * 10_000_000) / AUDIO_SAMPLE_RATE as i64;

    let mut game_buf = vec![0.0f32; SAMPLES_PER_CHUNK];
    let mut mic_buf = vec![0.0f32; SAMPLES_PER_CHUNK];
    let mut pcm_i16 = vec![0i16; SAMPLES_PER_CHUNK];

    // Reloj continuo anclado a `clock::now_100ns()` al inicio, avanzando exactamente `chunk_dur_100ns`
    // por cada bloque de 1024 muestras (evita huecos/solapamientos de timestamps en MP4).
    let mut next_pts_100ns = clock::now_100ns();
    let mut next_wakeup = Instant::now() + Duration::from_nanos((chunk_dur_100ns * 100) as u64);

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if next_wakeup > now {
            let sleep_dur = (next_wakeup - now).min(Duration::from_millis(10));
            thread::sleep(sleep_dur);
            continue;
        }

        // Extraer 1024 frames del juego/sistema (rellenando silencio 0.0 si el sistema está mudo).
        drain_or_silence(&game_queue, &mut game_buf);

        // Extraer 1024 frames del micrófono (si está habilitado).
        if let Some(mic_q) = &mic_queue {
            drain_or_silence(mic_q, &mut mic_buf);
        } else {
            mic_buf.fill(0.0);
        }

        // Mezcla en punto flotante con ganancia en vivo y soft-clipping `tanh`.
        let g_vol = f32::from_bits(game_volume_bits.load(Ordering::Relaxed));
        let m_vol = f32::from_bits(mic_volume_bits.load(Ordering::Relaxed));
        for i in 0..SAMPLES_PER_CHUNK {
            let mixed = game_buf[i] * g_vol + mic_buf[i] * m_vol;
            let limited = if mixed.abs() > 0.95 {
                mixed.tanh()
            } else {
                mixed
            };
            pcm_i16[i] = (limited.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        }

        match encoder.encode_pcm_i16(&pcm_i16, next_pts_100ns) {
            Ok(packets) => {
                for pkt in packets {
                    if let Err(TrySendError::Disconnected(_)) = tx.try_send(pkt) {
                        return;
                    }
                }
            }
            Err(err) => {
                warn!("Error codificando bloque AAC: {err:#}");
            }
        }

        next_pts_100ns += chunk_dur_100ns;
        next_wakeup += Duration::from_nanos((chunk_dur_100ns * 100) as u64);

        // Si el hilo se atrasó mucho (>150 ms, ej. hibernación), resincronizar el reloj.
        let wall_now = Instant::now();
        if wall_now > next_wakeup + Duration::from_millis(150) {
            debug!("Resincronizando reloj del mezclador de audio tras pausa del sistema");
            next_pts_100ns = clock::now_100ns();
            next_wakeup = wall_now + Duration::from_nanos((chunk_dur_100ns * 100) as u64);
        }
    }

    if let Ok(packets) = encoder.flush() {
        for pkt in packets {
            let _ = tx.try_send(pkt);
        }
    }
}

/// Copia contigua por slices (`as_slices`) reduciendo el tiempo bajo lock del mutex.
fn drain_or_silence(
    queue: &std::sync::Mutex<std::collections::VecDeque<f32>>,
    out: &mut [f32],
) {
    let available = {
        let mut guard = queue.lock().unwrap();
        let avail = guard.len().min(out.len());
        if avail > 0 {
            let (s1, s2) = guard.as_slices();
            let n1 = avail.min(s1.len());
            out[..n1].copy_from_slice(&s1[..n1]);
            if n1 < avail {
                let n2 = avail - n1;
                out[n1..avail].copy_from_slice(&s2[..n2]);
            }
            guard.drain(..avail);
        }
        avail
    };
    if available < out.len() {
        out[available..].fill(0.0);
    }
}

/// WAV PCM embebido (alineado a 4 bytes) para el sonido personalizado al guardar un clip.
#[repr(C, align(4))]
struct AlignedWav<const N: usize>([u8; N]);

static CLIP_SAVED_WAV: AlignedWav<{ include_bytes!("../../assets/clip_saved.wav").len() }> =
    AlignedWav(*include_bytes!("../../assets/clip_saved.wav"));

/// Reproduce de forma asíncrona (no bloqueante) el sonido retro CRT al guardar un clip.
pub fn play_clip_saved_sound() {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    use windows::core::PCWSTR;

    unsafe {
        let _ = PlaySoundW(
            PCWSTR(CLIP_SAVED_WAV.0.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}
