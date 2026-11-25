//! Autoevaluación del rendimiento: `bals --soak <minutos> <carpeta> [escenario|-] [segundos entre clips]`.
//!
//! Graba el monitor con el motor real (el mismo `RecorderEngine` de la bandeja, sin la bandeja ni los atajos), guarda un
//! clip cada tanto (60 s si no se indica) y cada segundo se mide a sí mismo: CPU, RAM, VRAM, uso de cada motor de la GPU,
//! handles, hilos y objetos de Windows, más lo que cuenta el motor (cuadros codificados, cuadros salteados y cuánto
//! trabaja cada tick CFR). Al terminar busca fugas (lo que sigue creciendo con el buffer ya lleno) y escribe en `<carpeta>`:
//! - `perf.csv`: una muestra por segundo.
//! - `informe.md` y `informe.toml`: el resumen, los avisos y el veredicto (también va a la consola).
//!
//! No toca `%APPDATA%\Bals\config.toml` ni la carpeta de clips del usuario (los clips quedan en `<carpeta>\clips`).
//! Sale con 0 si todo está bien, 3 si hay algo para revisar y 1 si no pudo medir.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    IDXGIAdapter3, IDXGIFactory1,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW,
};
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentThread, GetGuiResources,
    GetProcessHandleCount, GetProcessTimes, GetThreadTimes,
};
use windows::core::{Interface, PCWSTR, w};

use crate::config::{
    AppConfig, CaptureMode, EncoderPreference, FrameRate, QualityPreset, Resolution,
};
use crate::engine::{EngineCommand, EngineEvent, RecorderEngine, RecordingState};

// --- Lo que cuenta el motor ------------------------------------------------------------------------------------------
//
// Contadores atómicos que el motor actualiza en cada tick. Fuera de `--soak` están apagados y no hacen nada.

