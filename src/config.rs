use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Resoluciones soportadas por Bals (2K, 1080p, 720p).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    /// 1280x720 (HD) — Ideal para PCs de muy bajos recursos.
    P720,
    /// 1920x1080 (Full HD) — Equilibrio óptimo entre calidad y peso.
    P1080,
    /// 2560x1440 (2K / QHD) — Alta nitidez para monitores 1440p.
    P1440,
}

impl Resolution {
    pub const fn dimensions(self) -> (u32, u32) {
        match self {
            Resolution::P720 => (1280, 720),
            Resolution::P1080 => (1920, 1080),
            Resolution::P1440 => (2560, 1440),
        }
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "720" | "720p" | "hd" => Some(Resolution::P720),
            "1080" | "1080p" | "fhd" => Some(Resolution::P1080),
            "1440" | "1440p" | "2k" | "qhd" => Some(Resolution::P1440),
            _ => None,
        }
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Resolution::P720 => write!(f, "720p (1280x720)"),
            Resolution::P1080 => write!(f, "1080p (1920x1080)"),
            Resolution::P1440 => write!(f, "2K (2560x1440)"),
        }
    }
}

/// Tasas de cuadros soportadas por Bals (30 fps y 60 fps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameRate {
    Fps30,
    Fps60,
}

impl FrameRate {
    pub const fn as_u32(self) -> u32 {
        match self {
            FrameRate::Fps30 => 30,
            FrameRate::Fps60 => 60,
        }
    }

    /// Duración teórica de un frame en ticks de 100 nanosegundos (unidad estándar de Media Foundation).
    pub const fn frame_duration_100ns(self) -> i64 {
        10_000_000 / self.as_u32() as i64
    }

    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "30" | "30fps" => Some(FrameRate::Fps30),
            "60" | "60fps" => Some(FrameRate::Fps60),
            _ => None,
        }
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} fps", self.as_u32())
    }
}

/// Perfil de calidad/compresión para controlar tamaño de clips y RAM del buffer (0% CPU extra).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityPreset {
    /// ~45% menos peso y RAM (ideal para compartir fácil o PCs con poca RAM/disco).
    Compact,
    /// Equilibrio óptimo entre nitidez y peso.
    Balanced,
    /// Máxima nitidez (~40% más bitrate).
    High,
}

impl QualityPreset {
    pub const fn bitrate_scale_pct(self) -> u32 {
        match self {
            QualityPreset::Compact => 55,
            QualityPreset::Balanced => 100,
            QualityPreset::High => 140,
        }
    }
}

impl fmt::Display for QualityPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QualityPreset::Compact => write!(f, "Ahorro"),
            QualityPreset::Balanced => write!(f, "Normal"),
            QualityPreset::High => write!(f, "Alta"),
        }
    }
}

/// Preferencia de encoder de video H.264.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EncoderPreference {
    /// Usa el encoder por Hardware de tu GPU (AMD AMF / NVIDIA NVENC / Intel QSV),
    /// con fallback seguro a CPU si el driver falla al iniciar.
    #[default]
    #[serde(alias = "AutoGpu")]
    HardwareGpu,
    /// Usa el encoder H.264 por Software en CPU (útil para diagnóstico o bitrate CBR muy estricto).
    SoftwareCpu,
}

impl EncoderPreference {
    pub const fn label(self) -> &'static str {
        match self {
            EncoderPreference::HardwareGpu => "GPU (HW)",
            EncoderPreference::SoftwareCpu => "CPU (SW)",
        }
    }
}

impl fmt::Display for EncoderPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// Configuración de video del encoder H.264.
#[derive(Debug, Clone, Copy)]
pub struct VideoConfig {
    pub resolution: Resolution,
    pub fps: FrameRate,
    pub quality: QualityPreset,
    /// Bitrate promedio objetivo en bits por segundo.
    pub bitrate_bps: u32,
    /// Intervalo de keyframes (IDR) en segundos (2 s es ideal para replay buffer).
    pub gop_secs: u32,
    /// Preferencia de encoder (Auto GPU / Solo GPU / CPU Software).
    pub encoder_pref: EncoderPreference,
}

impl VideoConfig {
    #[allow(dead_code)]
    pub const fn preset(resolution: Resolution, fps: FrameRate) -> Self {
        Self::preset_with_quality(resolution, fps, QualityPreset::Balanced)
    }

