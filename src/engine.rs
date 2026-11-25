use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, GetCurrentThread, SetThreadPriority, SetWaitableTimer,
    THREAD_PRIORITY_ABOVE_NORMAL, WaitForSingleObject,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic, IsWindowVisible};
use windows::core::PCWSTR;

use crate::audio::AudioPipeline;
use crate::buffer::ReplayBuffer;
use crate::capture::{
    CaptureTarget, FrameCapture, is_windows_11_or_later, resolve_monitor_target,
};
use crate::clock;
use crate::config::{AppConfig, AudioCaptureMode, AudioSource, CaptureMode, VideoConfig};
use crate::d3d::SharedD3DDevice;
use crate::encode::h264_mft::H264Encoder;
use crate::encode::video_processor::GpuNv12Converter;
use crate::game_detect::{DetectedGame, detect_foreground_game, is_process_alive};
use crate::ipc::{TelemetryState, TelemetryWriter};
use crate::muxer::{WriteStats, generate_clip_path, write_mp4_clip};

const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION_FLAG: u32 = 0x0000_0002;
const TIMER_ACCESS_SYNC_MODIFY: u32 = 0x0010_0002; // SYNCHRONIZE | TIMER_MODIFY_STATE

/// Comandos que la bandeja del sistema, hotkeys o GUI envían al motor de grabación.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum EngineCommand {
    StartManual,
    Stop,
    ToggleManual,
    SaveClip,
    /// Actualiza la configuración en vivo. `persist_to_disk` es `false` cuando `--ui` ya la guardó.
    UpdateConfig {
        config: AppConfig,
        persist_to_disk: bool,
    },
    Shutdown,
}

/// Eventos que el motor emite hacia la bandeja del sistema / GUI (para notificaciones y UI).
#[derive(Debug, Clone)]
pub enum EngineEvent {
    ClipSaved(WriteStats),
    ClipFailed(String),
    StatusChanged(EngineStatus),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingState {
    Idle,
    RecordingManual {
        monitor_label: String,
    },
    RecordingGame {
        exe_name: String,
        game_name: String,
        pid: u32,
        paused_alt_tab: bool,
    },
}

#[derive(Debug, Clone)]
pub struct EngineStatus {
    pub state: RecordingState,
    pub buffered_secs: f64,
    pub buffer_ram_mb: f64,
    pub last_clip: Option<PathBuf>,
}

impl Default for EngineStatus {
    fn default() -> Self {
        Self {
            state: RecordingState::Idle,
            buffered_secs: 0.0,
            buffer_ram_mb: 0.0,
            last_clip: None,
        }
    }
}

/// Activa resolución de temporizador de 1 ms en Windows solo mientras se graba.
struct HighResTimerGuard;

impl HighResTimerGuard {
    fn new() -> Self {
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for HighResTimerGuard {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

/// Temporizador de espera de alta resolución de Windows 10/11 (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`).
/// Permite dormir con precisión de ~0.5 ms a 0% CPU en lugar de hacer spin-yield durante 1.5 ms/frame.
struct HighResWaitTimer {
    handle: Option<HANDLE>,
}

impl HighResWaitTimer {
    fn new() -> Self {
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION_FLAG,
                TIMER_ACCESS_SYNC_MODIFY,
            )
            .or_else(|_| {
                CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ACCESS_SYNC_MODIFY)
            })
            .ok()
        };
        Self { handle }
    }