static ACTIVE: AtomicBool = AtomicBool::new(false);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);
static BUSY_SUM_US: AtomicU64 = AtomicU64::new(0);
static BUSY_MAX_US: AtomicU64 = AtomicU64::new(0);
static SAVE_HELD_US: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Fin de un tick CFR: cuánto trabajó (sin contar la espera hasta el tick) y si codificó un cuadro.
pub fn tick(busy: Duration, encoded: bool) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let us = busy.as_micros() as u64;
    TICKS.fetch_add(1, Ordering::Relaxed);
    BUSY_SUM_US.fetch_add(us, Ordering::Relaxed);
    BUSY_MAX_US.fetch_max(us, Ordering::Relaxed);
    if encoded {
        FRAMES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Cuadros que el motor salteó al resincronizar el reloj CFR (se despertó más de 120 ms tarde).
pub fn skipped(frames: u64) {
    if ACTIVE.load(Ordering::Relaxed) {
        SKIPPED.fetch_add(frames, Ordering::Relaxed);
    }
}

/// Cuánto tuvo frenado al hilo del motor el pedido de guardar un clip (snapshot + lanzar el hilo del MP4).
pub fn save_held(held: Duration) {
    if ACTIVE.load(Ordering::Relaxed)
        && let Ok(mut v) = SAVE_HELD_US.lock()
    {
        v.push(held.as_micros() as u64);
    }
}

// --- Lo que se mide del proceso --------------------------------------------------------------------------------------

/// Una muestra por segundo.
#[derive(Clone, Default)]
struct Sample {
    /// Segundos desde que arrancó el motor.
    t: f64,
    /// CPU de Bals sin contar esta medición, y la de la medición (% de toda la CPU).
    cpu: f64,
    cpu_muestreo: f64,
    privada_mb: f64,
    ws_mb: f64,
    vram_mb: f64,
    handles: u32,
    hilos: u32,
    gdi: u32,
    user: u32,
    /// Uso de la GPU: el motor más ocupado y cada tipo de motor (%).
    gpu: f64,
    gpu_3d: f64,
    gpu_copia: f64,
    gpu_encoder: f64,
    gpu_conversion: f64,
    gpu_otros: f64,
    /// Había un clip guardándose.
    guardando: bool,
    /// Lo que contó el motor en este segundo.
    cuadros: u64,
    salteados: u64,
    tick_prom_ms: f64,
    tick_max_ms: f64,
    /// RAM del ReplayBuffer según el motor.
    buffer_mb: f64,
}

const CSV_HEADER: &str = "t,cpu,cpu_muestreo,privada_mb,ws_mb,vram_mb,handles,hilos,gdi,user,gpu,gpu_3d,gpu_copia,\
gpu_encoder,gpu_conversion,gpu_otros,guardando,cuadros,salteados,tick_prom_ms,tick_max_ms,buffer_mb";

impl Sample {
    fn csv(&self) -> String {
        format!(
            "{:.1},{:.2},{:.2},{:.1},{:.1},{:.1},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{},{},{},{:.3},{:.3},{:.1}",
            self.t,
            self.cpu,
            self.cpu_muestreo,
            self.privada_mb,
            self.ws_mb,
            self.vram_mb,
            self.handles,
            self.hilos,
            self.gdi,
            self.user,
            self.gpu,
            self.gpu_3d,
            self.gpu_copia,
            self.gpu_encoder,
            self.gpu_conversion,
            self.gpu_otros,
            self.guardando as u8,
            self.cuadros,
            self.salteados,
            self.tick_prom_ms,
            self.tick_max_ms,
            self.buffer_mb,
        )
    }
}

/// Mide este proceso.
struct Meter {
    cores: f64,
    pid: u32,
    at: Instant,
    cpu: u64,
    own: u64,
    adapters: Vec<IDXGIAdapter3>,
    gpu: Option<Gpu>,
}

fn ticks_of(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// (CPU de todo el proceso, de este hilo), en 100 ns.
fn cpu_times() -> (u64, u64) {
    let [mut c, mut e, mut k, mut u, mut tc, mut te, mut tk, mut tu] = [FILETIME::default(); 8];
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        let _ = GetThreadTimes(GetCurrentThread(), &mut tc, &mut te, &mut tk, &mut tu);
    }
    (ticks_of(k) + ticks_of(u), ticks_of(tk) + ticks_of(tu))
}

fn thread_count(pid: u32) -> u32 {
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return 0;
        };
        let mut e = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut n = 0;
        if Thread32First(snap, &mut e).is_ok() {
            loop {
                n += (e.th32OwnerProcessID == pid) as u32;
                if Thread32Next(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        n
    }
}

impl Meter {
    fn new() -> Self {
        let adapters = unsafe {
            let mut v = Vec::new();
            if let Ok(f) = CreateDXGIFactory1::<IDXGIFactory1>() {
                let mut i = 0;
                while let Ok(a) = f.EnumAdapters1(i) {
                    i += 1;
                    if let Ok(a) = a.cast::<IDXGIAdapter3>() {
                        v.push(a);
                    }
                }
            }
            v
        };
        let (cpu, own) = cpu_times();
        Meter {
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
            pid: std::process::id(),
            at: Instant::now(),
            cpu,
            own,
            adapters,
            gpu: Gpu::open(),
        }
    }

    fn sample(&mut self, t: f64, guardando: bool, buffer_mb: f64) -> Sample {
        let wall = self.at.elapsed().as_secs_f64().max(0.001) * 10_000_000.0;
        self.at = Instant::now();
        let (cpu, own) = cpu_times();
        let pct = |d: u64| d as f64 / wall / self.cores * 100.0;
        let own_d = own.saturating_sub(self.own);
        let all_d = cpu.saturating_sub(self.cpu);
        (self.cpu, self.own) = (cpu, own);

        let mut mem = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let mut handles = 0u32;
        let (gdi, user, vram) = unsafe {
            let p = GetCurrentProcess();
            let _ = K32GetProcessMemoryInfo(
                p,
                &mut mem as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                mem.cb,
            );
            let _ = GetProcessHandleCount(p, &mut handles);
            let vram: u64 = self
                .adapters
                .iter()
                .filter_map(|a| {
                    let mut i = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                    a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut i)
                        .ok()
                        .map(|_| i.CurrentUsage)
                })
                .sum();
            (
                GetGuiResources(p, GR_GDIOBJECTS),
                GetGuiResources(p, GR_USEROBJECTS),
                vram,
            )
        };
        let g = self
            .gpu
            .as_mut()
            .map(|g| g.read(self.pid))
            .unwrap_or_default();

        let turns = TICKS.swap(0, Ordering::Relaxed);
        let busy_sum = BUSY_SUM_US.swap(0, Ordering::Relaxed);
        let mb = |b: u64| b as f64 / 1_048_576.0;
        Sample {
            t,
            cpu: pct(all_d.saturating_sub(own_d)),
            cpu_muestreo: pct(own_d),
            privada_mb: mb(mem.PrivateUsage as u64),
            ws_mb: mb(mem.WorkingSetSize as u64),
            vram_mb: mb(vram),
            handles,
            hilos: thread_count(self.pid),
            gdi,
            user,
            gpu: g.iter().cloned().fold(0.0, f64::max).min(100.0),
            gpu_3d: g[0],
            gpu_copia: g[1],
            gpu_encoder: g[2],
            gpu_conversion: g[3],
            gpu_otros: g[4],
            guardando,
            cuadros: FRAMES.swap(0, Ordering::Relaxed),
            salteados: SKIPPED.swap(0, Ordering::Relaxed),
            tick_prom_ms: busy_sum as f64 / turns.max(1) as f64 / 1000.0,
            tick_max_ms: BUSY_MAX_US.swap(0, Ordering::Relaxed) as f64 / 1000.0,
            buffer_mb,
        }
    }
}

/// Uso de cada motor de la GPU por proceso, con los contadores de Windows que usa el Administrador de tareas
/// (por su nombre en inglés: funcionan con Windows en cualquier idioma).
struct Gpu {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    buf: Vec<u64>,
}

impl Gpu {
    fn open() -> Option<Gpu> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            let mut counter = PDH_HCOUNTER::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(
                query,
                w!("\\GPU Engine(*)\\Utilization Percentage"),
                0,
                &mut counter,
            ) != 0
            {
                PdhCloseQuery(query);
                return None;
            }
            // El uso sale de la diferencia entre dos lecturas: esta es la primera.
            PdhCollectQueryData(query);
            Some(Gpu {
                query,
                counter,
                buf: Vec::new(),
            })
        }
    }

    /// 3D, copia, encoder, conversión y otros (%) de los motores del proceso `pid`.
    fn read(&mut self, pid: u32) -> [f64; 5] {
        let mut out = [0.0; 5];
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return out;
            }
            let (mut size, mut count) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                None,
            ) != PDH_MORE_DATA
            {
                return out;
            }
            self.buf.resize((size as usize).div_ceil(8), 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(items),
            ) != 0
            {
                return out;
            }
            let prefix = format!("pid_{pid}_");
            for it in std::slice::from_raw_parts(items, count as usize) {
                // 0 o 1: dato válido (o nuevo).
                if it.FmtValue.CStatus > 1 {
                    continue;
                }
                let name = it.szName.to_string().unwrap_or_default();
                if !name.starts_with(&prefix) {
                    continue;
                }
                let kind = match name.rsplit("engtype_").next().unwrap_or("") {
                    "3D" => 0,
                    "Copy" => 1,
                    "VideoEncode" => 2,
                    "VideoProcessing" => 3,
                    _ => 4,
                };
                out[kind] += it.FmtValue.Anonymous.doubleValue;
            }
        }
        out
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