    #[allow(dead_code)]
    pub const fn preset_with_quality(
        resolution: Resolution,
        fps: FrameRate,
        quality: QualityPreset,
    ) -> Self {
        Self::preset_full(resolution, fps, quality, EncoderPreference::HardwareGpu)
    }

    pub const fn preset_full(
        resolution: Resolution,
        fps: FrameRate,
        quality: QualityPreset,
        encoder_pref: EncoderPreference,
    ) -> Self {
        let bitrate_bps = Self::recommended_bitrate_for(resolution, fps, quality);
        Self {
            resolution,
            fps,
            quality,
            bitrate_bps,
            gop_secs: 2,
            encoder_pref,
        }
    }

    /// Bitrate equilibrado para clips de juegos en H.264.
    pub const fn recommended_bitrate(resolution: Resolution, fps: FrameRate) -> u32 {
        match (resolution, fps) {
            (Resolution::P720, FrameRate::Fps30) => 4_000_000,   // 4 Mbps  (~15 MB / 30s)
            (Resolution::P720, FrameRate::Fps60) => 6_000_000,   // 6 Mbps  (~22.5 MB / 30s)
            (Resolution::P1080, FrameRate::Fps30) => 7_000_000,  // 7 Mbps  (~26 MB / 30s)
            (Resolution::P1080, FrameRate::Fps60) => 10_000_000, // 10 Mbps (~37.5 MB / 30s)
            (Resolution::P1440, FrameRate::Fps30) => 12_000_000, // 12 Mbps (~45 MB / 30s)
            (Resolution::P1440, FrameRate::Fps60) => 18_000_000, // 18 Mbps (~67.5 MB / 30s)
        }
    }

    pub const fn recommended_bitrate_for(
        resolution: Resolution,
        fps: FrameRate,
        quality: QualityPreset,
    ) -> u32 {
        let base = Self::recommended_bitrate(resolution, fps) as u64;
        ((base * quality.bitrate_scale_pct() as u64) / 100) as u32
    }

    /// Calidad objetivo QVBR (escala 0..=100 de `CODECAPI_AVEncCommonQuality`).
    pub const fn qvbr_quality_level(&self) -> u32 {
        match self.quality {
            QualityPreset::Compact => 62,
            QualityPreset::Balanced => 74,
            QualityPreset::High => 84,
        }
    }

    pub const fn gop_frames(&self) -> u32 {
        self.fps.as_u32() * self.gop_secs
    }

    /// Intervalo objetivo entre keyframes en ticks de 100 ns.
    pub const fn gop_duration_100ns(&self) -> i64 {
        self.gop_secs as i64 * 10_000_000
    }
}

/// Duraciones preestablecidas del buffer circular de Instant Replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayDuration {
    Sec15,
    Sec30,
    Sec60,
    Sec120,
}

impl ReplayDuration {
    pub const fn as_secs(self) -> u32 {
        match self {
            ReplayDuration::Sec15 => 15,
            ReplayDuration::Sec30 => 30,
            ReplayDuration::Sec60 => 60,
            ReplayDuration::Sec120 => 120,
        }
    }
}

impl fmt::Display for ReplayDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.as_secs())
    }
}

/// Fuente de audio del juego/sistema para captura WASAPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// Captura el audio de salida del sistema (ideal cuando se graba el monitor completo).
    SystemLoopback,
    /// Captura EXCLUSIVAMENTE el audio de un proceso específico (Windows 10 2004+),
    /// excluyendo Discord, música o notificaciones.
    ProcessLoopback { pid: u32, include_tree: bool },
}

/// Configuración de captura y codificación de audio (48 kHz estéreo AAC-LC).
#[derive(Debug, Clone, Copy)]
pub struct AudioConfig {
    pub source: AudioSource,
    pub sample_rate: u32,
    pub channels: u16,
    /// Bitrate AAC en bps (valores válidos para MF AAC MFT: 96k, 128k, 160k, 192k).
    pub bitrate_bps: u32,
    /// Ganancia del audio del juego/sistema (1.0 = 100%).
    pub game_volume: f32,
    /// Incluir micrófono mezclado en la pista de audio.
    pub mic_enabled: bool,
    /// Ganancia del micrófono (1.0 = 100%).
    pub mic_volume: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            source: AudioSource::SystemLoopback,
            sample_rate: 48_000,
            channels: 2,
            bitrate_bps: 192_000,
            game_volume: 1.0,
            mic_enabled: true,
            mic_volume: 1.0,
        }
    }
}