    /// Duerme dejando un margen mínimo (~0.25 ms) para el ajuste final de exactitud CFR.
    fn sleep_until_near(&self, remaining_100ns: i64) {
        if let Some(handle) = self.handle {
            // Dejar 0.25 ms (2500 ticks de 100 ns) para el yield fino final.
            let wait_100ns = (remaining_100ns - 2_500).clamp(1_000, 100_000);
            let due_time = -wait_100ns;
            if unsafe { SetWaitableTimer(handle, &due_time, 0, None, None, false) }.is_ok() {
                unsafe {
                    WaitForSingleObject(handle, 30);
                }
                return;
            }
        }
        // Fallback clásico si no hay waitable timer disponible.
        if remaining_100ns > 15_000 {
            let sleep_ms = ((remaining_100ns - 10_000) / 10_000).clamp(1, 8) as u64;
            thread::sleep(Duration::from_millis(sleep_ms));
        } else {
            thread::yield_now();
        }
    }
}

impl Drop for HighResWaitTimer {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Sesión activa de grabación en Replay Buffer a tasa de cuadros constante (CFR).
///
/// Posee su propio `SharedD3DDevice` creado bajo demanda; al cerrarse se liberan D3D11/VRAM,
/// audio y el timer de 1 ms (verdadero 0% GPU y 0 MB VRAM en escritorio).
struct ActiveSession {
    d3d: SharedD3DDevice,
    capture: Option<FrameCapture>,
    capture_target: CaptureTarget,
    prefer_dxgi: bool,
    capture_backend: &'static str,
    capture_cursor: bool,
    /// Ventana del juego detectado para pausar en Alt-Tab (incluso si capturamos su monitor con DXGI).
    target_hwnd: Option<HWND>,
    /// Momento desde el cual la ventana del juego perdió el foco (para debounce de Alt-Tab en modo monitor).
    lost_focus_at: Option<Instant>,
    converter: GpuNv12Converter,
    encoder: H264Encoder,
    encoder_label: String,
    audio: Option<AudioPipeline>,
    replay_buffer: ReplayBuffer,
    video_config: VideoConfig,
    /// Reloj QPC de inicio de la sesión para alinear video CFR (0, dt, 2*dt...) con el audio.
    qpc_origin_100ns: i64,
    /// Tiempo acumulado en pausa por Alt-Tab (en ticks de 100 ns) para mantener sync A/V exacto.
    total_paused_100ns: i64,
    /// Timestamp QPC en el que se entró en pausa por Alt-Tab (si actualmente está pausado).
    paused_since_100ns: Option<i64>,
    /// Último momento en que WGC/DXGI entregó un frame real (para detectar juegos congelados en Alt-Tab).
    last_wgc_frame_at: Instant,
    /// Índice del próximo frame CFR a emitir.
    frame_index: u64,
    /// Última textura NV12 ya convertida en VRAM (se reutiliza sin repetir `VideoProcessorBlt`
    /// si el juego/escritorio no presentó un frame nuevo en el tick actual).
    last_nv12: Option<ID3D11Texture2D>,
    state: RecordingState,
    wait_timer: HighResWaitTimer,
    _timer_guard: HighResTimerGuard,
}

impl ActiveSession {
    fn start(
        cfg: &AppConfig,
        target: CaptureTarget,
        alt_tab_hwnd: Option<HWND>,
        prefer_dxgi: bool,
        audio_source: AudioSource,
        state: RecordingState,
    ) -> Result<Self> {
        let timer_guard = HighResTimerGuard::new();
        let d3d = SharedD3DDevice::new()?;
        let video_config = cfg.video_config();
        let capture = FrameCapture::start(&d3d, target, cfg.capture_cursor, prefer_dxgi)?;
        let (init_w, init_h) = capture.initial_size();
        let capture_backend = capture.backend_label();
        let (out_w, out_h) = video_config.resolution.dimensions();

        let converter = GpuNv12Converter::new(
            &d3d,
            init_w,
            init_h,
            out_w,
            out_h,
            video_config.fps.as_u32(),
        )?;
        let encoder = H264Encoder::new(&d3d, video_config)?;
        let encoder_label = encoder.hardware_label();

        let max_bytes = cfg.max_buffer_ram_mb.max(32) * 1024 * 1024;
        let mut replay_buffer = ReplayBuffer::new(cfg.replay_duration.as_secs(), max_bytes);

        let audio = match AudioPipeline::start(cfg.audio_config(audio_source)) {
            Ok(pipe) => {
                replay_buffer.set_audio_info(pipe.stream_info.clone());
                Some(pipe)
            }
            Err(err) => {
                if matches!(audio_source, AudioSource::ProcessLoopback { .. }) {
                    warn!("ProcessLoopback falló ({err:#}); reintentando con SystemLoopback");
                    match AudioPipeline::start(cfg.audio_config(AudioSource::SystemLoopback)) {
                        Ok(pipe) => {
                            replay_buffer.set_audio_info(pipe.stream_info.clone());
                            Some(pipe)
                        }
                        Err(err2) => {
                            warn!("Audio no disponible ({err2:#}); grabando solo video");
                            None
                        }
                    }
                } else {
                    warn!("Audio no disponible ({err:#}); grabando solo video");
                    None
                }
            }
        };

        let qpc_origin_100ns = clock::now_100ns();
        let target_hwnd = alt_tab_hwnd.or(match target {
            CaptureTarget::Window(hwnd) => Some(hwnd),
            _ => None,
        });

        Ok(Self {
            d3d,
            capture: Some(capture),
            capture_target: target,
            prefer_dxgi,
            capture_backend,
            capture_cursor: cfg.capture_cursor,
            target_hwnd,
            lost_focus_at: None,
            converter,
            encoder,
            encoder_label,
            audio,
            replay_buffer,
            video_config,
            qpc_origin_100ns,
            total_paused_100ns: 0,
            paused_since_100ns: None,
            last_wgc_frame_at: Instant::now(),
            frame_index: 0,
            last_nv12: None,
            state,
            wait_timer: HighResWaitTimer::new(),
            _timer_guard: timer_guard,
        })
    }

