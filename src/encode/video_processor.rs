use anyhow::{Context, Result};
use tracing::{debug, warn};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CAPS,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use crate::config::{CameraCorner, CameraSize};
use crate::d3d::SharedD3DDevice;

const NV12_POOL_SIZE: usize = 12;

/// Datos de textura y posición en GPU para superponer la cámara (PiP) en el mismo pase `VideoProcessorBlt`.
pub struct CameraOverlay<'a> {
    pub texture: &'a ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub corner: CameraCorner,
    pub size: CameraSize,
}

struct Nv12Slot {
    texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
}

/// Conversor, escalador y compositor multi-stream 100% en GPU (`BGRA8` → `NV12`) usando `ID3D11VideoProcessor`.
///
/// - Estira resoluciones más angostas que 16:9 (ej. `4:3`, `5:4`, `16:10`) a pantalla completa
///   cuando `stretch_narrow == true`, replicando cómo se ve un juego estirado al jugar.
/// - Compone opcionalmente un segundo stream (`CameraOverlay`) en hardware manteniendo la proporción
///   natural de la webcam sin estirarla.
/// - Mantiene un pool circular de texturas `NV12` en VRAM para evitar asignaciones por frame.
pub struct GpuNv12Converter {
    d3d: SharedD3DDevice,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_width: u32,
    input_height: u32,
    content_max_w: u32,
    content_max_h: u32,
    output_width: u32,
    output_height: u32,
    fps: u32,
    stretch_narrow: bool,
    supports_multi_stream: bool,
    pool: Vec<Nv12Slot>,
    next_slot: usize,
}

impl GpuNv12Converter {
    pub fn new(
        d3d: &SharedD3DDevice,
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
        fps: u32,
        stretch_narrow: bool,
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = d3d
            .device
            .cast()
            .context("La GPU/driver no expone ID3D11VideoDevice")?;
        let video_context: ID3D11VideoContext = d3d
            .context
            .cast()
            .context("La GPU/driver no expone ID3D11VideoContext")?;

        let content_max_w = input_width.max(1280);
        let content_max_h = input_height.max(720);

        let (enumerator, processor, supports_multi_stream) = create_processor(
            &video_device,
            &video_context,
            input_width,
            input_height,
            content_max_w,
            content_max_h,
            output_width,
            output_height,
            fps,
            stretch_narrow,
        )?;

        let pool = create_nv12_pool(d3d, &video_device, &enumerator, output_width, output_height)?;

        debug!(
            in_w = input_width,
            in_h = input_height,
            out_w = output_width,
            out_h = output_height,
            stretch_narrow,
            supports_multi_stream,
            pool_size = pool.len(),
            "GpuNv12Converter inicializado (BGRA -> NV12 + escalado/compositor en VRAM)"
        );

        Ok(Self {
            d3d: d3d.clone(),
            video_device,
            video_context,
            enumerator,
            processor,
            input_width,
            input_height,
            content_max_w,
            content_max_h,
            output_width,
            output_height,
            fps,
            stretch_narrow,
            supports_multi_stream,
            pool,
            next_slot: 0,
        })
    }

