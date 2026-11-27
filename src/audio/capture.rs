use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use crate::config::AudioSource;

pub const AUDIO_SAMPLE_RATE: usize = 48_000;
pub const AUDIO_CHANNELS: usize = 2;
/// Tope de muestras f32 intercaladas en cola por fuente (~250 ms) para evitar latencia acumulada.
const MAX_QUEUED_SAMPLES: usize = AUDIO_SAMPLE_RATE * AUDIO_CHANNELS / 4;

/// Tipo interno de captura WASAPI.
#[derive(Debug, Clone, Copy)]
pub enum CaptureKind {
    GameOrSystem(AudioSource),
    Microphone,
}

/// Hilo de captura WASAPI liviano que vuelca muestras `f32` estéreo a 48 kHz
/// en una cola compartida para el mezclador.
pub struct WasapiStream {
    pub samples: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl WasapiStream {
    pub fn start(kind: CaptureKind) -> Result<Self> {
        let samples = Arc::new(Mutex::new(VecDeque::with_capacity(
            AUDIO_SAMPLE_RATE * AUDIO_CHANNELS / 10,
        )));
        let stop = Arc::new(AtomicBool::new(false));

        // Canal de arranque para saber de inmediato si el dispositivo pudo abrirse.
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<String>>(1);

        let samples_clone = Arc::clone(&samples);
        let stop_clone = Arc::clone(&stop);
        let thread_name = match kind {
            CaptureKind::GameOrSystem(_) => "bals-audio-game",
            CaptureKind::Microphone => "bals-audio-mic",
        };

        let handle = thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                capture_thread(kind, samples_clone, stop_clone, ready_tx);
            })
            .context("No se pudo crear el hilo de captura WASAPI")?;

