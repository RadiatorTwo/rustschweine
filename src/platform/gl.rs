//! OpenGL: the context of the game window and what it shows. A shown picture is either a 2D
//! frame (DirectDraw: menus, the software renderer) uploaded as a texture, or a texture
//! something else rendered (the Glide scene); a video and the dialog overlay go on top.

use glow::HasContext;
use glutin::context::{ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentGlContext, PossiblyCurrentContext, Version};
use glutin::config::GlConfig;
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use glutin_winit::GlWindow;
use raw_window_handle::HasWindowHandle;
use std::num::NonZeroU32;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes};

/// GLSL for desktop OpenGL 3.3 core.
pub const GLSL: &str = "#version 330 core\n";

pub struct Gl {
    pub gl: glow::Context,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    blit: glow::Program,
    vao: glow::VertexArray,
    frame: glow::Texture,
    overlay: glow::Texture,
    has_overlay: bool,
    video: glow::Texture,
    /// where the video is shown: x, y, width, height as parts of the 4:3 picture
    video_rect: Option<[f32; 4]>,
    /// what is shown: a texture, its size, whether its pixels are 0RGB/ARGB words (2D frames:
    /// the bytes are B G R A), and the gamma to show it with
    pub shown: (glow::Texture, u32, u32, bool, f32),
}

pub fn compile(gl: &glow::Context, vs: &str, fs: &str) -> glow::Program {
    unsafe {
        let p = gl.create_program().expect("GL program");
        for (kind, src) in [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)] {
            let s = gl.create_shader(kind).expect("GL shader");
            gl.shader_source(s, &format!("{GLSL}{src}"));
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                panic!("shader: {}", gl.get_shader_info_log(s));
            }
            gl.attach_shader(p, s);
        }
        gl.link_program(p);
        if !gl.get_program_link_status(p) {
            panic!("shader program: {}", gl.get_program_info_log(p));
        }
        p
    }
}

/// A texture with nearest or linear filtering, clamped.
pub fn texture(gl: &glow::Context, linear: bool) -> glow::Texture {
    TOUCHED.store(true, std::sync::atomic::Ordering::Relaxed);
    unsafe {
        let t = gl.create_texture().expect("GL texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        let f = if linear { glow::LINEAR } else { glow::NEAREST } as i32;
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, f);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, f);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        t
    }
}

/// Upload 32-bit words (as their little-endian bytes, RGBA order) to a texture.
/// Set when GL's state was changed outside the Direct3D device's drawing (a texture bound for
/// loading, the window drawn): `d3d` sets its state again then.
pub static TOUCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn upload(gl: &glow::Context, t: glow::Texture, px: &[u32], w: u32, h: u32) {
    TOUCHED.store(true, std::sync::atomic::Ordering::Relaxed);
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        let bytes = std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 4);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w as i32,
            h as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(bytes)),
        );
    }
}

const BLIT_VS: &str = r#"
uniform vec4 rect;
out vec2 uv;
void main() {
    vec2 c = vec2(gl_VertexID & 1, gl_VertexID >> 1);
    uv = vec2(c.x, 1.0 - c.y);
    gl_Position = vec4(mix(rect.xy, rect.zw, c), 0.0, 1.0);
}
"#;

const BLIT_FS: &str = r#"
uniform sampler2D tex;
uniform int words;
uniform int flip;
uniform float gamma;
in vec2 uv;
out vec4 color;
void main() {
    vec4 c = texture(tex, flip != 0 ? vec2(uv.x, 1.0 - uv.y) : uv);
    // 2D frames hold 0RGB/ARGB words: their bytes are B G R A
    if (words != 0) c = c.bgra;
    color = vec4(pow(c.rgb, vec3(1.0 / gamma)), c.a);
}
"#;