    /// Cambia en caliente el modo de estirado 4:3 sin recrear el pool NV12.
    pub fn set_stretch_narrow(&mut self, stretch_narrow: bool) {
        if self.stretch_narrow == stretch_narrow {
            return;
        }
        self.stretch_narrow = stretch_narrow;
        let dst_rect = compute_game_dest_rect(
            self.input_width,
            self.input_height,
            self.output_width,
            self.output_height,
            self.stretch_narrow,
        );
        unsafe {
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&dst_rect),
            );
        }
    }

    /// Convierte y escala una textura `BGRA` en GPU a una textura `NV12` del pool (sin cámara).
    pub fn convert(
        &mut self,
        bgra_texture: &ID3D11Texture2D,
        src_width: u32,
        src_height: u32,
    ) -> Result<ID3D11Texture2D> {
        self.convert_with_camera(bgra_texture, src_width, src_height, None)
    }

    /// Convierte y escala la textura del juego (`BGRA`) y, opcionalmente, compone la cámara PiP
    /// (`CameraOverlay`) en un único pase de hardware `VideoProcessorBlt` → `NV12` (Zero-Copy).
    pub fn convert_with_camera(
        &mut self,
        bgra_texture: &ID3D11Texture2D,
        src_width: u32,
        src_height: u32,
        camera: Option<CameraOverlay<'_>>,
    ) -> Result<ID3D11Texture2D> {
        let req_max_w = src_width.max(camera.as_ref().map_or(0, |c| c.width)).max(1280);
        let req_max_h = src_height.max(camera.as_ref().map_or(0, |c| c.height)).max(720);

        // Si cambió el tamaño de entrada (p. ej. cambio a 4:3 o transición de ventana)
        // o la cámara supera los límites actuales del enumerador, recreamos el procesador.
        if src_width != self.input_width
            || src_height != self.input_height
            || req_max_w > self.content_max_w
            || req_max_h > self.content_max_h
        {
            let new_max_w = self.content_max_w.max(req_max_w);
            let new_max_h = self.content_max_h.max(req_max_h);
            let (enumerator, processor, supports_multi_stream) = create_processor(
                &self.video_device,
                &self.video_context,
                src_width,
                src_height,
                new_max_w,
                new_max_h,
                self.output_width,
                self.output_height,
                self.fps,
                self.stretch_narrow,
            )?;
            self.pool = create_nv12_pool(
                &self.d3d,
                &self.video_device,
                &enumerator,
                self.output_width,
                self.output_height,
            )?;
            self.enumerator = enumerator;
            self.processor = processor;
            self.input_width = src_width;
            self.input_height = src_height;
            self.content_max_w = new_max_w;
            self.content_max_h = new_max_h;
            self.supports_multi_stream = self.supports_multi_stream && supports_multi_stream;
            self.next_slot = 0;
        }

        let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };

        let mut game_input_view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                bgra_texture,
                &self.enumerator,
                &in_view_desc,
                Some(&mut game_input_view),
            )
        }
        .context("CreateVideoProcessorInputView (juego) falló")?;

        // Preparar Stream 1 (cámara PiP) si está presente y el driver soporta multi-stream.
        let mut cam_input_view = None;
        if let Some(cam) = camera.as_ref().filter(|_| self.supports_multi_stream) {
            if cam.width >= 32 && cam.height >= 32 {
                match unsafe {
                    self.video_device.CreateVideoProcessorInputView(
                        cam.texture,
                        &self.enumerator,
                        &in_view_desc,
                        Some(&mut cam_input_view),
                    )
                } {
                    Ok(()) => {
                        let cam_src = RECT {
                            left: 0,
                            top: 0,
                            right: cam.width as i32,
                            bottom: cam.height as i32,
                        };
                        let cam_dst = compute_camera_pip_rect(
                            cam.width,
                            cam.height,
                            self.output_width,
                            self.output_height,
                            cam.corner,
                            cam.size,
                        );
                        let mut cam_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE::default();
                        cam_cs._bitfield = 0; // RGB Full 0-255
                        unsafe {
                            self.video_context.VideoProcessorSetStreamFrameFormat(
                                &self.processor,
                                1,
                                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                            );
                            self.video_context.VideoProcessorSetStreamSourceRect(
                                &self.processor,
                                1,
                                true,
                                Some(&cam_src),
                            );
                            self.video_context.VideoProcessorSetStreamDestRect(
                                &self.processor,
                                1,
                                true,
                                Some(&cam_dst),
                            );
                            self.video_context.VideoProcessorSetStreamColorSpace(
                                &self.processor,
                                1,
                                &cam_cs,
                            );
                        }
                    }
                    Err(err) => {
                        warn!(
                            "CreateVideoProcessorInputView para cámara falló ({err}); continuando solo con el juego"
                        );
                        self.supports_multi_stream = false;
                        cam_input_view = None;
                    }
                }
            }
        }

        let slot = &self.pool[self.next_slot];
        self.next_slot = (self.next_slot + 1) % self.pool.len();

        let has_cam = cam_input_view.is_some();
        let mut streams = [
            make_vp_stream(game_input_view),
            make_vp_stream(cam_input_view),
        ];

        let blt_res = if has_cam {
            let res = unsafe {
                self.video_context
                    .VideoProcessorBlt(&self.processor, &slot.output_view, 0, &streams[..2])
            };
            match res {
                Ok(()) => Ok(()),
                Err(err) => {
                    warn!(
                        "VideoProcessorBlt multi-stream falló en este driver ({err}); desactivando PiP HW y grabando solo juego"
                    );
                    self.supports_multi_stream = false;
                    unsafe {
                        self.video_context.VideoProcessorBlt(
                            &self.processor,
                            &slot.output_view,
                            0,
                            &streams[..1],
                        )
                    }
                }
            }
        } else {
            unsafe {
                self.video_context
                    .VideoProcessorBlt(&self.processor, &slot.output_view, 0, &streams[..1])
            }
        };

        // Liberar explícitamente los InputViews contenidos en ManuallyDrop para evitar fugas COM/VRAM.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            std::mem::ManuallyDrop::drop(&mut streams[1].pInputSurface);
        }

        blt_res.context("VideoProcessorBlt (BGRA -> NV12) falló")?;

        // Enviar inmediatamente la cola de comandos D3D11 a la GPU para garantizar que la escritura
        // en la textura NV12 quede encolada ANTES de que el encoder hardware (VCE/NVENC/QSV) la lea.
        unsafe {
            self.d3d.context.Flush();
        }

        Ok(slot.texture.clone())
    }
}

