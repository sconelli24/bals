// Ocultar ventana de consola en compilación --release para uso limpio en escritorio/bandeja.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod buffer;
mod capture;
mod clock;
mod config;
mod d3d;
mod encode;
mod engine;
mod game_detect;
mod hotkey;
mod ipc;
mod muxer;
mod perf;
mod startup;
mod tray;
mod trimmer;
mod ui;
mod updater;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

use crate::audio::AudioPipeline;
use crate::buffer::ReplayBuffer;
use crate::capture::{CaptureTarget, FrameCapture};
use crate::config::{
    AppConfig, AudioConfig, AudioSource, EncoderPreference, FrameRate, QualityPreset,
    ReplayDuration, Resolution, VideoConfig,
};
use crate::d3d::SharedD3DDevice;
use crate::encode::h264_mft::H264Encoder;
use crate::encode::video_processor::GpuNv12Converter;
use crate::engine::RecorderEngine;
use crate::muxer::{generate_clip_path, write_mp4_clip};

fn main() -> Result<()> {
    // Activar Per-Monitor DPI Awareness V2 desde el arranque: requerido para coordenadas físicas
    // reales de monitores (1440p/4K con escalado) y para DXGI Desktop Duplication en Windows 10/11.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    updater::cleanup_old_binary();

    match args.first().map(String::as_str) {
        // Proceso de interfaz gráfica bajo demanda: al cerrarse libera el 100% de RAM/GPU.
        Some("--ui") => return ui::run_gui(),
        // Arranque silencioso directo a la bandeja del sistema (ideal para Inicio de Windows).
        Some("--tray") => return run_daemon(false),
        // Prueba CLI automatizada: `bals --test [720p|1080p|2k] [30|60] [segundos] [--pid <PID>]`
        Some("--test") => return run_cli_test(&args[1..]),
        // Autoevaluación del rendimiento: `bals --soak <minutos> <carpeta> [escenario|-] [segundos entre clips]`.
        Some("--soak") => std::process::exit(perf::soak(&args[1..])),
        _ => {}
    }

    // Instancia única: si el motor ya está en la bandeja, abrir/enfocar la UI y pedir chequeo de update.
    if ipc::is_daemon_running() {
        ipc::spawn_or_focus_ui();
        ipc::notify_check_update();
        return Ok(());
    }

    // Primer arranque normal: inicia el motor en bandeja y abre la ventana de bienvenida/ajustes.
    run_daemon(true)
}

fn run_daemon(open_ui_on_start: bool) -> Result<()> {
    let cfg = AppConfig::load_or_create();
    let engine = RecorderEngine::spawn(cfg.clone())?;

    let (upd_tx, upd_rx) = crossbeam_channel::bounded::<()>(1);
    let pending_restart: Arc<Mutex<Option<(PathBuf, bool)>>> = Arc::new(Mutex::new(None));
    let pending_restart_bg = Arc::clone(&pending_restart);

    std::thread::Builder::new()
        .name("bals-auto-updater".to_string())
        .spawn(move || {
            let try_auto_update = |pending: &Arc<Mutex<Option<(PathBuf, bool)>>>| -> Result<bool, anyhow::Error> {
                if !AppConfig::load_or_create().auto_update {
                    return Ok(false);
                }
                match updater::check_latest_release()? {
                    Some(info) => {
                        info!(
                            version = %info.version,
                            "Nueva versión detectada; descargando actualización en segundo plano..."
                        );
                        let bytes = updater::download_update_bytes(&info)?;
                        let exe_path = updater::replace_current_exe(&bytes)?;
                        let reopen_ui = ipc::is_ui_open();
                        if let Ok(mut slot) = pending.lock() {
                            *slot = Some((exe_path, reopen_ui));
                        }
                        info!(
                            version = %info.version,
                            reopen_ui,
                            "Actualización preparada; cerrando daemon actual para reiniciar limpiamente"
                        );
                        ipc::close_ui_window();
                        ipc::notify_shutdown();
                        Ok(true)
                    }
                    None => Ok(false),
                }
            };

            // 1. Chequeo rápido al iniciar (con reintento a los 30s si el PC recién encendió sin red).
            std::thread::sleep(Duration::from_secs(2));
            let mut last_check = Instant::now();
            match try_auto_update(&pending_restart_bg) {
                Ok(true) => return,
                Ok(false) => {}
                Err(err) => {
                    tracing::debug!("Comprobación inicial de actualización falló ({err:#}); reintentando en 30s");
                    match upd_rx.recv_timeout(Duration::from_secs(30)) {
                        Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            last_check = Instant::now();
                            if matches!(try_auto_update(&pending_restart_bg), Ok(true)) {
                                return;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                    }
                }
            }

            // 2. Chequeos bajo demanda cada vez que el usuario abre Bals (cooldown de 30s).
            while upd_rx.recv().is_ok() {
                if last_check.elapsed() < Duration::from_secs(30) {
                    continue;
                }
                last_check = Instant::now();
                match try_auto_update(&pending_restart_bg) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(err) => {
                        warn!("No se pudo completar la actualización automática: {err:#}");
                    }
                }
            }
        })
        .ok();

    if open_ui_on_start {
        // Abrir la ventana UI tras un breve respiro para que la ventana de bandeja ya esté creada.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(250));
            ipc::spawn_or_focus_ui();
        });
    }

    let tray_res = tray::run_tray_loop(
        cfg,
        engine.cmd_tx.clone(),
        engine.event_rx.clone(),
        engine.status.clone(),
        upd_tx,
    );

    // Cerrar motor, GPU, audio y hotkey ANTES de lanzar el nuevo ejecutable (cero condiciones de carrera).
    engine.shutdown();

    if let Ok(mut slot) = pending_restart.lock() {
        if let Some((exe_path, reopen_ui)) = slot.take() {
            let launch_args: &[&str] = if reopen_ui { &[] } else { &["--tray"] };
            if let Err(err) = updater::spawn_updated_exe(&exe_path, launch_args) {
                warn!("Actualizado correctamente, pero falló el reinicio automático: {err:#}");
            }
        }
    }

    tray_res
}