// --- La prueba -------------------------------------------------------------------------------------------------------

/// La configuración de la prueba: la de fábrica grabando el monitor, sin micrófono, sin sonidos ni actualizaciones, con
/// los clips en `clips_dir`. `None` si el escenario no existe.
fn soak_config(clips_dir: PathBuf, scenario: Option<&str>) -> Option<AppConfig> {
    let mut cfg = AppConfig {
        capture_mode: CaptureMode::ManualMonitor,
        mic_enabled: false,
        sound_feedback: false,
        auto_update: false,
        clips_dir,
        encoder_pref: AppConfig::recommended_encoder_pref(),
        ..AppConfig::default()
    };
    match scenario {
        None | Some("monitor") => {}
        Some("mic") => cfg.mic_enabled = true,
        Some("cpu") => cfg.encoder_pref = EncoderPreference::SoftwareCpu,
        Some("30") => cfg.fps = FrameRate::Fps30,
        Some("ahorro") => cfg.quality = QualityPreset::Compact,
        Some("alta") => cfg.quality = QualityPreset::High,
        Some("sincursor") => cfg.capture_cursor = false,
        Some("juegos") => cfg.capture_mode = CaptureMode::AutoDetectGames,
        Some(r) => cfg.resolution = Resolution::from_str_opt(r)?,
    }
    Some(cfg)
}

const USAGE: &str = "uso: bals --soak <minutos> <carpeta> [escenario|-] [segundos entre clips]\n\
escenarios: monitor (por defecto), mic, cpu, 30, ahorro, alta, sincursor, juegos, 720, 1080, 1440";

