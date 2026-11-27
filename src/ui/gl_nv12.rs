use std::sync::{Arc, Mutex};

use eframe::{egui, egui_glow, glow};
use glow::HasContext;
use tracing::{debug, warn};

use crate::ui::player::FrameBufferPool;

const NV12_VERT_SRC: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    vec2 pos;
    vec2 uv;
    if (gl_VertexID == 0) {
        pos = vec2(-1.0, 1.0);
        uv = vec2(0.0, 0.0);
    } else if (gl_VertexID == 1) {
        pos = vec2(1.0, 1.0);
        uv = vec2(1.0, 0.0);
    } else if (gl_VertexID == 2) {
        pos = vec2(-1.0, -1.0);
        uv = vec2(0.0, 1.0);
    } else {
        pos = vec2(1.0, -1.0);
        uv = vec2(1.0, 1.0);
    }
    gl_Position = vec4(pos, 0.0, 1.0);
    v_uv = uv;
}
"#;

const NV12_FRAG_SRC: &str = r#"#version 330 core
in vec2 v_uv;
out vec4 frag_color;
uniform sampler2D u_tex_y;
uniform sampler2D u_tex_uv;
uniform int u_bt709;
uniform int u_full_range;
void main() {
    float y = texture(u_tex_y, v_uv).r;
    vec2 uv = texture(u_tex_uv, v_uv).rg - vec2(0.5);
    if (u_full_range == 0) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        uv *= (255.0 / 224.0);
    }
    vec3 rgb;
    if (u_bt709 != 0) {
        rgb = vec3(
            y + 1.5748 * uv.y,
            y - 0.187324 * uv.x - 0.468124 * uv.y,
            y + 1.8556 * uv.x
        );
    } else {
        rgb = vec3(
            y + 1.402 * uv.y,
            y - 0.344136 * uv.x - 0.714136 * uv.y,
            y + 1.772 * uv.x
        );
    }
    frag_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

struct PendingNv12 {
    width: u32,
    height: u32,
    data: Vec<u8>,
    is_bt709: bool,
    full_range: bool,
}

struct Nv12GlState {
    program: Option<glow::Program>,
    vao: Option<glow::VertexArray>,
    tex_y: Option<glow::Texture>,
    tex_uv: Option<glow::Texture>,
    u_tex_y: Option<glow::UniformLocation>,
    u_tex_uv: Option<glow::UniformLocation>,
    u_bt709: Option<glow::UniformLocation>,
    u_full_range: Option<glow::UniformLocation>,
    tex_w: u32,
    tex_h: u32,
    is_bt709: bool,
    full_range: bool,
    frame_size: Option<(u32, u32)>,
    has_frame: bool,
    pending: Option<PendingNv12>,
}

/// Renderizador OpenGL (`glow`) que sube planos `NV12` (`R8` + `RG8`) y convierte `YUV → RGB`
/// directamente en GPU sin tocar píxeles en CPU ni asignar buffers por frame.
#[derive(Clone)]
pub struct Nv12GlRenderer {
    state: Arc<Mutex<Nv12GlState>>,
}