fn run_cli_test(test_args: &[String]) -> Result<()> {
    let resolution = test_args
        .first()
        .and_then(|s| Resolution::from_str_opt(s))
        .unwrap_or(Resolution::P1080);
    let fps = test_args
        .get(1)
        .and_then(|s| FrameRate::from_str_opt(s))
        .unwrap_or(FrameRate::Fps60);
    let record_secs: u64 = test_args
        .get(2)
        .filter(|s| !s.starts_with("--"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let pid_opt = test_args
        .windows(2)
        .find(|w| w[0] == "--pid")
        .and_then(|w| w[1].parse::<u32>().ok());

    let video_config = VideoConfig::preset_full(
        resolution,
        fps,
        QualityPreset::Balanced,
        EncoderPreference::HardwareGpu,
    );
    let audio_config = AudioConfig {
        source: match pid_opt {
            Some(pid) => AudioSource::ProcessLoopback {
                pid,
                include_tree: true,
            },
            None => AudioSource::SystemLoopback,
        },
        ..AudioConfig::default()
    };

    let force_dxgi = test_args.iter().any(|s| s.eq_ignore_ascii_case("--dxgi"));

    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("RoInitialize falló")?;
    let res = run_av_clip_test(
        video_config,
        audio_config,
        ReplayDuration::Sec30,
        record_secs,
        force_dxgi,
    );
    unsafe { RoUninitialize() };
    res
}

fn run_av_clip_test(
    video_config: VideoConfig,
    audio_config: AudioConfig,
    replay_duration: ReplayDuration,
    record_secs: u64,
    prefer_dxgi: bool,
) -> Result<()> {
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }

    let d3d = SharedD3DDevice::new()?;
    let capture = FrameCapture::start(&d3d, CaptureTarget::PrimaryMonitor, true, prefer_dxgi)?;
    let (init_w, init_h) = capture.initial_size();

    let (out_w, out_h) = video_config.resolution.dimensions();
    let mut converter = GpuNv12Converter::new(
        &d3d,
        init_w,
        init_h,
        out_w,
        out_h,
        video_config.fps.as_u32(),
    )?;
    let mut encoder = H264Encoder::new(&d3d, video_config)?;

    let mut replay_buffer = ReplayBuffer::new(replay_duration.as_secs(), 128 * 1024 * 1024);

    let mut audio_pipeline = match AudioPipeline::start(audio_config) {
        Ok(pipe) => {
            replay_buffer.set_audio_info(pipe.stream_info.clone());
            Some(pipe)
        }
        Err(err) => {
            warn!("No se pudo iniciar el pipeline de audio ({err:#}); continuando solo con video");
            None
        }
    };

    let fps = video_config.fps.as_u32() as i64;
    let test_duration = Duration::from_secs(record_secs);
    let start = Instant::now();
    let mut qpc_origin = clock::now_100ns();
    let mut frame_idx = 0u64;
    let mut last_nv12 = None;

    while start.elapsed() < test_duration {
        if let Some(audio) = &audio_pipeline {
            while let Ok(apkt) = audio.rx.try_recv() {
                replay_buffer.push_audio(apkt);
            }
        }

        let target_rel = (frame_idx as i64 * 10_000_000) / fps;
        let target_qpc = qpc_origin + target_rel;
        loop {
            let rem = target_qpc - clock::now_100ns();
            if rem <= 0 {
                break;
            }
            if rem > 15_000 {
                std::thread::sleep(Duration::from_millis(((rem - 10_000) / 10_000).clamp(1, 8) as u64));
            } else {
                std::thread::yield_now();
            }
        }

        if let Some(frame) = capture.take_latest() {
            let nv12 = converter.convert(&frame.texture, frame.width, frame.height)?;
            last_nv12 = Some(nv12);
        }

        if let Some(nv12) = last_nv12.as_ref() {
            let pkts = encoder.encode_nv12_texture(nv12, target_rel, qpc_origin)?;
            for pkt in pkts {
                replay_buffer.push_video(pkt);
            }
            frame_idx += 1;
        } else {
            qpc_origin = clock::now_100ns();
        }
    }

    unsafe {
        windows::Win32::Media::timeEndPeriod(1);
    }

    if let Some(mut audio) = audio_pipeline.take() {
        audio.stop();
        while let Ok(apkt) = audio.rx.try_recv() {
            replay_buffer.push_audio(apkt);
        }
    }

    for pkt in encoder.flush(qpc_origin)? {
        replay_buffer.push_video(pkt);
    }

    let (received, replaced) = capture.stats_counts();
    info!(
        backend = capture.backend_label(),
        received,
        replaced,
        encoded_cfr_frames = frame_idx,
        "Prueba CLI CFR finalizada"
    );

    let snapshot = replay_buffer
        .snapshot(Some(replay_duration.as_secs()))
        .context("El buffer circular está vacío")?;

    let clips_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("clips");
    let clip_path = generate_clip_path(&clips_dir, None);

    let stats = write_mp4_clip(&clip_path, &snapshot, video_config)?;
    info!(
        file = %stats.path.display(),
        size_kb = stats.file_bytes / 1024,
        video_packets = stats.video_packets_written,
        duration_s = format!("{:.2}", stats.clip_duration_secs),
        "Clip CLI CFR generado"
    );

    Ok(())
}