pub fn soak(args: &[String]) -> i32 {
    let minutes: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let Some(dir) = args.get(1).map(PathBuf::from) else {
        println!("{USAGE}");
        return 1;
    };
    // "-": el escenario por defecto, para poder indicar igual los segundos entre clips.
    let scenario = args.get(2).map(String::as_str).filter(|s| *s != "-");
    let every: u64 = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .filter(|&s| s >= 5)
        .unwrap_or(60);
    if crate::ipc::is_daemon_running() {
        println!(
            "ERROR Bals está abierto en la bandeja: ciérralo antes de medir (los números se mezclarían y ambos escriben la misma telemetría)"
        );
        return 1;
    }
    let clips_dir = dir.join("clips");
    if std::fs::create_dir_all(&clips_dir).is_err() {
        println!("ERROR no se pudo crear {}", clips_dir.display());
        return 1;
    }
    let Some(cfg) = soak_config(clips_dir, scenario) else {
        println!("ERROR escenario desconocido\n{USAGE}");
        return 1;
    };
    let Ok(file) = std::fs::File::create(dir.join("perf.csv")) else {
        println!("ERROR no se pudo escribir en {}", dir.display());
        return 1;
    };
    let mut out = std::io::BufWriter::new(file);
    let _ = writeln!(out, "{CSV_HEADER}");

    let replay_secs = cfg.replay_duration.as_secs();
    let fps = cfg.fps.as_u32();
    let warmup = replay_secs as f64 + 15.0;
    ACTIVE.store(true, Ordering::Relaxed);
    let mut meter = Meter::new();
    let before = meter.sample(0.0, false, 0.0);

    let start = Instant::now();
    let engine = match RecorderEngine::spawn(cfg.clone()) {
        Ok(e) => e,
        Err(err) => {
            println!("ERROR no se pudo iniciar el motor: {err:#}");
            return 1;
        }
    };
    println!(
        "midiendo {minutes} min ({}, {fps} fps, un clip cada {every} s; el buffer se llena en {replay_secs} s)",
        cfg.resolution
    );

    let mut samples: Vec<Sample> = Vec::new();
    let mut log = vec![format!("un clip cada {every} s")];
    let (mut encoder, mut saving, mut ok, mut failed) =
        (String::new(), None::<Instant>, 0u32, Vec::new());
    let (mut save_total_ms, mut mux_ms) = (Vec::new(), Vec::new());
    let mut state: Option<RecordingState> = None;
    let mut next_save = every as f64;
    let total = minutes * 60.0;
    let mut tick = 1.0;
    // Después del final se espera a que termine el último clip que se estaba guardando.
    while tick <= total || (saving.is_some() && tick <= total + 30.0) {
        let wait = tick - start.elapsed().as_secs_f64();
        if wait > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
        let t = start.elapsed().as_secs_f64();
        for e in engine.event_rx.try_iter() {
            match e {
                EngineEvent::ClipSaved(stats) => {
                    if let Some(sent) = saving.take() {
                        save_total_ms.push(sent.elapsed().as_secs_f64() * 1000.0);
                    }
                    mux_ms.push(stats.mux_elapsed_ms);
                    ok += 1;
                }
                EngineEvent::ClipFailed(m) => {
                    saving = None;
                    failed.push(format!("{t:.0} s: {m}"));
                }
                EngineEvent::StatusChanged(st) => {
                    if state.as_ref() != Some(&st.state) {
                        log.push(format!("{t:.0} s: {:?}", st.state));
                        state = Some(st.state);
                    }
                }
            }
        }
        if encoder.is_empty() && t >= 3.0 {
            encoder = crate::ipc::read_engine_telemetry()
                .map(|s| s.encoder_label)
                .unwrap_or_default();
        }
        let buffer_mb = engine.status.lock().map_or(0.0, |s| s.buffer_ram_mb);
        let s = meter.sample(t, saving.is_some(), buffer_mb);
        let _ = writeln!(out, "{}", s.csv());
        let _ = out.flush();
        live(&s, samples.last(), fps);
        samples.push(s);
        if tick >= next_save && tick <= total {
            let _ = engine.cmd_tx.send(EngineCommand::SaveClip);
            saving = Some(Instant::now());
            next_save += every as f64;
        }
        tick += 1.0;
    }
    if saving.is_some() {
        failed.push("el último clip no terminó de guardarse en 30 s".to_string());
    }
    engine.shutdown();
    // Lo que queda después de cerrar el motor (hilos y handles que no se soltaron).
    std::thread::sleep(Duration::from_secs(2));
    let after = meter.sample(start.elapsed().as_secs_f64(), false, 0.0);
    ACTIVE.store(false, Ordering::Relaxed);

    let held_ms: Vec<f64> = SAVE_HELD_US
        .lock()
        .map(|v| v.iter().map(|&us| us as f64 / 1000.0).collect())
        .unwrap_or_default();
    let input = Input {
        scenario: scenario.unwrap_or("monitor"),
        minutes,
        warmup,
        fps,
        encoder,
        ok,
        failed,
        log,
        before,
        after,
        held_ms,
        save_total_ms,
        mux_ms,
    };
    let report = analyze(&input, &samples);
    let md = report.markdown();
    write_report(&dir, &report, &md);
    println!("{md}");
    if report.veredicto == "OK" { 0 } else { 3 }
}

