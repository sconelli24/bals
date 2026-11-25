---
name: rendimiento
description: Medir y evaluar el rendimiento del motor de Bals en Windows (CPU, RAM, VRAM, uso de cada motor de la GPU, cuadros perdidos, trabajo por tick, tiempos al guardar) y buscar fugas de memoria, handles o hilos en una grabación larga con `bals --soak`. Úsala cuando pidan medir, comparar o vigilar el rendimiento, buscar memory leaks, ver si un cambio mejoró o empeoró algo, o decidir dónde optimizar.
---

# Rendimiento del motor de Bals

`bals --soak` graba el monitor con el motor real (`RecorderEngine`), guarda un clip cada tanto y cada segundo anota lo
que usa. Al final busca fugas y deja un veredicto. Todo es de Windows.

## 1. Preparar

- Compila: `cargo build --release`.
- **Cierra Bals** (bandeja → Salir): `--soak` se niega a correr con el daemon abierto, porque los números se mezclarían.
- Cada corrida va en una carpeta nueva. Los clips quedan ahí, en `clips\`, y no se toca `%APPDATA%\Bals\config.toml`.
- **De a una por vez** y sin otras mediciones al mismo tiempo: los números de CPU y GPU se mezclan.

## 2. Correr

```powershell
$D = "$env:TEMP\bals-perf\$(Get-Date -Format yyyyMMdd-HHmmss)-monitor"
target\release\bals.exe --soak 10 $D monitor 60 > "$D-consola.txt" 2>&1
```

En bash (Git Bash): `target/release/bals.exe --soak 10 "$D" monitor 60 > "$D-consola.txt" 2>&1`.

Los argumentos son `<minutos> <carpeta> [escenario|-] [segundos entre clips]`. Con `-` queda el escenario por defecto
y puedes indicar igual los segundos. El exe de release no tiene consola: la salida se ve redirigiéndola.

- **Duración**: para buscar fugas, al menos 10 minutos. El buffer (30 s) se llena en eso más 15 s y las fugas se miden
  recién después. Para una comparación rápida de consumo alcanzan 3 minutos.
- **Escenarios**: todos graban el monitor principal a 1080p60, calidad Normal, sin micrófono, con el encoder que elige
  Bals, salvo lo que cambie cada uno:
  - `monitor`: así, tal cual.
  - `mic`: con el micrófono.
  - `cpu`: con el encoder por software.
  - `30`: a 30 fps.
  - `ahorro` o `alta`: con esa calidad.
  - `sincursor`: sin dibujar el cursor.
  - `720`, `1080` o `1440`: la resolución del clip.
  - `juegos`: la detección automática de juegos, con un juego abierto.
- **Con movimiento**: con el escritorio quieto, el motor reutiliza el último cuadro y casi no trabaja. Para medir como
  en un juego, abre un juego o un video a pantalla completa mientras corre.

### Vigilar mientras graba

Lanza la corrida en segundo plano y sigue la consola. Cada minuto aparece una línea `ESTADO`, con CPU, RAM, ReplayBuffer,
VRAM, GPU, handles e hilos. Una línea `AVISO` aparece en cuanto algo se sale de lo normal:
- el encoder recibe menos cuadros;
- el motor saltea cuadros;
- un tick tarda más que un cuadro;
- la RAM salta sin guardar un clip.

`tail -f consola.txt | grep --line-buffered -E "ESTADO|AVISO|ERROR|Rendimiento"`. No hace falta más: cada segundo ya
queda en `perf.csv`.

## 3. Leer el resultado

En la carpeta quedan:
- `informe.md`: para leer y mostrar.
- `informe.toml`: los mismos datos, para comparar.
- `perf.csv`: una muestra por segundo.

El exe sale con 0 si todo está bien, con 3 si hay algo para revisar y con 1 si no pudo medir.

- `veredicto` y `avisos` primero.
- **Fugas**: el piso de cada tramo de 30 s (sin los picos de guardar), desde que el buffer está lleno.
  - "posible fuga": sube más de lo tolerado por minuto y en total.
  - "RAM fuera del ReplayBuffer" es la clave. Si sube la RAM privada pero esta no, es el buffer creciendo (por ejemplo,
    un video con más movimiento pesa más), no una fuga.
  - `al_cerrar`: hilos y handles que quedaron después de cerrar el motor.
- `cpu_muestreo_pct` es lo que costó medir (sobre todo los contadores de la GPU). Ya está descontado de `cpu_pct`.
- `tick_ms` es el trabajo de cada tick CFR sin la espera: captura, conversión y encoder. Compáralo con
  `presupuesto_por_cuadro_ms`.
- Las dudas se responden en `perf.csv`: picos alrededor de cada `guardando = 1`, saltos, cuándo empezó a crecer algo.

## 4. Dónde mirar según lo que salga

| Síntoma | Dónde mirar |
|---|---|
| Sube la RAM fuera del ReplayBuffer | Muestras de Media Foundation sin liberar (`encode/h264_mft.rs`, `audio/aac_mft.rs`), hilos `bals-mp4-writer` (`engine.rs`, `muxer/mod.rs`), snapshots que se quedan vivos. |
| Sube la VRAM | Texturas que se recrean: `encode/video_processor.rs` (vistas y NV12 de salida), `capture/wgc.rs` y `capture/dxgi_dup.rs`. |
| Suben los hilos o los handles | `audio/capture.rs` (WASAPI), `game_detect.rs` (procesos abiertos), hilos de guardado. |
| `fps_real` por debajo de lo pedido | Encoder saturado (`encode_nv12_texture`), GPU al 100 %, ticks más largos que un cuadro. |
| Ticks largos o cuadros salteados | Lo que corre en `step_cfr_tick` (`engine.rs`): `take_latest`, `converter.convert`, `encode_nv12_texture`, y `pump_audio`. |
| Motor frenado al guardar | `handle_save_clip` (`engine.rs`): el `snapshot` del `ReplayBuffer` (`buffer/mod.rs`). |
| Mucha CPU con encoder por software | `encode/h264_mft.rs` (lectura de la textura a la CPU y el MFT de software). |

## 5. Comparar contra otra versión

Para saber si un cambio mejoró algo, mide las dos versiones en las mismas condiciones: mismo escenario, mismos minutos,
misma PC y nada más abierto.

1. La versión anterior va en su propio worktree: `git worktree add ..\bals-base <commit>`. Ahí, `cargo build --release`.
   Las versiones anteriores a `--soak` no lo tienen: para esas, mide desde afuera (Administrador de tareas o
   `Get-Counter`).
2. Corre el mismo comando con cada exe, uno después del otro.
3. Compara los `informe.toml` campo por campo y arma una tabla con antes, después y diferencia en %, sobre todo:
   - `cpu_pct.prom`
   - `gpu_pct.prom`
   - `ram_privada_mb.prom`
   - `vram_mb.prom`
   - `fps_real.prom`
   - `tick_ms.p95`
   - `guardado_frenada_ms.max`
4. Una diferencia menor que la que hay entre dos corridas de la misma versión es ruido. Si importa, repite cada corrida
   dos veces.

## 6. Qué contarle al usuario

- El veredicto y los avisos, en palabras: qué pasa y qué se nota al usarlo.
- Los números clave, con su escenario.
- Si hay una fuga: cuánto crece por minuto y cuánto sería en una hora de juego.
- Dónde mejorar: lo que más consume o lo que se acerca al límite, con el archivo y la función, y qué cambiarías.
- Lo que no se pudo medir y por qué.