fn make_vp_stream(
    input_view: Option<windows::Win32::Graphics::Direct3D11::ID3D11VideoProcessorInputView>,
) -> D3D11_VIDEO_PROCESSOR_STREAM {
    let enabled = input_view.is_some();
    D3D11_VIDEO_PROCESSOR_STREAM {
        Enable: enabled.into(),
        OutputIndex: 0,
        InputFrameOrField: 0,
        PastFrames: 0,
        FutureFrames: 0,
        ppPastSurfaces: std::ptr::null_mut(),
        pInputSurface: std::mem::ManuallyDrop::new(input_view),
        ppFutureSurfaces: std::ptr::null_mut(),
        ppPastSurfacesRight: std::ptr::null_mut(),
        pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
        ppFutureSurfacesRight: std::ptr::null_mut(),
    }
}

#[allow(clippy::too_many_arguments)]
fn create_processor(
    video_device: &ID3D11VideoDevice,
    video_context: &ID3D11VideoContext,
    input_width: u32,
    input_height: u32,
    content_max_w: u32,
    content_max_h: u32,
    output_width: u32,
    output_height: u32,
    fps: u32,
    stretch_narrow: bool,
) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor, bool)> {
    let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        },
        InputWidth: content_max_w.max(input_width),
        InputHeight: content_max_h.max(input_height),
        OutputFrameRate: DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        },
        OutputWidth: output_width,
        OutputHeight: output_height,
        Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    };

    let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
        .context("CreateVideoProcessorEnumerator falló")?;
    let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
    let supports_multi_stream = if unsafe { enumerator.GetVideoProcessorCaps(&mut caps) }.is_ok() {
        caps.MaxInputStreams >= 2 && caps.MaxStreamStates >= 2
    } else {
        true
    };

    let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
        .context("CreateVideoProcessor falló")?;

    let src_rect = RECT {
        left: 0,
        top: 0,
        right: input_width as i32,
        bottom: input_height as i32,
    };
    let dst_rect = compute_game_dest_rect(
        input_width,
        input_height,
        output_width,
        output_height,
        stretch_narrow,
    );

    // Configurar espacio de color: entrada RGB rango completo (0-255) → salida YCbCr Rec.709 (HD/2K).
    let mut in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE::default();
    in_cs._bitfield = 0; // Usage=0 (Playback), RGB_Range=0 (Full 0-255)

    let mut out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE::default();
    // Bit 2 = YCbCr_Matrix (1 = BT.709), Nominal_Range = Studio/Video estándar H.264
    out_cs._bitfield = 1 << 2;

    let black_bg = D3D11_VIDEO_COLOR {
        Anonymous: D3D11_VIDEO_COLOR_0 {
            RGBA: D3D11_VIDEO_COLOR_RGBA {
                R: 0.0,
                G: 0.0,
                B: 0.0,
                A: 1.0,
            },
        },
    };

    unsafe {
        video_context.VideoProcessorSetStreamFrameFormat(
            &processor,
            0,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        );
        video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src_rect));
        video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst_rect));
        video_context.VideoProcessorSetOutputTargetRect(
            &processor,
            true,
            Some(&RECT {
                left: 0,
                top: 0,
                right: output_width as i32,
                bottom: output_height as i32,
            }),
        );
        video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black_bg);
        video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &in_cs);
        video_context.VideoProcessorSetOutputColorSpace(&processor, &out_cs);
    }

    Ok((enumerator, processor, supports_multi_stream))
}