fn write_report(dir: &Path, report: &Report, md: &str) {
    match toml::to_string_pretty(report) {
        Ok(s) => {
            let _ = std::fs::write(dir.join("informe.toml"), s);
        }
        Err(err) => println!("ERROR no se pudo escribir informe.toml: {err}"),
    }
    let _ = std::fs::write(dir.join("informe.md"), md);
}

/// Lo que se ve en la consola mientras graba (para seguirla en vivo): un "ESTADO" por minuto y un "AVISO" cuando algo
/// se sale de lo normal.
fn live(s: &Sample, prev: Option<&Sample>, fps: u32) {
    let t = s.t.round();
    if (t as u64).is_multiple_of(60) {
        println!(
            "ESTADO {t} s: CPU {:.1} %, RAM {:.0} MB (buffer {:.0} MB), VRAM {:.0} MB, GPU {:.1} %, {} handles, {} hilos",
            s.cpu, s.privada_mb, s.buffer_mb, s.vram_mb, s.gpu, s.handles, s.hilos
        );
    }
    if s.t < 3.0 {
        return;
    }
    let budget = 1000.0 / fps.max(1) as f64;
    if (s.cuadros as f64) < fps as f64 * 0.9 {
        println!(
            "AVISO {t} s: el encoder recibió {} cuadros de {fps}",
            s.cuadros
        );
    }
    if s.salteados > 0 {
        println!(
            "AVISO {t} s: el motor salteó {} cuadros por despertarse tarde",
            s.salteados
        );
    }
    if s.tick_max_ms > budget {
        println!(
            "AVISO {t} s: un tick del motor tardó {:.1} ms (un cuadro dura {budget:.1} ms)",
            s.tick_max_ms
        );
    }
    if let Some(p) =
        prev.filter(|p| !s.guardando && !p.guardando && s.privada_mb - p.privada_mb > 50.0)
    {
        println!(
            "AVISO {t} s: la RAM privada subió {:.0} MB en un segundo sin guardar un clip",
            s.privada_mb - p.privada_mb
        );
    }
}

// --- El informe ------------------------------------------------------------------------------------------------------

struct Input<'a> {
    scenario: &'a str,
    minutes: f64,
    warmup: f64,
    fps: u32,
    encoder: String,
    ok: u32,
    failed: Vec<String>,
    log: Vec<String>,
    before: Sample,
    after: Sample,
    held_ms: Vec<f64>,
    save_total_ms: Vec<f64>,
    mux_ms: Vec<f64>,
}