/// Modo de activación del Replay Buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureMode {
    /// Inicia el Replay Buffer automáticamente al abrir un juego y lo pausa al cerrarlo (0% GPU en escritorio).
    AutoDetectGames,
    /// Graba automáticamente y de forma continua el monitor seleccionado (pausable manualmente con F5 / bandeja).
    ManualMonitor,
}

/// Modo de captura de audio cuando se graba un juego.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCaptureMode {
    /// Captura únicamente el audio del juego detectado (excluye Discord, música, navegador).
    GameOnly,
    /// Captura todo el audio del escritorio (juego + Discord + música + sistema).
    FullDesktop,
}

/// Esquina de la pantalla donde se dibuja el recuadro PiP de la cámara en clips 16:9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CameraCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

impl CameraCorner {
    pub const ALL: [CameraCorner; 4] = [
        CameraCorner::TopLeft,
        CameraCorner::TopRight,
        CameraCorner::BottomLeft,
        CameraCorner::BottomRight,
    ];

    pub const fn arrow_label(self) -> &'static str {
        match self {
            CameraCorner::TopLeft => "↖",
            CameraCorner::TopRight => "↗",
            CameraCorner::BottomLeft => "↙",
            CameraCorner::BottomRight => "↘",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            CameraCorner::TopLeft => "Superior izquierda",
            CameraCorner::TopRight => "Superior derecha",
            CameraCorner::BottomLeft => "Inferior izquierda",
            CameraCorner::BottomRight => "Inferior derecha",
        }
    }
}

/// Tamaño relativo del recuadro PiP de la cámara (porcentaje del ancho del video).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CameraSize {
    /// ~18% del ancho (ej. 346×194 en 1080p).
    Small,
    /// ~24% del ancho (ej. 460×258 en 1080p).
    #[default]
    Medium,
    /// ~30% del ancho (ej. 576×324 en 1080p).
    Large,
}

impl CameraSize {
    pub const ALL: [CameraSize; 3] = [CameraSize::Small, CameraSize::Medium, CameraSize::Large];

    pub const fn width_fraction(self) -> f64 {
        match self {
            CameraSize::Small => 0.18,
            CameraSize::Medium => 0.24,
            CameraSize::Large => 0.30,
        }
    }

    pub const fn short_label(self) -> &'static str {
        match self {
            CameraSize::Small => "S",
            CameraSize::Medium => "M",
            CameraSize::Large => "L",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            CameraSize::Small => "Chico (18% ancho)",
            CameraSize::Medium => "Mediano (24% ancho)",
            CameraSize::Large => "Grande (30% ancho)",
        }
    }
}

/// Preset de resolución y cuadros por segundo para la cámara (ej. `720p30`, `720p60`, `1080p30`, `1080p60`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CameraPreset {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl CameraPreset {
    pub fn label(self) -> String {
        format!("{}p{}", self.height, self.fps)
    }
}

/// Configuración resuelta de la fuente de cámara para la sesión activa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraConfig {
    pub enabled: bool,
    pub device_id: Option<String>,
    pub preset: Option<CameraPreset>,
    pub corner: CameraCorner,
    pub size: CameraSize,
    pub mirror: bool,
}

/// Tema retro seleccionable para la interfaz de consola CRT de Bals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UiTheme {
    /// Paleta oficial inspirada en el logo de Bals: carcasa crema retro + CRT fósforo verde.
    #[default]
    CrtPhosphorGreen,
    /// Monitor monocromático ámbar clásico de los 80s.
    CrtAmber80s,
    /// Consola clásica gris con acentos rojo/índigo estilo SNES/Famicom.
    ClassicConsole,
    /// Gabinete arcade negro con fósforo cian/esmeralda neón.
    ArcadeNeon,
}

impl UiTheme {
    pub const ALL: [UiTheme; 4] = [
        UiTheme::CrtPhosphorGreen,
        UiTheme::CrtAmber80s,
        UiTheme::ClassicConsole,
        UiTheme::ArcadeNeon,
    ];

    pub const fn short_label(self) -> &'static str {
        match self {
            UiTheme::CrtPhosphorGreen => "VERDE",
            UiTheme::CrtAmber80s => "ÁMBAR",
            UiTheme::ClassicConsole => "CONSOLA",
            UiTheme::ArcadeNeon => "ARCADE",
        }
    }
}