impl Nv12GlRenderer {
    pub fn new(gl: &glow::Context) -> Option<Self> {
        unsafe {
            let program = match link_nv12_program(gl) {
                Ok(p) => p,
                Err(msg) => {
                    warn!("Shader GL NV12 no disponible; usando fallback RGBA: {msg}");
                    return None;
                }
            };

            let vao = match gl.create_vertex_array() {
                Ok(v) => v,
                Err(err) => {
                    gl.delete_program(program);
                    warn!("create_vertex_array NV12 falló: {err}");
                    return None;
                }
            };

            let tex_y = create_video_plane_texture(gl);
            let tex_uv = create_video_plane_texture(gl);
            let (Some(tex_y), Some(tex_uv)) = (tex_y, tex_uv) else {
                if let Some(t) = tex_y {
                    gl.delete_texture(t);
                }
                if let Some(t) = tex_uv {
                    gl.delete_texture(t);
                }
                gl.delete_vertex_array(vao);
                gl.delete_program(program);
                return None;
            };

            let u_tex_y = gl.get_uniform_location(program, "u_tex_y");
            let u_tex_uv = gl.get_uniform_location(program, "u_tex_uv");
            let u_bt709 = gl.get_uniform_location(program, "u_bt709");
            let u_full_range = gl.get_uniform_location(program, "u_full_range");

            debug!("Renderizador GL NV12 inicializado correctamente");
            Some(Self {
                state: Arc::new(Mutex::new(Nv12GlState {
                    program: Some(program),
                    vao: Some(vao),
                    tex_y: Some(tex_y),
                    tex_uv: Some(tex_uv),
                    u_tex_y,
                    u_tex_uv,
                    u_bt709,
                    u_full_range,
                    tex_w: 0,
                    tex_h: 0,
                    is_bt709: true,
                    full_range: false,
                    frame_size: None,
                    has_frame: false,
                    pending: None,
                })),
            })
        }
    }

    pub fn set_frame(
        &self,
        width: u32,
        height: u32,
        data: Vec<u8>,
        is_bt709: bool,
        full_range: bool,
        pool: &FrameBufferPool,
    ) {
        if let Ok(mut st) = self.state.lock() {
            if let Some(prev) = st.pending.take() {
                pool.recycle_nv12(prev.data);
            }
            st.frame_size = Some((width, height));
            st.has_frame = true;
            st.pending = Some(PendingNv12 {
                width,
                height,
                data,
                is_bt709,
                full_range,
            });
        } else {
            pool.recycle_nv12(data);
        }
    }

    pub fn clear(&self, pool: &FrameBufferPool) {
        if let Ok(mut st) = self.state.lock() {
            st.has_frame = false;
            st.frame_size = None;
            if let Some(prev) = st.pending.take() {
                pool.recycle_nv12(prev.data);
            }
        }
    }

    pub fn frame_size(&self) -> Option<(u32, u32)> {
        self.state
            .lock()
            .ok()
            .filter(|st| st.has_frame)
            .and_then(|st| st.frame_size)
    }

    pub fn paint_callback(&self, rect: egui::Rect, pool: FrameBufferPool) -> egui::PaintCallback {
        let state = Arc::clone(&self.state);
        egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
                let gl = painter.gl();
                if let Ok(mut st) = state.lock() {
                    paint_nv12_locked(gl, &mut st, &pool);
                }
            })),
        }
    }

    pub fn destroy(&self, gl: &glow::Context) {
        if let Ok(mut st) = self.state.lock() {
            st.has_frame = false;
            st.pending = None;
            unsafe {
                if let Some(t) = st.tex_y.take() {
                    gl.delete_texture(t);
                }
                if let Some(t) = st.tex_uv.take() {
                    gl.delete_texture(t);
                }
                if let Some(v) = st.vao.take() {
                    gl.delete_vertex_array(v);
                }
                if let Some(p) = st.program.take() {
                    gl.delete_program(p);
                }
            }
        }
    }
}

