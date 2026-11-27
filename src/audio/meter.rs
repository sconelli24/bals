use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tracing::debug;
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

/// Medidor de volumen (VU Meter) en tiempo real para la ventana de Ajustes.
///
/// Captura silenciosamente los picos de audio del escritorio (loopback) y del micrófono
/// usando WASAPI y expone sus niveles (`0.0..=1.0`) de forma *lock-free* para la UI.
/// Al cerrar la ventana (`Drop`), ambos hilos terminan de inmediato (0% CPU/RAM en juego).
pub struct AudioLevelMonitor {
    desktop_peak: Arc<AtomicU32>,
    mic_peak: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    mic_enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    desktop_handle: Option<JoinHandle<()>>,
    mic_handle: Option<JoinHandle<()>>,
}

impl AudioLevelMonitor {
    pub fn start(mic_enabled_init: bool) -> Self {
        let desktop_peak = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let mic_peak = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let paused = Arc::new(AtomicBool::new(false));
        let mic_enabled = Arc::new(AtomicBool::new(mic_enabled_init));
        let stop = Arc::new(AtomicBool::new(false));

        let d_peak = Arc::clone(&desktop_peak);
        let d_paused = Arc::clone(&paused);
        let d_mic = Arc::clone(&mic_enabled);
        let d_stop = Arc::clone(&stop);
        let desktop_handle = thread::Builder::new()
            .name("bals-vu-desktop".to_string())
            .spawn(move || monitor_loop(Direction::Render, d_peak, d_paused, d_mic, d_stop))
            .ok();

        let m_peak = Arc::clone(&mic_peak);
        let m_paused = Arc::clone(&paused);
        let m_mic = Arc::clone(&mic_enabled);
        let m_stop = Arc::clone(&stop);
        let mic_handle = thread::Builder::new()
            .name("bals-vu-mic".to_string())
            .spawn(move || monitor_loop(Direction::Capture, m_peak, m_paused, m_mic, m_stop))
            .ok();

        Self {
            desktop_peak,
            mic_peak,
            paused,
            mic_enabled,
            stop,
            desktop_handle,
            mic_handle,
        }
    }

    /// Reduce cadencia de lectura cuando la ventana no está enfocada o no requiere refresco fino.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// Evita procesar muestras de micrófono cuando el usuario lo tiene desactivado.
    pub fn set_mic_enabled(&self, enabled: bool) {
        self.mic_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Nivel instantáneo suavizado del audio de escritorio/juego (`0.0..=1.0`).
    pub fn desktop_level(&self) -> f32 {
        f32::from_bits(self.desktop_peak.load(Ordering::Relaxed))
    }

    /// Nivel instantáneo suavizado del micrófono (`0.0..=1.0`).
    pub fn mic_level(&self) -> f32 {
        f32::from_bits(self.mic_peak.load(Ordering::Relaxed))
    }
}

impl Drop for AudioLevelMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.desktop_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.mic_handle.take() {
            let _ = h.join();
        }
    }
}

fn monitor_loop(
    device_dir: Direction,
    level_out: Arc<AtomicU32>,
    paused: Arc<AtomicBool>,
    mic_enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let _ = wasapi::initialize_mta();

    let Ok(enumerator) = DeviceEnumerator::new() else {
        return;
    };
    let Ok(device) = enumerator.get_default_device(device_dir) else {
        return;
    };
    let Ok(mut client) = device.get_iaudioclient() else {
        return;
    };

    let format = WaveFormat::new(32, 32, SampleType::Float, 48_000, 2, None);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 200_000,
    };
    // En Render + Direction::Capture activa loopback del sistema; en Capture abre el micrófono.
    if client
        .initialize_client(&format, Direction::Capture, mode, None)
        .is_err()
    {
        return;
    }

    let Ok(h_event) = client.set_get_eventhandle() else {
        return;
    };
    let Ok(capture) = client.get_audiocaptureclient() else {
        return;
    };
    if client.start_stream().is_err() {
        return;
    }

    let mut raw: VecDeque<u8> = VecDeque::with_capacity(8192);
    let mut smoothed = 0.0f32;
    let mut was_sleeping = false;

    while !stop.load(Ordering::Relaxed) {
        let should_sleep = paused.load(Ordering::Relaxed)
            || (device_dir == Direction::Capture && !mic_enabled.load(Ordering::Relaxed));
        if should_sleep {
            if smoothed != 0.0 {
                smoothed = 0.0;
                level_out.store(0.0f32.to_bits(), Ordering::Relaxed);
            }
            was_sleeping = true;
            thread::sleep(Duration::from_millis(120));
            continue;
        }

        if was_sleeping {
            was_sleeping = false;
            // Descartar backlog acumulado durante la pausa antes de volver a medir.
            while matches!(capture.get_next_packet_size(), Ok(Some(n)) if n > 0) {
                if capture.read_from_device_to_deque(&mut raw).is_err() {
                    break;
                }
                raw.clear();
            }
        }

        let _ = h_event.wait_for_event(30);
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let mut chunk_peak = 0.0f32;
        loop {
            let frames = match capture.get_next_packet_size() {
                Ok(Some(n)) => n,
                _ => 0,
            };
            if frames == 0 {
                break;
            }
            if capture.read_from_device_to_deque(&mut raw).is_err() {
                break;
            }
            let complete_bytes = raw.len() & !3;
            if complete_bytes > 0 {
                let slice = raw.make_contiguous();
                for c in slice[..complete_bytes].chunks_exact(4) {
                    let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs();
                    if v > chunk_peak {
                        chunk_peak = v;
                    }
                }
                raw.drain(..complete_bytes);
            }
        }

        // Ataque instantáneo y caída suave tipo VU meter profesional.
        if chunk_peak > smoothed {
            smoothed = chunk_peak;
        } else {
            smoothed *= 0.78;
            if smoothed < 0.002 {
                smoothed = 0.0;
            }
        }
        level_out.store(smoothed.clamp(0.0, 1.5).to_bits(), Ordering::Relaxed);
    }

    let _ = client.stop_stream();
    debug!(?device_dir, "Monitor VU detenido");
}