impl fmt::Display for UiTheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short_label())
    }
}

/// Configuración persistente de Bals guardada en `%APPDATA%\Bals\config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub resolution: Resolution,
    pub fps: FrameRate,
    pub quality: QualityPreset,
    pub encoder_pref: EncoderPreference,
    pub replay_duration: ReplayDuration,
    /// Atajo global para guardar los últimos N segundos (por defecto `\`).
    pub save_clip_hotkey: String,
    pub capture_mode: CaptureMode,
    /// Nombre de dispositivo Windows del monitor seleccionado en modo Monitor (ej. `\\.\DISPLAY1`).
    /// `None` selecciona automáticamente el monitor principal.
    pub selected_monitor: Option<String>,
    pub audio_capture_mode: AudioCaptureMode,
    pub mic_enabled: bool,
    pub game_volume: f32,
    pub mic_volume: f32,
    pub capture_cursor: bool,
    /// Estirar resoluciones más angostas que 16:9 (ej. 4:3, 5:4, 16:10) para clipearlas
    /// exactamente como las ve quien juega estirado, sin barras negras laterales.
    pub stretch_aspect_ratio: bool,
    /// Activar superposición de cámara (webcam PiP) en los clips.
    pub camera_enabled: bool,
    /// Identificador estable (symbolic link Media Foundation) de la cámara elegida;
    /// `None` usa la primera cámara detectada.
    pub selected_camera: Option<String>,
    /// Resolución y FPS elegidos para la cámara (`None` = automático ~720p30 o mejor disponible).
    pub camera_preset: Option<CameraPreset>,
    /// Esquina donde se ubica la cámara en el video 16:9.
    pub camera_corner: CameraCorner,
    /// Tamaño del recuadro de la cámara.
    pub camera_size: CameraSize,
    /// Invertir horizontalmente (efecto espejo) la imagen de la cámara.
    pub camera_mirror: bool,
    pub sound_feedback: bool,
    /// Mostrar notificaciones de Windows al guardar un clip o avisar de errores.
    pub show_notifications: bool,
    /// Comprobar e instalar automáticamente nuevas versiones desde GitHub Releases al iniciar.
    pub auto_update: bool,
    /// Tema visual retro de la interfaz.
    pub ui_theme: UiTheme,
    /// Activar efecto sutil de líneas de barrido CRT en la pantalla interior.
    pub crt_scanlines: bool,
    /// Tope de seguridad de RAM para el buffer circular en MB (protege PCs con poca RAM).
    pub max_buffer_ram_mb: usize,
    pub clips_dir: PathBuf,
    /// Lista opcional de ejecutables (ej. `["minecraft.exe", "valorant.exe"]`) para detectar incluso en modo ventana.
    pub extra_game_exes: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::P1080,
            fps: FrameRate::Fps60,
            quality: QualityPreset::Balanced,
            encoder_pref: EncoderPreference::HardwareGpu,
            replay_duration: ReplayDuration::Sec30,
            save_clip_hotkey: "\\".to_string(),
            capture_mode: CaptureMode::AutoDetectGames,
            selected_monitor: None,
            audio_capture_mode: AudioCaptureMode::FullDesktop,
            mic_enabled: true,
            game_volume: 1.0,
            mic_volume: 1.0,
            capture_cursor: true,
            stretch_aspect_ratio: true,
            camera_enabled: false,
            selected_camera: None,
            camera_preset: None,
            camera_corner: CameraCorner::BottomRight,
            camera_size: CameraSize::Medium,
            camera_mirror: true,
            sound_feedback: true,
            show_notifications: true,
            auto_update: true,
            ui_theme: UiTheme::CrtPhosphorGreen,
            crt_scanlines: true,
            max_buffer_ram_mb: 160,
            clips_dir: default_clips_dir(),
            extra_game_exes: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn video_config(&self) -> VideoConfig {
        VideoConfig::preset_full(
            self.resolution,
            self.fps,
            self.quality,
            self.encoder_pref,
        )
    }

    pub fn audio_config(&self, source: AudioSource) -> AudioConfig {
        AudioConfig {
            source,
            mic_enabled: self.mic_enabled,
            game_volume: self.game_volume,
            mic_volume: self.mic_volume,
            ..AudioConfig::default()
        }
    }

    pub fn camera_config(&self) -> CameraConfig {
        CameraConfig {
            enabled: self.camera_enabled,
            device_id: self.selected_camera.clone(),
            preset: self.camera_preset,
            corner: self.camera_corner,
            size: self.camera_size,
            mirror: self.camera_mirror,
        }
    }

    pub fn config_path() -> PathBuf {
        if let Ok(appdata) = std::env::var("APPDATA") {
            PathBuf::from(appdata).join("Bals").join("config.toml")
        } else {
            PathBuf::from("bals_config.toml")
        }
    }

    /// Selecciona automáticamente el encoder más coherente según el hardware detectado
    /// (GPU por hardware si está disponible; CPU software en caso contrario).
    pub fn recommended_encoder_pref() -> EncoderPreference {
        let detected = crate::encode::h264_mft::detect_system_encoders();
        if detected.hw_name.is_some() {
            EncoderPreference::HardwareGpu
        } else {
            EncoderPreference::SoftwareCpu
        }
    }

    pub fn load_or_create() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| toml::from_str::<AppConfig>(&s).ok().map(|cfg| (s, cfg)))
            {
                Some((raw, mut cfg)) => {
                    // Migrar configuraciones heredadas que tenían `encoder_pref = "AutoGpu"` o sin campo.
                    let is_legacy_auto = !raw.contains("encoder_pref")
                        || raw.contains("\"AutoGpu\"")
                        || raw.contains("'AutoGpu'");
                    if is_legacy_auto {
                        cfg.encoder_pref = Self::recommended_encoder_pref();
                    }
                    info!(path = %path.display(), "Configuración cargada");
                    return cfg;
                }
                None => {
                    warn!(path = %path.display(), "config.toml inválido; usando valores por defecto");
                }
            }
        }

        let mut cfg = Self::default();
        cfg.encoder_pref = Self::recommended_encoder_pref();
        if let Err(err) = cfg.save() {
            warn!("No se pudo escribir config.toml inicial: {err:#}");
        }
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("No se pudo crear {}", parent.display()))?;
        }
        let content = toml::to_string_pretty(self).context("Error serializando config.toml")?;
        write_atomic(&path, content.as_bytes())
            .with_context(|| format!("No se pudo guardar {}", path.display()))?;
        Ok(())
    }
}