fn create_nv12_pool(
    d3d: &SharedD3DDevice,
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    width: u32,
    height: u32,
) -> Result<Vec<Nv12Slot>> {
    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };

    let mut pool = Vec::with_capacity(NV12_POOL_SIZE);
    for _ in 0..NV12_POOL_SIZE {
        let mut texture = None;
        unsafe { d3d.device.CreateTexture2D(&tex_desc, None, Some(&mut texture)) }
            .context("No se pudo crear textura NV12 en VRAM")?;
        let texture = texture.context("CreateTexture2D devolvió None")?;

        let mut output_view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &texture,
                enumerator,
                &out_view_desc,
                Some(&mut output_view),
            )
        }
        .context("CreateVideoProcessorOutputView falló para NV12")?;
        let output_view = output_view.context("CreateVideoProcessorOutputView devolvió None")?;

        pool.push(Nv12Slot {
            texture,
            output_view,
        });
    }

    Ok(pool)
}

/// Calcula el rectángulo destino del juego dentro del frame de salida `16:9`.
///
/// - Si `stretch_narrow == true` y la relación de aspecto de entrada es menor o igual que la de salida
///   (p. ej. `4:3` `1280×960` / `1440×1080`, `5:4` `1280×1024`, `16:10` `1680×1050` o `16:9`),
///   estira la imagen para llenar el `100%` del frame sin barras negras laterales.
/// - Si la entrada es más ancha que `16:9` (monitores ultrawide `21:9` / `32:9`) o `stretch_narrow == false`,
///   preserva la proporción con letterbox limpio para no aplastar la imagen.
pub fn compute_game_dest_rect(
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
    stretch_narrow: bool,
) -> RECT {
    if in_w == 0 || in_h == 0 || out_w == 0 || out_h == 0 {
        return RECT {
            left: 0,
            top: 0,
            right: out_w as i32,
            bottom: out_h as i32,
        };
    }

    let in_lhs = in_w as u64 * out_h as u64;
    let out_rhs = out_w as u64 * in_h as u64;
    if in_lhs == out_rhs || (stretch_narrow && in_lhs < out_rhs) {
        return RECT {
            left: 0,
            top: 0,
            right: out_w as i32,
            bottom: out_h as i32,
        };
    }

    fit_aspect_ratio(in_w, in_h, out_w, out_h)
}