impl Gl {
    /// Create the window with an OpenGL 3.3 context.
    pub fn new(el: &ActiveEventLoop, attrs: WindowAttributes) -> (Window, Gl) {
        let template = glutin::config::ConfigTemplateBuilder::new().with_alpha_size(8);
        let (win, config) = glutin_winit::DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(el, template, |configs| configs.reduce(|a, b| if b.num_samples() < a.num_samples() { b } else { a }).unwrap())
            .expect("cannot create an OpenGL window");
        let win = win.expect("no window");
        let display = config.display();
        let handle = win.window_handle().ok().map(|h| h.as_raw());
        let attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .with_profile(GlProfile::Core)
            .build(handle);
        let context = unsafe { display.create_context(&config, &attrs) }.expect("cannot create an OpenGL 3.3 context");
        let sattrs = win.build_surface_attributes(Default::default()).expect("surface attributes");
        let surface = unsafe { display.create_window_surface(&config, &sattrs) }.expect("cannot create the GL surface");
        let context = context.make_current(&surface).expect("cannot make the GL context current");
        // the game paces itself
        let _ = surface.set_swap_interval(&context, SwapInterval::DontWait);
        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s)) };
        let blit = compile(&gl, BLIT_VS, BLIT_FS);
        let vao = unsafe { gl.create_vertex_array() }.expect("GL vertex array");
        let frame = texture(&gl, false);
        upload(&gl, frame, &vec![0; 640 * 480], 640, 480);
        let overlay = texture(&gl, true);
        let video = texture(&gl, true);
        let g = Gl { gl, surface, context, blit, vao, frame, overlay, has_overlay: false, video, video_rect: None, shown: (frame, 640, 480, true, 1.0) };
        (win, g)
    }

    /// Show a 2D frame of 0RGB pixels.
    pub fn frame(&mut self, px: &[u32], w: u32, h: u32) {
        upload(&self.gl, self.frame, px, w, h);
        self.shown = (self.frame, w, h, true, 1.0);
    }

    pub fn overlay(&mut self, o: Option<(&[u32], u32, u32)>) {
        self.has_overlay = o.is_some();
        if let Some((px, w, h)) = o {
            upload(&self.gl, self.overlay, px, w, h);
        }
    }

    /// Show a video frame (0RGB, w x h) in `rect` (see `video_rect`), or no video.
    pub fn video(&mut self, v: Option<(&[u32], u32, u32, [f32; 4])>) {
        self.video_rect = v.map(|v| v.3);
        if let Some((px, w, h, _)) = v {
            upload(&self.gl, self.video, px, w, h);
        }
    }

    pub fn resize(&self, w: u32, h: u32) {
        if let (Some(w), Some(h)) = (NonZeroU32::new(w), NonZeroU32::new(h)) {
            self.surface.resize(&self.context, w, h);
        }
    }

    /// Draw the shown picture and the overlay into the window (w x h), letterboxed; with `shot`,
    /// read back the picture's area as 0RGB pixels.
    pub fn draw(&mut self, w: u32, h: u32, shot: bool) -> Option<(Vec<u32>, u32, u32)> {
        let gl = &self.gl;
        let (tex, fw, fh, words, gamma) = self.shown;
        let (ox, oy, s) = super::window::fit(fw, fh, w, h);
        let (x0, y0) = (ox / w as f64 * 2.0 - 1.0, 1.0 - oy / h as f64 * 2.0);
        let (x1, y1) = (x0 + fw as f64 * s / w as f64 * 2.0, y0 - fh as f64 * s / h as f64 * 2.0);
        TOUCHED.store(true, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.viewport(0, 0, w as i32, h as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            // the window stays opaque: 2D frames' 0RGB words have alpha 0, and a compositor that
            // blends windows by their alpha (Wayland) showed what is behind through the menu
            gl.color_mask(true, true, true, false);
            gl.use_program(Some(self.blit));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            let u = |n: &str| gl.get_uniform_location(self.blit, n);
            gl.uniform_4_f32(u("rect").as_ref(), x0 as f32, y1 as f32, x1 as f32, y0 as f32);
            gl.uniform_1_i32(u("tex").as_ref(), 0);
            gl.disable(glow::BLEND);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.uniform_1_i32(u("words").as_ref(), words as i32);
            // rendered scenes have their first row at the bottom, uploaded frames at the top
            gl.uniform_1_i32(u("flip").as_ref(), !words as i32);
            gl.uniform_1_f32(u("gamma").as_ref(), gamma);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            // videos and dialogs stay 4:3 over a wide picture
            let (sx, sw) = super::window::overlay_span(fw, fh);
            let (ox0, ox1) = (x0 + (x1 - x0) * sx as f64 / fw as f64, x0 + (x1 - x0) * (sx + sw) as f64 / fw as f64);
            if let Some([vx, vy, vw, vh]) = self.video_rect {
                let (vx, vy, vw, vh) = (vx as f64, vy as f64, vw as f64, vh as f64);
                let (ax, bx) = (ox0 + (ox1 - ox0) * vx, ox0 + (ox1 - ox0) * (vx + vw));
                let (ay, by) = (y0 + (y1 - y0) * vy, y0 + (y1 - y0) * (vy + vh));
                gl.uniform_4_f32(u("rect").as_ref(), ax as f32, by as f32, bx as f32, ay as f32);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.video));
                gl.uniform_1_i32(u("words").as_ref(), 1);
                gl.uniform_1_i32(u("flip").as_ref(), 0);
                gl.uniform_1_f32(u("gamma").as_ref(), 1.0);
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
            if self.has_overlay {
                gl.uniform_4_f32(u("rect").as_ref(), ox0 as f32, y1 as f32, ox1 as f32, y0 as f32);
                gl.enable(glow::BLEND);
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.overlay));
                gl.uniform_1_i32(u("words").as_ref(), 1);
                gl.uniform_1_i32(u("flip").as_ref(), 0);
                gl.uniform_1_f32(u("gamma").as_ref(), 1.0);
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                gl.disable(glow::BLEND);
            }
            gl.color_mask(true, true, true, true);
        }
        let out = shot.then(|| {
            // the picture's area of the window, top row first
            let (rx, ry) = (ox.round() as i32, oy.round() as i32);
            let (rw, rh) = ((fw as f64 * s).round() as i32, (fh as f64 * s).round() as i32);
            let mut buf = vec![0u8; (rw * rh * 4) as usize];
            unsafe {
                gl.read_pixels(rx, h as i32 - ry - rh, rw, rh, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut buf)));
            }
            let px = (0..rh)
                .rev()
                .flat_map(|y| (0..rw).map(move |x| (x, y)))
                .map(|(x, y)| {
                    let i = ((y * rw + x) * 4) as usize;
                    (buf[i] as u32) << 16 | (buf[i + 1] as u32) << 8 | buf[i + 2] as u32
                })
                .collect();
            (px, rw as u32, rh as u32)
        });
        let _ = self.surface.swap_buffers(&self.context);
        out
    }
}