/// Escritura atómica vía archivo temporal + `rename` para evitar lecturas parciales en carrera
/// cuando `--ui` guarda `config.toml` y el daemon en bandeja lo recarga al instante.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, bytes)?;
    if std::fs::rename(&tmp, path).is_err() {
        // En Windows, si el destino existe y rename falla transitoriamente, reintentamos o caemos a write.
        let _ = std::fs::remove_file(path);
        if std::fs::rename(&tmp, path).is_err() {
            std::fs::write(path, bytes)?;
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok(())
}

fn default_clips_dir() -> PathBuf {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        PathBuf::from(profile).join("Videos").join("Bals")
    } else {
        PathBuf::from("clips")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_legacy_config_with_removed_video_codec_field() {
        let legacy_toml = r#"
            resolution = "P1080"
            fps = "Fps60"
            quality = "Balanced"
            video_codec = "Hevc"
            capture_mode = "ManualMonitor"
        "#;
        let cfg: AppConfig = toml::from_str(legacy_toml).expect("debería ignorar video_codec");
        assert_eq!(cfg.capture_mode, CaptureMode::ManualMonitor);
        assert_eq!(cfg.selected_monitor, None);
        assert!(cfg.stretch_aspect_ratio);
        assert!(!cfg.camera_enabled);
        assert_eq!(cfg.selected_camera, None);
        assert_eq!(cfg.camera_preset, None);
        assert_eq!(cfg.camera_corner, CameraCorner::BottomRight);
        assert_eq!(cfg.camera_size, CameraSize::Medium);
        assert!(cfg.camera_mirror);
    }

    #[test]
    fn migrates_legacy_auto_gpu_encoder_preference() {
        let legacy_auto = r#"
            encoder_pref = "AutoGpu"
        "#;
        let cfg: AppConfig = toml::from_str(legacy_auto).expect("debería aceptar alias AutoGpu");
        assert_eq!(cfg.encoder_pref, EncoderPreference::HardwareGpu);

        let sw_cfg: AppConfig = toml::from_str("encoder_pref = \"SoftwareCpu\"").unwrap();
        assert_eq!(sw_cfg.encoder_pref, EncoderPreference::SoftwareCpu);
    }
}