/// Calcula un rectángulo centrado dentro de `(out_w, out_h)` preservando la relación de aspecto.
fn fit_aspect_ratio(in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> RECT {
    if in_w == 0 || in_h == 0 || (in_w as u64 * out_h as u64 == out_w as u64 * in_h as u64) {
        return RECT {
            left: 0,
            top: 0,
            right: out_w as i32,
            bottom: out_h as i32,
        };
    }

    let in_aspect = in_w as f64 / in_h as f64;
    let out_aspect = out_w as f64 / out_h as f64;

    let (w, h) = if in_aspect > out_aspect {
        let w = out_w as f64;
        let h = (w / in_aspect).round();
        (w as i32, h as i32)
    } else {
        let h = out_h as f64;
        let w = (h * in_aspect).round();
        (w as i32, h as i32)
    };

    // Alinear a múltiplos de 2 para submuestreo chroma NV12 (4:2:0).
    let w = w & !1;
    let h = h & !1;
    let left = ((out_w as i32 - w) / 2) & !1;
    let top = ((out_h as i32 - h) / 2) & !1;

    RECT {
        left,
        top,
        right: left + w,
        bottom: top + h,
    }
}

/// Calcula el rectángulo PiP de la webcam en una de las 4 esquinas preservando la relación de aspecto
/// nativa de la cámara (nunca deformada aunque el juego sea 4:3) y alineado a píxeles pares (`NV12`).
pub fn compute_camera_pip_rect(
    cam_w: u32,
    cam_h: u32,
    out_w: u32,
    out_h: u32,
    corner: CameraCorner,
    size: CameraSize,
) -> RECT {
    let safe_cam_w = cam_w.max(1) as f64;
    let safe_cam_h = cam_h.max(1) as f64;
    let cam_aspect = (safe_cam_w / safe_cam_h).clamp(0.5, 3.0);

    // Margen proporcional (~2.2% de la altura: ~24 px en 1080p, ~16 px en 720p).
    let margin = (((out_h as f64 * 0.022).round() as i32) & !1).max(8);

    let max_w = (out_w as i32 - margin * 2).max(64);
    let max_h = (out_h as i32 - margin * 2).max(48);

    let mut pip_w = ((out_w as f64 * size.width_fraction()).round() as i32).clamp(64, max_w);
    let mut pip_h = ((pip_w as f64 / cam_aspect).round() as i32).max(48);

    // Evitar que una cámara vertical/cuadrada exceda el 45% de la altura total.
    let height_cap = ((out_h as f64 * 0.45).round() as i32).min(max_h);
    if pip_h > height_cap {
        pip_h = height_cap;
        pip_w = ((pip_h as f64 * cam_aspect).round() as i32).clamp(64, max_w);
    }

    // Alinear dimensiones y origen a múltiplos de 2 para chroma NV12 (4:2:0).
    pip_w = (pip_w & !1).max(64);
    pip_h = (pip_h & !1).max(48);

    let left = match corner {
        CameraCorner::TopLeft | CameraCorner::BottomLeft => margin,
        CameraCorner::TopRight | CameraCorner::BottomRight => {
            ((out_w as i32 - margin - pip_w) & !1).max(0)
        }
    };
    let top = match corner {
        CameraCorner::TopLeft | CameraCorner::TopRight => margin,
        CameraCorner::BottomLeft | CameraCorner::BottomRight => {
            ((out_h as i32 - margin - pip_h) & !1).max(0)
        }
    };

    RECT {
        left,
        top,
        right: (left + pip_w).min(out_w as i32),
        bottom: (top + pip_h).min(out_h as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretches_narrow_resolutions_and_preserves_ultrawide() {
        // 4:3 (1280×960 y 1440×1080) en 1080p -> estirado completo sin barras negras.
        let r_43 = compute_game_dest_rect(1280, 960, 1920, 1080, true);
        assert_eq!((r_43.left, r_43.top, r_43.right, r_43.bottom), (0, 0, 1920, 1080));

        let r_1440 = compute_game_dest_rect(1440, 1080, 1920, 1080, true);
        assert_eq!((r_1440.left, r_1440.top, r_1440.right, r_1440.bottom), (0, 0, 1920, 1080));

        // 5:4 (1280×1024) en 720p -> estirado completo.
        let r_54 = compute_game_dest_rect(1280, 1024, 1280, 720, true);
        assert_eq!((r_54.left, r_54.top, r_54.right, r_54.bottom), (0, 0, 1280, 720));

        // 4:3 con stretch desactivado -> pillarbox centrado 1440×1080.
        let r_fit = compute_game_dest_rect(1280, 960, 1920, 1080, false);
        assert_eq!((r_fit.left, r_fit.top, r_fit.right, r_fit.bottom), (240, 0, 1680, 1080));

        // Ultrawide 21:9 (2560×1080) en 1080p -> letterbox vertical (no aplasta horizontalmente).
        let r_uw = compute_game_dest_rect(2560, 1080, 1920, 1080, true);
        assert_eq!(r_uw.left, 0);
        assert_eq!(r_uw.right, 1920);
        assert!(r_uw.top > 0 && r_uw.bottom < 1080);
    }

    #[test]
    fn computes_aligned_camera_pip_rects_for_all_corners_and_sizes() {
        for corner in CameraCorner::ALL {
            let mut prev_w = 0;
            for size in CameraSize::ALL {
                let r = compute_camera_pip_rect(1280, 720, 1920, 1080, corner, size);
                let w = r.right - r.left;
                let h = r.bottom - r.top;

                assert!(r.left >= 0 && r.top >= 0 && r.right <= 1920 && r.bottom <= 1080);
                assert_eq!(r.left % 2, 0);
                assert_eq!(r.top % 2, 0);
                assert_eq!(w % 2, 0);
                assert_eq!(h % 2, 0);
                assert!(w > prev_w);
                prev_w = w;

                // Conserva proporción 16:9 de la cámara (~1.77).
                let ratio = w as f64 / h as f64;
                assert!((ratio - 16.0 / 9.0).abs() < 0.05);
            }
        }

        let tl = compute_camera_pip_rect(1280, 720, 1920, 1080, CameraCorner::TopLeft, CameraSize::Medium);
        let br = compute_camera_pip_rect(1280, 720, 1920, 1080, CameraCorner::BottomRight, CameraSize::Medium);
        assert!(tl.left < 60 && tl.top < 60);
        assert!(br.right > 1860 && br.bottom > 1020);
    }

    #[test]
    fn gpu_composites_camera_pip_pixels() {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
            D3D11_SUBRESOURCE_DATA, D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

        let Ok(d3d) = SharedD3DDevice::new() else {
            return;
        };

        let make_solid = |w: u32, h: u32, b: u8, g: u8, r: u8| -> ID3D11Texture2D {
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for px in pixels.chunks_exact_mut(4) {
                px[0] = b;
                px[1] = g;
                px[2] = r;
                px[3] = 255;
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: pixels.as_ptr().cast(),
                SysMemPitch: w * 4,
                SysMemSlicePitch: 0,
            };
            let mut tex = None;
            unsafe { d3d.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }.unwrap();
            tex.unwrap()
        };

        // Juego oscuro (luma baja), cámara blanca brillante (luma alta) para lectura inequívoca en plano Y de NV12.
        let game_tex = make_solid(1280, 960, 20, 20, 20);
        let cam_tex = make_solid(1280, 720, 240, 240, 240);

        let Ok(mut conv) = GpuNv12Converter::new(&d3d, 1280, 960, 1920, 1080, 60, true) else {
            // Entornos CI sin soporte D3D11VideoDevice por hardware.
            return;
        };
        let nv12 = conv
            .convert_with_camera(
                &game_tex,
                1280,
                960,
                Some(CameraOverlay {
                    texture: &cam_tex,
                    width: 1280,
                    height: 720,
                    corner: CameraCorner::BottomRight,
                    size: CameraSize::Medium,
                }),
            )
            .unwrap();

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: 1920,
            Height: 1080,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { d3d.device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }.unwrap();
        let staging = staging.unwrap();
        unsafe {
            d3d.context.CopyResource(&staging, &nv12);
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { d3d.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.unwrap();
        let pitch = mapped.RowPitch as usize;
        let y_plane = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * 1080) };

        // Esquina superior izquierda = juego oscuro; esquina inferior derecha (centro del PiP) = cámara blanca.
        let y_game = y_plane[100 * pitch + 100];
        let pip = compute_camera_pip_rect(1280, 720, 1920, 1080, CameraCorner::BottomRight, CameraSize::Medium);
        let cx = ((pip.left + pip.right) / 2) as usize;
        let cy = ((pip.top + pip.bottom) / 2) as usize;
        let y_cam = y_plane[cy * pitch + cx];

        unsafe { d3d.context.Unmap(&staging, 0) };

        println!(
            "GPU={} multi_stream={} y_game={y_game} y_cam={y_cam} pip=({},{},{},{})",
            d3d.adapter_name, conv.supports_multi_stream, pip.left, pip.top, pip.right, pip.bottom
        );
        assert!(y_game < 80, "el área del juego debería ser oscura, y_game={y_game}");
        assert!(
            y_cam > 180,
            "el área de la cámara PiP debería verse brillante en el NV12, y_cam={y_cam} (multi_stream={})",
            conv.supports_multi_stream
        );
    }
}