fn paint_nv12_locked(gl: &glow::Context, st: &mut Nv12GlState, pool: &FrameBufferPool) {
    let (Some(program), Some(vao), Some(tex_y), Some(tex_uv)) =
        (st.program, st.vao, st.tex_y, st.tex_uv)
    else {
        return;
    };

    if let Some(frame) = st.pending.take() {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let uv_w = w / 2;
        let uv_h = h / 2;
        let y_bytes = w * h;
        let uv_bytes = uv_w * uv_h * 2;
        if w >= 2 && h >= 2 && frame.data.len() >= y_bytes + uv_bytes {
            let realloc = st.tex_w != frame.width || st.tex_h != frame.height;
            unsafe {
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                gl.bind_texture(glow::TEXTURE_2D, Some(tex_y));
                let y_slice = &frame.data[..y_bytes];
                if realloc {
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::R8 as i32,
                        frame.width as i32,
                        frame.height as i32,
                        0,
                        glow::RED,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(y_slice)),
                    );
                } else {
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        0,
                        0,
                        frame.width as i32,
                        frame.height as i32,
                        glow::RED,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(y_slice)),
                    );
                }

                gl.bind_texture(glow::TEXTURE_2D, Some(tex_uv));
                let uv_slice = &frame.data[y_bytes..y_bytes + uv_bytes];
                if realloc {
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RG8 as i32,
                        uv_w as i32,
                        uv_h as i32,
                        0,
                        glow::RG,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(uv_slice)),
                    );
                } else {
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        0,
                        0,
                        uv_w as i32,
                        uv_h as i32,
                        glow::RG,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(uv_slice)),
                    );
                }
                gl.bind_texture(glow::TEXTURE_2D, None);
            }
            st.tex_w = frame.width;
            st.tex_h = frame.height;
            st.is_bt709 = frame.is_bt709;
            st.full_range = frame.full_range;
        }
        pool.recycle_nv12(frame.data);
    }

    if !st.has_frame || st.tex_w == 0 || st.tex_h == 0 {
        return;
    }

    unsafe {
        gl.disable(glow::BLEND);
        gl.use_program(Some(program));
        gl.bind_vertex_array(Some(vao));

        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(tex_y));
        gl.uniform_1_i32(st.u_tex_y.as_ref(), 0);

        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(tex_uv));
        gl.uniform_1_i32(st.u_tex_uv.as_ref(), 1);

        gl.uniform_1_i32(st.u_bt709.as_ref(), i32::from(st.is_bt709));
        gl.uniform_1_i32(st.u_full_range.as_ref(), i32::from(st.full_range));

        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

        gl.enable(glow::BLEND);
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.bind_vertex_array(None);
        gl.use_program(None);
    }
}

unsafe fn create_video_plane_texture(gl: &glow::Context) -> Option<glow::Texture> {
    let tex = unsafe { gl.create_texture() }.ok()?;
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
    Some(tex)
}

unsafe fn link_nv12_program(gl: &glow::Context) -> Result<glow::Program, String> {
    let vs = unsafe { compile_gl_shader(gl, glow::VERTEX_SHADER, NV12_VERT_SRC) }?;
    let fs = match unsafe { compile_gl_shader(gl, glow::FRAGMENT_SHADER, NV12_FRAG_SRC) } {
        Ok(s) => s,
        Err(err) => {
            unsafe { gl.delete_shader(vs) };
            return Err(err);
        }
    };

    let program = unsafe { gl.create_program() }.map_err(|e| format!("create_program: {e}"))?;
    unsafe {
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);
        gl.detach_shader(program, vs);
        gl.detach_shader(program, fs);
        gl.delete_shader(vs);
        gl.delete_shader(fs);
    }

    if unsafe { gl.get_program_link_status(program) } {
        Ok(program)
    } else {
        let log = unsafe { gl.get_program_info_log(program) };
        unsafe { gl.delete_program(program) };
        Err(format!("link NV12 shader: {log}"))
    }
}

unsafe fn compile_gl_shader(
    gl: &glow::Context,
    shader_type: u32,
    source: &str,
) -> Result<glow::Shader, String> {
    let shader = unsafe { gl.create_shader(shader_type) }
        .map_err(|e| format!("create_shader: {e}"))?;
    unsafe {
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
    }
    if unsafe { gl.get_shader_compile_status(shader) } {
        Ok(shader)
    } else {
        let log = unsafe { gl.get_shader_info_log(shader) };
        unsafe { gl.delete_shader(shader) };
        Err(format!("compile NV12 shader: {log}"))
    }
}