        let label = match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(res) => res?,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::SeqCst);
                anyhow::bail!("Tiempo de espera agotado al inicializar captura WASAPI");
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("El hilo WASAPI terminó antes de inicializar");
            }
        };
        info!(source = %label, "Captura de audio WASAPI iniciada (48 kHz estéreo)");

        Ok(Self {
            samples,
            stop,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_thread(
    kind: CaptureKind,
    samples_out: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
    ready_tx: crossbeam_channel::Sender<Result<String>>,
) {
    // Inicializar COM MTA en este hilo de audio.
    let _ = wasapi::initialize_mta();

    let init_res = init_audio_client(kind);
    let (audio_client, label) = match init_res {
        Ok(pair) => {
            let _ = ready_tx.send(Ok(pair.1.clone()));
            pair
        }
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    let h_event = match audio_client.set_get_eventhandle() {
        Ok(ev) => ev,
        Err(err) => {
            warn!(source = %label, "No se pudo crear event handle WASAPI: {err}");
            return;
        }
    };

    let capture_client = match audio_client.get_audiocaptureclient() {
        Ok(c) => c,
        Err(err) => {
            warn!(source = %label, "No se pudo obtener IAudioCaptureClient: {err}");
            return;
        }
    };

    if let Err(err) = audio_client.start_stream() {
        warn!(source = %label, "start_stream falló: {err}");
        return;
    }

    let mut raw_bytes: VecDeque<u8> = VecDeque::with_capacity(16 * 1024);
    let mut scratch_samples: Vec<f32> = Vec::with_capacity(4096);

    while !stop.load(Ordering::Relaxed) {
        // Espera corta (25 ms) para reaccionar rápido al cierre incluso en silencio total.
        let _ = h_event.wait_for_event(25);

        if stop.load(Ordering::Relaxed) {
            break;
        }

        loop {
            let next_frames = match capture_client.get_next_packet_size() {
                Ok(Some(n)) => n,
                Ok(None) => 0,
                Err(_) => break,
            };
            if next_frames == 0 {
                break;
            }

            if let Err(err) = capture_client.read_from_device_to_deque(&mut raw_bytes) {
                debug!(source = %label, "read_from_device_to_deque error: {err}");
                break;
            }
        }

        let complete_bytes = raw_bytes.len() & !3;
        if complete_bytes > 0 {
            // Convertir muestras fuera del mutex haciendo contiguo el VecDeque en una operación O(1) amortizada.
            let slice = raw_bytes.make_contiguous();
            scratch_samples.clear();
            scratch_samples.extend(
                slice[..complete_bytes]
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
            );
            raw_bytes.drain(..complete_bytes);

            let mut guard = samples_out.lock().unwrap();
            guard.extend(&scratch_samples);
            // Limitar la cola para evitar acumulación de latencia si hay picos de carga.
            if guard.len() > MAX_QUEUED_SAMPLES {
                let excess = guard.len() - MAX_QUEUED_SAMPLES;
                // Alinear a pares estéreo (2 muestras f32 por frame).
                let drop_count = excess & !1;
                guard.drain(..drop_count);
            }
        }
    }

    let _ = audio_client.stop_stream();
    debug!(source = %label, "Hilo de captura WASAPI detenido");
}

fn init_audio_client(kind: CaptureKind) -> Result<(AudioClient, String)> {
    let desired_format = WaveFormat::new(
        32,
        32,
        SampleType::Float,
        AUDIO_SAMPLE_RATE,
        AUDIO_CHANNELS,
        None,
    );
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000, // 20 ms
    };

    match kind {
        CaptureKind::GameOrSystem(AudioSource::ProcessLoopback { pid, include_tree }) => {
            let mut client = AudioClient::new_application_loopback_client(pid, include_tree)
                .map_err(|e| anyhow::anyhow!("new_application_loopback_client(pid={pid}) falló: {e}"))?;
            client
                .initialize_client(&desired_format, Direction::Capture, mode, None)
                .map_err(|e| anyhow::anyhow!("initialize_client (ProcessLoopback) falló: {e}"))?;
            Ok((client, format!("ProcessLoopback(pid={pid})")))
        }
        CaptureKind::GameOrSystem(AudioSource::SystemLoopback) => {
            let enumerator = DeviceEnumerator::new()
                .map_err(|e| anyhow::anyhow!("DeviceEnumerator::new falló: {e}"))?;
            let device = enumerator
                .get_default_device(Direction::Render)
                .map_err(|e| anyhow::anyhow!("No se encontró dispositivo de salida por defecto: {e}"))?;
            let dev_name = device
                .get_friendlyname()
                .unwrap_or_else(|_| "Salida por defecto".to_string());
            let mut client = device
                .get_iaudioclient()
                .map_err(|e| anyhow::anyhow!("get_iaudioclient (Render) falló: {e}"))?;
            // Abrir un dispositivo Render en modo Capture activa automáticamente AUDCLNT_STREAMFLAGS_LOOPBACK.
            client
                .initialize_client(&desired_format, Direction::Capture, mode, None)
                .map_err(|e| anyhow::anyhow!("initialize_client (SystemLoopback) falló: {e}"))?;
            Ok((client, format!("SystemLoopback ({dev_name})")))
        }
        CaptureKind::Microphone => {
            let enumerator = DeviceEnumerator::new()
                .map_err(|e| anyhow::anyhow!("DeviceEnumerator::new falló: {e}"))?;
            let device = enumerator
                .get_default_device(Direction::Capture)
                .map_err(|e| anyhow::anyhow!("No se encontró micrófono por defecto: {e}"))?;
            let dev_name = device
                .get_friendlyname()
                .unwrap_or_else(|_| "Micrófono".to_string());
            let mut client = device
                .get_iaudioclient()
                .map_err(|e| anyhow::anyhow!("get_iaudioclient (Mic) falló: {e}"))?;
            client
                .initialize_client(&desired_format, Direction::Capture, mode, None)
                .map_err(|e| anyhow::anyhow!("initialize_client (Mic) falló: {e}"))?;
            Ok((client, format!("Microphone ({dev_name})")))
        }
    }
}