#[derive(Serialize)]
struct Stat {
    prom: f64,
    p95: f64,
    max: f64,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn stat(v: impl Iterator<Item = f64>) -> Stat {
    let mut v: Vec<f64> = v.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return Stat {
            prom: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    v.sort_by(f64::total_cmp);
    Stat {
        prom: round2(v.iter().sum::<f64>() / v.len() as f64),
        p95: round2(v[(v.len() - 1) * 95 / 100]),
        max: round2(v[v.len() - 1]),
    }
}

/// Un recurso que se revisa por fugas.
#[derive(Serialize)]
struct Leak {
    recurso: &'static str,
    unidad: &'static str,
    /// Al terminar de llenarse el buffer y al final (el piso de cada tramo de 30 s: sin los picos de guardar).
    inicio: f64,
    fin: f64,
    pendiente_por_min: f64,
    veredicto: &'static str,
}

#[derive(Serialize)]
struct GpuPorMotor {
    d3: f64,
    copia: f64,
    encoder: f64,
    conversion: f64,
}

#[derive(Serialize)]
struct Report {
    veredicto: &'static str,
    escenario: String,
    minutos: f64,
    encoder: String,
    fps_objetivo: u32,
    clips_ok: u32,
    clips_fallidos: Vec<String>,
    avisos: Vec<String>,
    eventos: Vec<String>,
    al_cerrar: String,
    cuadros_salteados: u64,
    presupuesto_por_cuadro_ms: f64,
    cpu_muestreo_pct: f64,
    cpu_pct: Stat,
    cpu_guardando_pct: Stat,
    gpu_pct: Stat,
    gpu_por_motor_prom: GpuPorMotor,
    ram_privada_mb: Stat,
    buffer_mb: Stat,
    vram_mb: Stat,
    fps_real: Stat,
    tick_ms: Stat,
    guardado_frenada_ms: Stat,
    guardado_mp4_ms: Stat,
    guardado_total_ms: Stat,
    fugas: Vec<Leak>,
}

/// Piso (mínimo) de cada tramo de 30 s después del calentamiento: (minuto del tramo, valor).
fn floors(samples: &[&Sample], f: impl Fn(&Sample) -> f64) -> Vec<(f64, f64)> {
    let Some(first) = samples.first() else {
        return Vec::new();
    };
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut window = -1i64;
    for s in samples {
        let w = ((s.t - first.t) / 30.0) as i64;
        let v = f(s);
        if w != window {
            window = w;
            out.push((s.t / 60.0, v));
        } else if let Some(last) = out.last_mut() {
            last.1 = last.1.min(v);
        }
    }
    out
}

/// Pendiente por minuto (mínimos cuadrados).
fn slope(p: &[(f64, f64)]) -> f64 {
    let n = p.len() as f64;
    let mx = p.iter().map(|x| x.0).sum::<f64>() / n;
    let my = p.iter().map(|x| x.1).sum::<f64>() / n;
    let num: f64 = p.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = p.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    if den > 0.0 { num / den } else { 0.0 }
}

fn analyze(i: &Input, samples: &[Sample]) -> Report {
    let steady: Vec<&Sample> = samples.iter().filter(|s| s.t >= i.warmup).collect();
    let recording: Vec<&Sample> = samples.iter().filter(|s| s.t >= 3.0).collect();
    let budget = 1000.0 / i.fps.max(1) as f64;
    let mut avisos = Vec::new();

    // Recurso, unidad, cómo se lee, cuánto puede subir por minuto y en total sin que sea una fuga.
    type Get = fn(&Sample) -> f64;
    let checks: [(&'static str, &'static str, Get, f64, f64); 7] = [
        ("RAM privada", "MB", |s| s.privada_mb, 0.5, 5.0),
        (
            "RAM fuera del ReplayBuffer",
            "MB",
            |s| s.privada_mb - s.buffer_mb,
            0.5,
            5.0,
        ),
        ("memoria de video", "MB", |s| s.vram_mb, 0.5, 5.0),
        ("handles", "", |s| s.handles as f64, 2.0, 20.0),
        ("hilos", "", |s| s.hilos as f64, 0.2, 3.0),
        ("objetos GDI", "", |s| s.gdi as f64, 1.0, 10.0),
        ("objetos USER", "", |s| s.user as f64, 1.0, 10.0),
    ];
    let mut fugas = Vec::new();
    for (recurso, unidad, get, per_min, total) in checks {
        let f = floors(&steady, get);
        let (inicio, fin) = (
            f.first().map_or(0.0, |x| x.1),
            f.last().map_or(0.0, |x| x.1),
        );
        let m = if f.len() >= 3 { slope(&f) } else { 0.0 };
        let veredicto = if f.len() < 4 {
            "corrida corta para saber"
        } else if m > per_min && fin - inicio > total {
            avisos.push(format!(
                "Posible fuga de {recurso}: sube {m:.2} {unidad}/min ({inicio:.1} → {fin:.1})."
            ));
            "posible fuga"
        } else {
            "estable"
        };
        fugas.push(Leak {
            recurso,
            unidad,
            inicio: round2(inicio),
            fin: round2(fin),
            pendiente_por_min: round2(m),
            veredicto,
        });
    }

    let fps_real = stat(recording.iter().map(|s| s.cuadros as f64));
    let salteados: u64 = recording.iter().map(|s| s.salteados).sum();
    let tick = stat(recording.iter().map(|s| s.tick_max_ms));
    let low_fps = fps_real.prom < i.fps as f64 * 0.97;
    if low_fps {
        avisos.push(format!(
            "El encoder recibe {:.1} cuadros por segundo de {}: el video pierde cuadros.",
            fps_real.prom, i.fps
        ));
    }
    if salteados > 0 {
        avisos.push(format!(
            "El motor se despertó tarde y salteó {salteados} cuadros."
        ));
    }
    if tick.max > budget {
        avisos.push(format!(
            "Algún tick del motor tardó {:.1} ms, más que un cuadro ({budget:.1} ms).",
            tick.max
        ));
    } else if tick.p95 > budget * 0.5 {
        avisos.push(format!(
            "El motor usa más de la mitad del tiempo de un cuadro ({:.1} de {budget:.1} ms).",
            tick.p95
        ));
    }
    let held = stat(i.held_ms.iter().copied());
    if held.max > budget {
        avisos.push(format!(
            "Guardar un clip frenó al motor {:.1} ms (más que un cuadro).",
            held.max
        ));
    }
    let cpu = stat(steady.iter().filter(|s| !s.guardando).map(|s| s.cpu));
    if cpu.prom > 5.0 {
        avisos.push(format!(
            "Bals usa {:.1} % de la CPU en promedio grabando.",
            cpu.prom
        ));
    }
    if !i.failed.is_empty() {
        avisos.push(format!(
            "{} clips fallaron (ver clips_fallidos).",
            i.failed.len()
        ));
    }
    let left = i.after.hilos as i64 - i.before.hilos as i64;
    let left_h = i.after.handles as i64 - i.before.handles as i64;
    let al_cerrar = format!(
        "al cerrar el motor quedaron {left:+} hilos y {left_h:+} handles respecto de antes de empezar"
    );
    if left > 2 || left_h > 50 {
        avisos.push(format!(
            "Al cerrar el motor no se soltó todo: {left:+} hilos y {left_h:+} handles."
        ));
    }
    if i.minutes * 60.0 < i.warmup + 120.0 {
        avisos.push(format!(
            "La corrida es corta: para buscar fugas conviene grabar al menos {:.0} min.",
            (i.warmup + 300.0) / 60.0
        ));
    }
    let leak = fugas.iter().any(|f| f.veredicto == "posible fuga");
    let veredicto = if leak || !i.failed.is_empty() || low_fps {
        "REVISAR"
    } else {
        "OK"
    };

    let motor = |f: fn(&Sample) -> f64| stat(steady.iter().map(|s| f(s))).prom;
    Report {
        veredicto,
        escenario: i.scenario.to_string(),
        minutos: i.minutes,
        encoder: i.encoder.clone(),
        fps_objetivo: i.fps,
        clips_ok: i.ok,
        clips_fallidos: i.failed.clone(),
        avisos,
        eventos: i.log.clone(),
        al_cerrar,
        cuadros_salteados: salteados,
        presupuesto_por_cuadro_ms: round2(budget),
        cpu_muestreo_pct: stat(samples.iter().map(|s| s.cpu_muestreo)).prom,
        cpu_pct: cpu,
        cpu_guardando_pct: stat(samples.iter().filter(|s| s.guardando).map(|s| s.cpu)),
        gpu_pct: stat(steady.iter().map(|s| s.gpu)),
        gpu_por_motor_prom: GpuPorMotor {
            d3: motor(|s| s.gpu_3d),
            copia: motor(|s| s.gpu_copia),
            encoder: motor(|s| s.gpu_encoder),
            conversion: motor(|s| s.gpu_conversion),
        },
        ram_privada_mb: stat(steady.iter().map(|s| s.privada_mb)),
        buffer_mb: stat(steady.iter().map(|s| s.buffer_mb)),
        vram_mb: stat(steady.iter().map(|s| s.vram_mb)),
        fps_real,
        tick_ms: tick,
        guardado_frenada_ms: held,
        guardado_mp4_ms: stat(i.mux_ms.iter().copied()),
        guardado_total_ms: stat(i.save_total_ms.iter().copied()),
        fugas,
    }
}

impl Report {
    fn markdown(&self) -> String {
        let st = |s: &Stat| format!("{} prom · {} p95 · {} máx", s.prom, s.p95, s.max);
        let mut m = String::new();
        let _ = writeln!(
            m,
            "# Rendimiento de Bals: {}\n\n{} min, escenario {}, encoder {}, {} clips guardados.\n",
            self.veredicto,
            self.minutos,
            self.escenario,
            if self.encoder.is_empty() {
                "?"
            } else {
                &self.encoder
            },
            self.clips_ok
        );
        m += "## Avisos\n\n";
        if self.avisos.is_empty() {
            m += "Ninguno.\n";
        }
        for a in &self.avisos {
            let _ = writeln!(m, "- {a}");
        }
        m += "\n## Consumo grabando (después de llenarse el buffer)\n\n";
        let _ = writeln!(m, "- CPU: {} %", st(&self.cpu_pct));
        let _ = writeln!(m, "- CPU al guardar: {} %", st(&self.cpu_guardando_pct));
        let _ = writeln!(m, "- GPU (motor más ocupado): {} %", st(&self.gpu_pct));
        let g = &self.gpu_por_motor_prom;
        let _ = writeln!(
            m,
            "- GPU por motor (promedio): 3D {} %, copia {} %, encoder {} %, conversión {} %",
            g.d3, g.copia, g.encoder, g.conversion
        );
        let _ = writeln!(m, "- RAM privada: {} MB", st(&self.ram_privada_mb));
        let _ = writeln!(m, "- ReplayBuffer: {} MB", st(&self.buffer_mb));
        let _ = writeln!(m, "- VRAM: {} MB", st(&self.vram_mb));
        let _ = writeln!(
            m,
            "- Esta medición usó {:.2} % de la CPU (ya descontado arriba)",
            self.cpu_muestreo_pct
        );
        m += "\n## Video\n\n";
        let _ = writeln!(
            m,
            "- Cuadros por segundo al encoder: {} (pedidos: {})",
            st(&self.fps_real),
            self.fps_objetivo
        );
        let _ = writeln!(
            m,
            "- Cuadros salteados por despertarse tarde: {}",
            self.cuadros_salteados
        );
        let _ = writeln!(
            m,
            "- Trabajo por tick (máximo de cada segundo): {} ms, de {} ms por cuadro",
            st(&self.tick_ms),
            self.presupuesto_por_cuadro_ms
        );
        m += "\n## Guardar un clip\n\n";
        let _ = writeln!(m, "- Motor frenado: {} ms", st(&self.guardado_frenada_ms));
        let _ = writeln!(m, "- Escritura del MP4: {} ms", st(&self.guardado_mp4_ms));
        let _ = writeln!(
            m,
            "- Desde el pedido hasta el clip listo: {} ms",
            st(&self.guardado_total_ms)
        );
        for f in &self.clips_fallidos {
            let _ = writeln!(m, "- Falló: {f}");
        }
        m += "\n## Fugas (piso de cada tramo de 30 s, sin los picos de guardar)\n\n";
        m += "| Recurso | Al llenarse el buffer | Al final | Por minuto | Veredicto |\n|---|---|---|---|---|\n";
        for f in &self.fugas {
            let _ = writeln!(
                m,
                "| {} | {} {} | {} {} | {:+} | {} |",
                f.recurso, f.inicio, f.unidad, f.fin, f.unidad, f.pendiente_por_min, f.veredicto
            );
        }
        let _ = writeln!(m, "\n{}.", capitalize(&self.al_cerrar));
        if !self.eventos.is_empty() {
            m += "\n## Eventos\n\n";
            for e in &self.eventos {
                let _ = writeln!(m, "- {e}");
            }
        }
        m
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t: f64, privada_mb: f64) -> Sample {
        Sample {
            t,
            privada_mb,
            ..Default::default()
        }
    }

    #[test]
    fn slope_per_minute_of_floors_ignores_save_spikes() {
        // Sube 1 MB por minuto con un pico de +80 MB cada 30 s (un clip guardándose).
        let samples: Vec<Sample> = (0..600)
            .map(|t| {
                let t = t as f64;
                let spike = if t as u64 % 30 == 10 { 80.0 } else { 0.0 };
                at(t, 200.0 + t / 60.0 + spike)
            })
            .collect();
        let refs: Vec<&Sample> = samples.iter().collect();
        let f = floors(&refs, |s| s.privada_mb);
        assert_eq!(f.len(), 20);
        assert!((slope(&f) - 1.0).abs() < 0.01);
    }

    #[test]
    fn stat_percentiles() {
        let s = stat((1..=100).map(|x| x as f64));
        assert_eq!((s.prom, s.p95, s.max), (50.5, 95.0, 100.0));
        let empty = stat(std::iter::empty());
        assert_eq!(empty.max, 0.0);
    }

    #[test]
    fn scenarios() {
        let dir = PathBuf::from("clips");
        assert!(
            soak_config(dir.clone(), None)
                .is_some_and(|c| c.capture_mode == CaptureMode::ManualMonitor && !c.auto_update)
        );
        assert!(
            soak_config(dir.clone(), Some("1440"))
                .is_some_and(|c| c.resolution == Resolution::P1440)
        );
        assert!(soak_config(dir, Some("no-existe")).is_none());
    }
}