    fn is_paused(&self) -> bool {
        self.paused_since_100ns.is_some()
    }

    /// Evalúa si el juego está minimizado o en Alt-Tab sin frames activos.
    /// Devuelve `true` si cambió el estado de pausa (para refrescar la bandeja).
    fn update_alt_tab_pause(&mut self) -> bool {
        let Some(hwnd) = self.target_hwnd else {
            return false;
        };

        let is_minimized = unsafe { IsIconic(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() };
        let is_foreground = unsafe { GetForegroundWindow() == hwnd };

        if is_foreground {
            self.lost_focus_at = None;
        } else if self.lost_focus_at.is_none() {
            self.lost_focus_at = Some(Instant::now());
        }

        // Si capturamos el monitor entero para un juego en Win10 (DXGI), el monitor sigue enviando
        // frames del escritorio durante Alt-Tab; pausamos tras 400 ms sin foco para no pisar el clip.
        let is_monitor_for_game = !matches!(self.capture_target, CaptureTarget::Window(_));
        let should_pause = if is_monitor_for_game {
            is_minimized
                || self
                    .lost_focus_at
                    .is_some_and(|t| t.elapsed() > Duration::from_millis(400))
        } else {
            is_minimized
                || (!is_foreground && self.last_wgc_frame_at.elapsed() > Duration::from_millis(500))
        };

        if should_pause && self.paused_since_100ns.is_none() {
            self.paused_since_100ns = Some(clock::now_100ns());
            if let RecordingState::RecordingGame { paused_alt_tab, .. } = &mut self.state {
                *paused_alt_tab = true;
            }
            debug!(
                buffered_secs = format!("{:.1}", self.replay_buffer.buffered_secs()),
                ram_mb = format!("{:.1}", self.replay_buffer.total_bytes() as f64 / (1024.0 * 1024.0)),
                "Juego en Alt-Tab/minimizado; ReplayBuffer congelado en RAM para preservar los últimos segundos"
            );
            return true;
        }

        if !should_pause && self.paused_since_100ns.is_some() {
            // Si la captura se había soltado durante el Alt-Tab (ej. fullscreen exclusivo), re-engancharla
            // manteniendo intacto todo el ReplayBuffer.
            if self.capture.is_none() {
                if let Ok(new_cap) = FrameCapture::start(
                    &self.d3d,
                    self.capture_target,
                    self.capture_cursor,
                    self.prefer_dxgi,
                ) {
                    self.capture_backend = new_cap.backend_label();
                    self.capture = Some(new_cap);
                } else {
                    return false;
                }
            }

            if let Some(since) = self.paused_since_100ns.take() {
                self.total_paused_100ns += (clock::now_100ns() - since).max(0);
            }
            if let RecordingState::RecordingGame { paused_alt_tab, .. } = &mut self.state {
                *paused_alt_tab = false;
            }
            self.encoder.force_keyframe();
            self.last_wgc_frame_at = Instant::now();
            debug!("Juego de vuelta en primer plano; reanudando ReplayBuffer sin pérdida");
            return true;
        }

        false
    }

    fn pump_audio(&mut self) {
        if let Some(audio) = &self.audio {
            while let Ok(mut apkt) = audio.rx.try_recv() {
                if self.is_paused() {
                    continue;
                }
                apkt.pts_100ns = apkt.pts_100ns.saturating_sub(self.total_paused_100ns);
                self.replay_buffer.push_audio(apkt);
            }
        }
    }

    /// Espera con precisión de sub-milisegundo sin busy-spin hasta el siguiente tick CFR
    /// y codifica el frame más reciente de la GPU (o reutiliza el último NV12 si no cambió).
    fn step_cfr_tick(&mut self) -> Result<bool> {
        let pause_changed = self.update_alt_tab_pause();

        if self.is_paused() {
            if let Some(cap) = &self.capture {
                if let Some(fresh) = cap.take_latest() {
                    self.last_wgc_frame_at = Instant::now();
                    if let Ok(nv12) = self.converter.convert(&fresh.texture, fresh.width, fresh.height) {
                        self.last_nv12 = Some(nv12);
                    }
                    return Ok(self.update_alt_tab_pause() || pause_changed);
                }
            }
            thread::sleep(Duration::from_millis(50));
            return Ok(pause_changed);
        }

        let fps = self.video_config.fps.as_u32() as i64;
        let target_rel_100ns = (self.frame_index as i64 * 10_000_000) / fps;
        let target_wall_qpc = self.qpc_origin_100ns + self.total_paused_100ns + target_rel_100ns;

        // Espera con HighResWaitTimer (0% CPU) hasta faltando ~0.25 ms, luego yield fino.
        loop {
            let now = clock::now_100ns();
            let remaining = target_wall_qpc - now;
            if remaining <= 0 {
                if remaining < -1_200_000 {
                    let elapsed = (now - self.qpc_origin_100ns - self.total_paused_100ns).max(0);
                    let resync = ((elapsed * fps) / 10_000_000) as u64;
                    crate::perf::skipped(resync.saturating_sub(self.frame_index));
                    self.frame_index = resync;
                }
                break;
            }
            if remaining > 4_000 {
                self.wait_timer.sleep_until_near(remaining);
            } else {
                thread::yield_now();
            }
        }

        let work_started = Instant::now();

        // 1. Tomar el frame más reciente del buzón WGC (si llegó uno nuevo desde el tick anterior).
        if let Some(cap) = &self.capture {
            if let Some(fresh) = cap.take_latest() {
                self.last_wgc_frame_at = Instant::now();
                match self.converter.convert(&fresh.texture, fresh.width, fresh.height) {
                    Ok(nv12) => self.last_nv12 = Some(nv12),
                    Err(err) => {
                        warn!("Conversión transitoria falló durante transición de ventana ({err:#}); preservando ReplayBuffer");
                        self.capture = None;
                        self.paused_since_100ns = Some(clock::now_100ns());
                        return Ok(true);
                    }
                }
            }
        }

        // 2. Codificar exactamente 1 frame CFR en este tick (si ya recibimos al menos el primer frame).
        if let Some(nv12) = self.last_nv12.as_ref() {
            let rel_pts = (self.frame_index as i64 * 10_000_000) / fps;
            let pkts = self
                .encoder
                .encode_nv12_texture(nv12, rel_pts, self.qpc_origin_100ns)?;
            for pkt in pkts {
                self.replay_buffer.push_video(pkt);
            }
            self.frame_index += 1;
        } else {
            self.qpc_origin_100ns = clock::now_100ns();
        }

        crate::perf::tick(work_started.elapsed(), self.last_nv12.is_some());
        Ok(pause_changed)
    }
}

/// Controlador principal del motor en segundo plano de Bals.
pub struct RecorderEngine {
    pub cmd_tx: Sender<EngineCommand>,
    pub event_rx: Receiver<EngineEvent>,
    pub status: Arc<Mutex<EngineStatus>>,
    handle: Option<JoinHandle<()>>,
}

impl RecorderEngine {
    pub fn spawn(initial_config: AppConfig) -> Result<Self> {
        let (cmd_tx, cmd_rx) = unbounded::<EngineCommand>();
        let (event_tx, event_rx) = unbounded::<EngineEvent>();
        let status = Arc::new(Mutex::new(EngineStatus::default()));
        let status_clone = Arc::clone(&status);

        let handle = thread::Builder::new()
            .name("bals-engine".to_string())
            .spawn(move || {
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                }
                if let Err(err) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
                    error!("RoInitialize falló en bals-engine: {err}");
                    return;
                }
                engine_loop(initial_config, cmd_rx, event_tx, status_clone);
                unsafe { RoUninitialize() };
            })
            .context("No se pudo iniciar el hilo bals-engine")?;

