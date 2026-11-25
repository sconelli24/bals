<p align="center">
  <img src="assets/bals.png" width="128" alt="Bals" />
</p>

# Bals

Cliper de videojuegos ultra-liviano para **Windows 10/11**, escrito en **Rust** con captura **Zero-Copy en GPU** y codificación nativa por hardware (**NVENC / AMD AMF / Intel QSV**), pensado para PCs de bajos recursos.

## Características principales

- **Pipeline 100% Zero-Copy en GPU:** Captura mediante **Windows Graphics Capture (WGC)** (seguro con anti-cheats), escalado y conversión `BGRA → NV12` en VRAM con `ID3D11VideoProcessor`, y codificación H.264 por hardware mediante **Windows Media Foundation MFT** (con fallback a software).
- **Resoluciones y FPS:** `720p`, `1080p` y `2K (1440p)` a `30 fps` y `60 fps`.
- **Replay Buffer en RAM (0 MB/s en disco mientras juegas):** Guarda en memoria los últimos `15 s`, `30 s`, `60 s` o `120 s` ya comprimidos en H.264 + AAC y escribe el `.mp4` en ~12 ms al pulsar tu atajo global (por defecto **`\`**).
- **Audio WASAPI 48 kHz AAC-LC:** Captura el audio exclusivo del juego por proceso (`ProcessLoopback` en Windows 10 2004+) o del sistema + micrófono opcional con volúmenes independientes.
- **Detección automática de juegos + Modo manual:** Consume **0% GPU** en el escritorio; al abrir un juego en pantalla completa (o en tu lista de ejecutables) inicia el Replay Buffer automáticamente.
- **Interfaz liviana bajo demanda (`eframe` + `glow`):** Al cerrar la ventana, el proceso `--ui` finaliza y libera el **100% de su RAM y contexto gráfico**, quedando solo el icono nativo en la bandeja del sistema.
- **Recortador MP4 sin pérdida:** Recorta tus clips desde la pestaña *Clips y Recortador* en milisegundos y sin recodificar (`IMFSourceReader` → `IMFSinkWriter` en passthrough).

## Instalación y Uso

- **Instalador Windows (Recomendado):** Descarga `Bals-Setup-vX.Y.Z.exe` desde [Releases](https://github.com/sconelli24/bals/releases/latest). Instala en `%LOCALAPPDATA%\Programs\Bals` sin pedir permisos de administrador, crea acceso directo en el **Menú de Inicio** y ofrece arranque automático con Windows.
- **Versión portable:** Descarga `bals.exe` y ejecútalo directamente:
  ```powershell
  .\bals.exe
  ```
- **Iniciar silenciosamente en la bandeja (ideal para Inicio de Windows):**
  ```powershell
  .\bals.exe --tray
  ```
- **Abrir solo la ventana de configuración y recortador:**
  ```powershell
  .\bals.exe --ui
  ```
- **Prueba rápida por CLI (ej. 1080p 60fps durante 6 segundos):**
  ```powershell
  .\bals.exe --test 1080p 60 6
  ```
- **Medir el rendimiento y buscar fugas (ej. 10 minutos, un clip por minuto, con Bals cerrado):**
  ```powershell
  .\bals.exe --soak 10 C:\bals-perf monitor 60 > C:\bals-perf.txt
  ```
  Graba el monitor con el motor real, guarda un clip cada 60 s y se mide cada segundo: CPU, RAM, VRAM, cada motor de la
  GPU, handles, hilos, cuadros codificados y salteados, trabajo por tick y tiempos al guardar. Al terminar deja en la
  carpeta `informe.md`, `informe.toml` (veredicto, avisos y fugas) y `perf.csv` (una muestra por segundo). No toca tu
  `config.toml` ni tu carpeta de clips. Escenarios: `monitor`, `mic`, `cpu`, `30`, `ahorro`, `alta`, `sincursor`,
  `juegos`, `720`, `1080`, `1440`. Con Claude Code, pídele "mide el rendimiento de Bals y busca fugas": usa la skill
  `.claude/skills/rendimiento`.

## Configuración

Los ajustes se guardan automáticamente en `%APPDATA%\Bals\config.toml` y los clips en `%USERPROFILE%\Videos\Bals` (ambos configurables desde la interfaz).

## Actualizaciones automáticas y Releases

- **CI/CD automático:** Cada vez que incrementas `version` en `Cargo.toml` (por ejemplo `0.1.0` → `0.1.1`) y haces `git push origin main`, GitHub Actions ejecuta los tests, compila `bals.exe` en `--release` y publica automáticamente el GitHub Release `vX.Y.Z` con `bals.exe`, `.zip` y checksums SHA-256.
- **Auto-updater integrado:** Al iniciar, `bals.exe` consulta el último Release en `https://github.com/sconelli24/bals` usando WinHTTP nativo de Windows (sin dependencias externas), descarga la nueva versión si existe, reemplaza el binario y se reinicia automáticamente. También puedes desactivarlo o buscar actualizaciones manualmente desde **Ajustes**.