        Ok(Self {
            cmd_tx,
            event_rx,
            status,
            handle: Some(handle),
        })
    }

    pub fn shutdown(mut self) {
        let _ = self.cmd_tx.send(EngineCommand::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn engine_loop(
    mut cfg: AppConfig,
    cmd_rx: Receiver<EngineCommand>,
    event_tx: Sender<EngineEvent>,
    shared_status: Arc<Mutex<EngineStatus>>,
) {
    let telemetry = Arc::new(Mutex::new(TelemetryWriter::create()));
    let mut session: Option<ActiveSession> = None;
    let mut last_game_poll = Instant::now() - Duration::from_secs(10);
    let mut last_status_publish = Instant::now();

    // Si el usuario eligió grabar un monitor completo, iniciar la captura automáticamente al arrancar.
    if cfg.capture_mode == CaptureMode::ManualMonitor {
        start_manual_session(&cfg, &mut session);
        publish_status(&session, &cfg, &shared_status, &event_tx, &telemetry);
    } else {
        publish_telemetry(&session, &cfg, &telemetry);
    }

    info!(
        mode = ?cfg.capture_mode,
        resolution = %cfg.resolution,
        fps = %cfg.fps,
        replay = %cfg.replay_duration,
        hotkey = %cfg.save_clip_hotkey,
        "Motor CFR de Bals listo en segundo plano (D3D11 lazy: 0% GPU / 0 MB VRAM en espera)"
    );

    loop {
        // 1. Procesar todos los comandos pendientes sin bloquear.
        let mut should_exit = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                EngineCommand::Shutdown => {
                    should_exit = true;
                    break;
                }
                EngineCommand::StartManual => {
                    start_manual_session(&cfg, &mut session);
                    publish_status(&session, &cfg, &shared_status, &event_tx, &telemetry);
                }
                EngineCommand::Stop => {
                    if session.take().is_some() {
                        info!("Grabación detenida (GPU y VRAM liberadas al 100%)");
                        publish_status(&session, &cfg, &shared_status, &event_tx, &telemetry);
                    }
                }
                EngineCommand::ToggleManual => {
                    if session.is_some() {
                        session = None;
                        info!("Grabación pausada manualmente (GPU liberada)");
                    } else {
                        start_manual_session(&cfg, &mut session);
                    }
                    publish_status(&session, &cfg, &shared_status, &event_tx, &telemetry);
                }
                EngineCommand::SaveClip => {
                    handle_save_clip(&mut session, &cfg, &shared_status, &event_tx, &telemetry);
                }
                EngineCommand::UpdateConfig {
                    config: new_cfg,
                    persist_to_disk,
                } => {
                    apply_config_update(
                        &mut cfg,
                        new_cfg,
                        persist_to_disk,
                        &mut session,
                        &mut last_game_poll,
                        &shared_status,
                        &event_tx,
                        &telemetry,
                    );
                }
            }
        }

        if should_exit {
            break;
        }

        // 2. Detección automática de juegos cada 2 segundos (si está activa).
        if cfg.capture_mode == CaptureMode::AutoDetectGames
            && last_game_poll.elapsed() >= Duration::from_secs(2)
        {
            last_game_poll = Instant::now();
            poll_game_autodetect(&cfg, &mut session, &shared_status, &event_tx, &telemetry);
        }

        // 3. Ejecutar el siguiente tick CFR exacto si hay sesión activa, o dormir si estamos en reposo.
        if let Some(active) = session.as_mut() {
            active.pump_audio();
            match active.step_cfr_tick() {
                Ok(true) => publish_status(&session, &cfg, &shared_status, &event_tx, &telemetry),
                Ok(false) => {}
                Err(err) => {
                    warn!("La sesión de captura se cerró ({err:#}); pasando a reposo");
                    session = None;
                    publish_status(&session, &cfg, &shared_status, &event_tx, &telemetry);
                }
            }
        } else if let Ok(cmd) = cmd_rx.recv_timeout(Duration::from_millis(200)) {
            match cmd {
                EngineCommand::Shutdown => break,
                EngineCommand::StartManual | EngineCommand::ToggleManual => {
                    start_manual_session(&cfg, &mut session);
                    publish_status(&session, &cfg, &shared_status, &event_tx, &telemetry);
                }
                EngineCommand::Stop => {}
                EngineCommand::SaveClip => {
                    let msg = "No hay grabación activa. Inicia un juego o activa el modo manual.";
                    record_and_publish_event(&session, &cfg, &telemetry, msg, true);
                    let _ = event_tx.send(EngineEvent::ClipFailed(msg.to_string()));
                }
                EngineCommand::UpdateConfig {
                    config: new_cfg,
                    persist_to_disk,
                } => {
                    apply_config_update(
                        &mut cfg,
                        new_cfg,
                        persist_to_disk,
                        &mut session,
                        &mut last_game_poll,
                        &shared_status,
                        &event_tx,
                        &telemetry,
                    );
                }
            }
        }

        // 4. Actualizar telemetría (segundos en buffer y RAM usada) cada ~500 ms.
        if last_status_publish.elapsed() >= Duration::from_millis(500) {
            last_status_publish = Instant::now();
            update_shared_metrics(&session, &shared_status);
            publish_telemetry(&session, &cfg, &telemetry);
        }
    }

    drop(session);
    info!("Motor de Bals apagado limpiamente");
}

#[allow(clippy::too_many_arguments)]
fn apply_config_update(
    cfg: &mut AppConfig,
    new_cfg: AppConfig,
    persist_to_disk: bool,
    session: &mut Option<ActiveSession>,
    last_game_poll: &mut Instant,
    shared_status: &Arc<Mutex<EngineStatus>>,
    event_tx: &Sender<EngineEvent>,
    telemetry: &Arc<Mutex<Option<TelemetryWriter>>>,
) {
    // Los volúmenes se actualizan en caliente vía AtomicU32 sin reiniciar ni perder el ReplayBuffer.
    let mode_changed = new_cfg.capture_mode != cfg.capture_mode;
    let monitor_changed = new_cfg.selected_monitor != cfg.selected_monitor;
    let needs_restart = new_cfg.resolution != cfg.resolution
        || new_cfg.fps != cfg.fps
        || new_cfg.quality != cfg.quality
        || new_cfg.encoder_pref != cfg.encoder_pref
        || new_cfg.replay_duration != cfg.replay_duration
        || new_cfg.audio_capture_mode != cfg.audio_capture_mode
        || new_cfg.mic_enabled != cfg.mic_enabled
        || new_cfg.capture_cursor != cfg.capture_cursor
        || (new_cfg.capture_mode == CaptureMode::ManualMonitor && monitor_changed);

    let volumes_changed = (new_cfg.game_volume - cfg.game_volume).abs() > 0.001
        || (new_cfg.mic_volume - cfg.mic_volume).abs() > 0.001;

    let prev_state = session.as_ref().map(|s| s.state.clone());
    *cfg = new_cfg;
    if persist_to_disk {
        if let Err(err) = cfg.save() {
            warn!("Error guardando config.toml: {err:#}");
        }
    }

    if mode_changed {
        *session = None;
        match cfg.capture_mode {
            CaptureMode::ManualMonitor => {
                start_manual_session(cfg, session);
            }
            CaptureMode::AutoDetectGames => {
                *last_game_poll = Instant::now() - Duration::from_secs(10);
            }
        }
        publish_status(session, cfg, shared_status, event_tx, telemetry);
    } else if needs_restart {
        if let Some(prev) = prev_state {
            *session = None;
            match prev {
                RecordingState::RecordingManual { .. } => {
                    start_manual_session(cfg, session);
                }
                RecordingState::RecordingGame { .. } => {
                    *last_game_poll = Instant::now() - Duration::from_secs(10);
                }
                RecordingState::Idle => {}
            }
            publish_status(session, cfg, shared_status, event_tx, telemetry);
        } else if cfg.capture_mode == CaptureMode::ManualMonitor && monitor_changed {
            start_manual_session(cfg, session);
            publish_status(session, cfg, shared_status, event_tx, telemetry);
        }
    } else if volumes_changed {
        if let Some(active) = session.as_ref() {
            if let Some(audio) = active.audio.as_ref() {
                audio.set_volumes(cfg.game_volume, cfg.mic_volume);
                info!(
                    game_vol = format!("{:.0}%", cfg.game_volume * 100.0),
                    mic_vol = format!("{:.0}%", cfg.mic_volume * 100.0),
                    "Volúmenes actualizados en vivo sin interrumpir el Replay Buffer"
                );
            }
        }
        publish_telemetry(session, cfg, telemetry);
    } else {
        publish_telemetry(session, cfg, telemetry);
    }
}

fn start_manual_session(cfg: &AppConfig, session: &mut Option<ActiveSession>) {
    *session = None;
    let (target, monitor_label) = resolve_monitor_target(cfg.selected_monitor.as_deref());
    let prefer_dxgi = !is_windows_11_or_later();
    let state = RecordingState::RecordingManual {
        monitor_label: monitor_label.clone(),
    };
    match ActiveSession::start(
        cfg,
        target,
        None,
        prefer_dxgi,
        AudioSource::SystemLoopback,
        state,
    ) {
        Ok(active) => {
            info!(
                monitor = %monitor_label,
                capture = active.capture_backend,
                resolution = %cfg.resolution,
                fps = %cfg.fps,
                replay = %cfg.replay_duration,
                encoder = %active.encoder_label,
                "Replay Buffer CFR iniciado sobre monitor seleccionado"
            );
            *session = Some(active);
        }
        Err(err) => {
            error!("No se pudo iniciar la grabación del monitor ({monitor_label}): {err:#}");
        }
    }
}

fn poll_game_autodetect(
    cfg: &AppConfig,
    session: &mut Option<ActiveSession>,
    shared_status: &Arc<Mutex<EngineStatus>>,
    event_tx: &Sender<EngineEvent>,
    telemetry: &Arc<Mutex<Option<TelemetryWriter>>>,
) {
    if let Some(active) = session.as_ref() {
        match &active.state {
            RecordingState::RecordingManual { .. } => return,
            RecordingState::RecordingGame { pid, exe_name, .. } => {
                if is_process_alive(*pid) {
                    return;
                }
                info!(game = %exe_name, pid, "El juego se cerró; liberando GPU/VRAM (0%)");
                *session = None;
                publish_status(session, cfg, shared_status, event_tx, telemetry);
            }
            RecordingState::Idle => {}
        }
    }

    if let Some(DetectedGame {
        hwnd,
        pid,
        exe_name,
        game_name,
        is_fullscreen,
        monitor,
    }) = detect_foreground_game(&cfg.extra_game_exes)
    {
        let use_win10_dxgi = !is_windows_11_or_later() && is_fullscreen;
        let primary_target = if use_win10_dxgi {
            CaptureTarget::Monitor(monitor)
        } else {
            CaptureTarget::Window(hwnd)
        };

        info!(
            game = %game_name,
            exe = %exe_name,
            pid,
            audio_mode = ?cfg.audio_capture_mode,
            win10_dxgi = use_win10_dxgi,
            "Juego detectado automáticamente; iniciando Replay Buffer CFR"
        );
        let audio_src = match cfg.audio_capture_mode {
            AudioCaptureMode::GameOnly => AudioSource::ProcessLoopback {
                pid,
                include_tree: true,
            },
            AudioCaptureMode::FullDesktop => AudioSource::SystemLoopback,
        };
        let state = RecordingState::RecordingGame {
            exe_name: exe_name.clone(),
            game_name: game_name.clone(),
            pid,
            paused_alt_tab: false,
        };
        match ActiveSession::start(
            cfg,
            primary_target,
            Some(hwnd),
            use_win10_dxgi,
            audio_src,
            state,
        ) {
            Ok(active) => {
                info!(
                    game = %game_name,
                    exe = %exe_name,
                    capture = active.capture_backend,
                    encoder = %active.encoder_label,
                    "Sesión de juego iniciada"
                );
                *session = Some(active);
                publish_status(session, cfg, shared_status, event_tx, telemetry);
            }
            Err(err) => {
                warn!(game = %game_name, "No se pudo enganchar captura principal ({err:#}); intentando monitor del juego");
                let fallback_state = RecordingState::RecordingGame {
                    exe_name,
                    game_name,
                    pid,
                    paused_alt_tab: false,
                };
                if let Ok(active) = ActiveSession::start(
                    cfg,
                    CaptureTarget::Monitor(monitor),
                    Some(hwnd),
                    false,
                    audio_src,
                    fallback_state,
                ) {
                    *session = Some(active);
                    publish_status(session, cfg, shared_status, event_tx, telemetry);
                }
            }
        }
    }
}

fn handle_save_clip(
    session: &mut Option<ActiveSession>,
    cfg: &AppConfig,
    shared_status: &Arc<Mutex<EngineStatus>>,
    event_tx: &Sender<EngineEvent>,
    telemetry: &Arc<Mutex<Option<TelemetryWriter>>>,
) {
    let held_since = Instant::now();
    let Some(active) = session.as_mut() else {
        let msg = "No hay grabación activa en este momento.";
        record_and_publish_event(session, cfg, telemetry, msg, true);
        let _ = event_tx.send(EngineEvent::ClipFailed(msg.to_string()));
        return;
    };

    active.pump_audio();

    // Snapshot Zero-Copy (Arc<[u8]>): ~0.05 ms y 0 MB de copia en el hilo de grabación.
    let Some(snapshot) = active
        .replay_buffer
        .snapshot(Some(cfg.replay_duration.as_secs()))
    else {
        let msg = "El buffer aún no tiene suficiente video (espera 2 segundos).";
        record_and_publish_event(session, cfg, telemetry, msg, true);
        let _ = event_tx.send(EngineEvent::ClipFailed(msg.to_string()));
        return;
    };

    let game_name = match &active.state {
        RecordingState::RecordingGame { game_name, .. } => Some(game_name.as_str()),
        _ => None,
    };
    let clip_path = generate_clip_path(&cfg.clips_dir, game_name);
    let video_config = active.video_config;
    let event_tx_clone = event_tx.clone();
    let status_clone = Arc::clone(shared_status);
    let telemetry_clone = Arc::clone(telemetry);
    let target_secs = cfg.replay_duration.as_secs();
    let state_snap = active.state.clone();
    let enc_snap = active.encoder_label.clone();

    let _ = thread::Builder::new()
        .name("bals-mp4-writer".to_string())
        .spawn(move || match write_mp4_clip(&clip_path, &snapshot, video_config) {
            Ok(stats) => {
                let (buf_s, ram_mb) = if let Ok(mut guard) = status_clone.lock() {
                    guard.last_clip = Some(stats.path.clone());
                    (guard.buffered_secs as f32, guard.buffer_ram_mb as f32)
                } else {
                    (0.0, 0.0)
                };
                let fname = stats
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "clip.mp4".to_string());
                let msg = format!(
                    "CLIP GUARDADO: {fname} ({:.1}s · {:.1} MB)",
                    stats.clip_duration_secs,
                    stats.file_bytes as f64 / (1024.0 * 1024.0)
                );
                if let Ok(mut guard) = telemetry_clone.lock() {
                    if let Some(writer) = guard.as_mut() {
                        writer.record_clip_event(&msg, false);
                        let (tstate, gname) = recording_state_to_telemetry(&state_snap);
                        writer.publish(tstate, target_secs, buf_s, ram_mb, gname, &enc_snap);
                    }
                }
                let _ = event_tx_clone.send(EngineEvent::ClipSaved(stats));
            }
            Err(err) => {
                error!("Error escribiendo clip MP4: {err:#}");
                let err_msg = format!("Error guardando clip: {err:#}");
                if let Ok(mut guard) = telemetry_clone.lock() {
                    if let Some(writer) = guard.as_mut() {
                        writer.record_clip_event(&err_msg, true);
                        let (tstate, gname) = recording_state_to_telemetry(&state_snap);
                        writer.publish(tstate, target_secs, 0.0, 0.0, gname, &enc_snap);
                    }
                }
                let _ = event_tx_clone.send(EngineEvent::ClipFailed(format!("{err:#}")));
            }
        });
    crate::perf::save_held(held_since.elapsed());
}

fn recording_state_to_telemetry(state: &RecordingState) -> (TelemetryState, &str) {
    match state {
        RecordingState::Idle => (TelemetryState::Idle, ""),
        RecordingState::RecordingManual { monitor_label } => {
            (TelemetryState::RecordingManual, monitor_label.as_str())
        }
        RecordingState::RecordingGame {
            game_name,
            paused_alt_tab,
            ..
        } => {
            if *paused_alt_tab {
                (TelemetryState::PausedAltTab, game_name.as_str())
            } else {
                (TelemetryState::RecordingGame, game_name.as_str())
            }
        }
    }
}

fn record_and_publish_event(
    session: &Option<ActiveSession>,
    cfg: &AppConfig,
    telemetry: &Arc<Mutex<Option<TelemetryWriter>>>,
    msg: &str,
    is_err: bool,
) {
    if let Ok(mut guard) = telemetry.lock() {
        if let Some(writer) = guard.as_mut() {
            writer.record_clip_event(msg, is_err);
        }
    }
    publish_telemetry(session, cfg, telemetry);
}

fn publish_telemetry(
    session: &Option<ActiveSession>,
    cfg: &AppConfig,
    telemetry: &Arc<Mutex<Option<TelemetryWriter>>>,
) {
    let Ok(mut guard) = telemetry.lock() else {
        return;
    };
    let Some(writer) = guard.as_mut() else {
        return;
    };

    if let Some(active) = session {
        let (tstate, gname) = recording_state_to_telemetry(&active.state);
        writer.publish(
            tstate,
            cfg.replay_duration.as_secs(),
            active.replay_buffer.buffered_secs() as f32,
            (active.replay_buffer.total_bytes() as f64 / (1024.0 * 1024.0)) as f32,
            gname,
            &active.encoder_label,
        );
    } else {
        writer.publish(
            TelemetryState::Idle,
            cfg.replay_duration.as_secs(),
            0.0,
            0.0,
            "",
            "ZERO-COPY STANDBY",
        );
    }
}

fn update_shared_metrics(
    session: &Option<ActiveSession>,
    shared_status: &Arc<Mutex<EngineStatus>>,
) {
    if let Ok(mut guard) = shared_status.lock() {
        if let Some(active) = session {
            guard.state = active.state.clone();
            guard.buffered_secs = active.replay_buffer.buffered_secs();
            guard.buffer_ram_mb = active.replay_buffer.total_bytes() as f64 / (1024.0 * 1024.0);
        } else {
            guard.state = RecordingState::Idle;
            guard.buffered_secs = 0.0;
            guard.buffer_ram_mb = 0.0;
        }
    }
}

fn publish_status(
    session: &Option<ActiveSession>,
    cfg: &AppConfig,
    shared_status: &Arc<Mutex<EngineStatus>>,
    event_tx: &Sender<EngineEvent>,
    telemetry: &Arc<Mutex<Option<TelemetryWriter>>>,
) {
    update_shared_metrics(session, shared_status);
    publish_telemetry(session, cfg, telemetry);
    if let Ok(guard) = shared_status.lock() {
        let _ = event_tx.send(EngineEvent::StatusChanged(guard.clone()));
    }
}
