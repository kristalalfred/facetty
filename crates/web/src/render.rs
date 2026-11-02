use std::collections::HashMap;

use anyhow::{Result, anyhow};
use facetty_ascii::{Frame, GLYPHS};
use js_sys::{Object, Reflect};
use wasm_bindgen::JsCast;
use web_sys::{
    CanvasRenderingContext2d, HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlProgram,
    WebGlShader, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject,
};

use crate::layout::Rect;
use crate::util::{document, js};

pub const BACKGROUND: [f32; 3] = [12.0 / 255.0, 12.0 / 255.0, 14.0 / 255.0];
const GLYPH_W: i32 = 48;
const GLYPH_H: i32 = 96;
const FULL_BLOCK: usize = 9;
const RISING: f32 = 12.0;
const FALLING: f32 = 13.0;

const VERTEX: &str = r#"#version 300 es
uniform vec4 u_rect;
uniform vec2 u_canvas;
out vec2 v_uv;
void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    v_uv = corner;
    vec2 clip = (u_rect.xy + corner * u_rect.zw) / u_canvas * 2.0 - 1.0;
    gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"#version 300 es
precision highp float;
precision highp int;
precision highp sampler2DArray;
uniform sampler2D u_cells;
uniform sampler2DArray u_glyphs;
uniform vec2 u_grid;
uniform bool u_mirror;
uniform vec3 u_background;
uniform int u_palette;
in vec2 v_uv;
out vec4 color;
vec3 tint(vec3 c) {
    if (u_palette == 1) {
        float top = max(c.r, max(c.g, c.b));
        return top < 12.0 / 255.0 ? vec3(90.0 / 255.0) : c / top;
    }
    if (u_palette == 2) return vec3(230.0 / 255.0);
    if (u_palette == 3) return vec3(40.0, 255.0, 90.0) / 255.0;
    return c;
}
void main() {
    vec2 p = v_uv * u_grid;
    ivec2 grid = ivec2(u_grid);
    ivec2 cell = clamp(ivec2(p), ivec2(0), grid - 1);
    if (u_mirror) cell.x = grid.x - 1 - cell.x;
    vec4 c = texelFetch(u_cells, cell, 0);
    float glyph = floor(c.a * 255.0 + 0.5);
    if (u_mirror && (glyph == RISING || glyph == FALLING)) glyph = RISING + FALLING - glyph;
    float cover = textureGrad(u_glyphs, vec3(fract(p), glyph), dFdx(p), dFdy(p)).r;
    color = vec4(mix(u_background, tint(c.rgb), cover), 1.0);
}
"#;

/// Claude: the native client's palettes, applied as cells are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    Natural,
    Vivid,
    Mono,
    Matrix,
}

impl Palette {
    pub const ALL: [Palette; 4] = [
        Palette::Natural,
        Palette::Vivid,
        Palette::Mono,
        Palette::Matrix,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Palette::Natural => "natural",
            Palette::Vivid => "vivid",
            Palette::Mono => "mono",
            Palette::Matrix => "matrix",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

pub struct Tile<'a> {
    pub id: u32,
    pub rect: Rect,
    pub frame: &'a Frame,
    pub version: u64,
    pub mirror: bool,
}

struct Cells {
    texture: WebGlTexture,
    version: u64,
    size: (u16, u16),
}

/// Claude: each tile's cells are a small texture, and every pixel looks up
/// its cell and that cell's glyph.
pub struct Renderer {
    canvas: HtmlCanvasElement,
    gl: Gl,
    program: WebGlProgram,
    vao: WebGlVertexArrayObject,
    glyphs: WebGlTexture,
    uniforms: Uniforms,
    cells: HashMap<u32, Cells>,
}

struct Uniforms {
    rect: Option<WebGlUniformLocation>,
    canvas: Option<WebGlUniformLocation>,
    grid: Option<WebGlUniformLocation>,
    mirror: Option<WebGlUniformLocation>,
    palette: Option<WebGlUniformLocation>,
}

impl Renderer {
    pub fn new(canvas: HtmlCanvasElement) -> Result<Self> {
        let options = Object::new();
        let _ = Reflect::set(&options, &"alpha".into(), &false.into());
        let _ = Reflect::set(&options, &"antialias".into(), &false.into());
        let gl: Gl = canvas
            .get_context_with_context_options("webgl2", &options)
            .map_err(js)?
            .ok_or_else(|| anyhow!("this browser has no WebGL2"))?
            .unchecked_into();

        let fragment = FRAGMENT
            .replace("RISING", &format!("{RISING:.1}"))
            .replace("FALLING", &format!("{FALLING:.1}"));
        let program = link(
            &gl,
            &compile(&gl, Gl::VERTEX_SHADER, VERTEX)?,
            &compile(&gl, Gl::FRAGMENT_SHADER, &fragment)?,
        )?;
        gl.use_program(Some(&program));
        let uniform = |name| gl.get_uniform_location(&program, name);
        let uniforms = Uniforms {
            rect: uniform("u_rect"),
            canvas: uniform("u_canvas"),
            grid: uniform("u_grid"),
            mirror: uniform("u_mirror"),
            palette: uniform("u_palette"),
        };
        gl.uniform1i(uniform("u_cells").as_ref(), 0);
        gl.uniform1i(uniform("u_glyphs").as_ref(), 1);
        let [r, g, b] = BACKGROUND;
        gl.uniform3f(uniform("u_background").as_ref(), r, g, b);

        let vao = gl
            .create_vertex_array()
            .ok_or_else(|| anyhow!("cannot create a vertex array"))?;
        let glyphs = glyph_texture(&gl)?;
        let [r, g, b] = BACKGROUND;
        gl.clear_color(r, g, b, 1.0);
        gl.clear(Gl::COLOR_BUFFER_BIT);
        Ok(Self {
            canvas,
            gl,
            program,
            vao,
            glyphs,
            uniforms,
            cells: HashMap::new(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.canvas.width(), self.canvas.height())
    }

    pub fn resize(&self, width: u32, height: u32) {
        self.canvas.set_width(width);
        self.canvas.set_height(height);
    }

    /// Claude: `rect`s are in CSS pixels; `scale` is the device pixel ratio.
    pub fn draw(&mut self, tiles: &[Tile], scale: f64, palette: Palette) -> Result<()> {
        let gl = &self.gl;
        let (w, h) = (self.canvas.width() as i32, self.canvas.height() as i32);
        gl.viewport(0, 0, w, h);
        let [r, g, b] = BACKGROUND;
        gl.clear_color(r, g, b, 1.0);
        gl.clear(Gl::COLOR_BUFFER_BIT);
        gl.use_program(Some(&self.program));
        gl.bind_vertex_array(Some(&self.vao));
        gl.active_texture(Gl::TEXTURE1);
        gl.bind_texture(Gl::TEXTURE_2D_ARRAY, Some(&self.glyphs));
        gl.active_texture(Gl::TEXTURE0);
        gl.uniform2f(self.uniforms.canvas.as_ref(), w as f32, h as f32);
        let palette = Palette::ALL.iter().position(|p| *p == palette).unwrap_or(0);
        gl.uniform1i(self.uniforms.palette.as_ref(), palette as i32);

        self.cells.retain(|id, _| tiles.iter().any(|t| t.id == *id));
        for tile in tiles {
            if tile.frame.cols == 0 || tile.frame.rows == 0 {
                continue;
            }
            let cells = match self.cells.get_mut(&tile.id) {
                Some(cells) => cells,
                None => {
                    let texture = gl
                        .create_texture()
                        .ok_or_else(|| anyhow!("cannot create a texture"))?;
                    self.cells.entry(tile.id).or_insert(Cells {
                        texture,
                        version: u64::MAX,
                        size: (0, 0),
                    })
                }
            };
            gl.bind_texture(Gl::TEXTURE_2D, Some(&cells.texture));
            if cells.version != tile.version {
                upload_cells(gl, tile.frame, cells.size)?;
                cells.version = tile.version;
                cells.size = (tile.frame.cols, tile.frame.rows);
            }
            let rect = tile.rect;
            gl.uniform4f(
                self.uniforms.rect.as_ref(),
                (rect.x * scale).round() as f32,
                (rect.y * scale).round() as f32,
                (rect.w * scale).round() as f32,
                (rect.h * scale).round() as f32,
            );
            gl.uniform2f(
                self.uniforms.grid.as_ref(),
                tile.frame.cols as f32,
                tile.frame.rows as f32,
            );
            gl.uniform1i(self.uniforms.mirror.as_ref(), tile.mirror as i32);
            gl.draw_arrays(Gl::TRIANGLE_STRIP, 0, 4);
        }
        Ok(())
    }
}

fn upload_cells(gl: &Gl, frame: &Frame, previous: (u16, u16)) -> Result<()> {
    let data: Vec<u8> = frame
        .cells
        .iter()
        .flat_map(|c| [c.rgb[0], c.rgb[1], c.rgb[2], c.glyph])
        .collect();
    let (cols, rows) = (frame.cols as i32, frame.rows as i32);
    if previous != (frame.cols, frame.rows) {
        for (param, value) in [
            (Gl::TEXTURE_MIN_FILTER, Gl::NEAREST),
            (Gl::TEXTURE_MAG_FILTER, Gl::NEAREST),
            (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
            (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameteri(Gl::TEXTURE_2D, param, value as i32);
        }
        gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            Gl::RGBA8 as i32,
            cols,
            rows,
            0,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            Some(&data),
        )
        .map_err(js)?;
    } else {
        gl.tex_sub_image_2d_with_i32_and_i32_and_u32_and_type_and_opt_u8_array(
            Gl::TEXTURE_2D,
            0,
            0,
            0,
            cols,
            rows,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            Some(&data),
        )
        .map_err(js)?;
    }
    Ok(())
}

/// Claude: every glyph white on black, one layer each, so that sampling one
/// never bleeds into its neighbour.
fn glyph_texture(gl: &Gl) -> Result<WebGlTexture> {
    let canvas: HtmlCanvasElement = document()
        .create_element("canvas")
        .map_err(js)?
        .unchecked_into();
    canvas.set_width(GLYPH_W as u32);
    canvas.set_height(GLYPH_H as u32);
    let options = Object::new();
    let _ = Reflect::set(&options, &"willReadFrequently".into(), &true.into());
    let ctx: CanvasRenderingContext2d = canvas
        .get_context_with_context_options("2d", &options)
        .map_err(js)?
        .ok_or_else(|| anyhow!("no 2D canvas"))?
        .unchecked_into();
    let font_px = GLYPH_H as f64 / 1.2;
    ctx.set_font(&format!(
        "{font_px}px ui-monospace, SFMono-Regular, Menlo, Consolas, \"Liberation Mono\", monospace"
    ));
    ctx.set_text_align("center");
    ctx.set_text_baseline("middle");

    let (w, h) = (GLYPH_W as f64, GLYPH_H as f64);
    let mut layers = Vec::with_capacity((GLYPH_W * GLYPH_H) as usize * GLYPHS.len());
    for (i, glyph) in GLYPHS.iter().enumerate() {
        ctx.set_fill_style_str("black");
        ctx.fill_rect(0.0, 0.0, w, h);
        ctx.set_fill_style_str("white");
        if i == FULL_BLOCK {
            ctx.fill_rect(0.0, 0.0, w, h);
        } else {
            ctx.fill_text(&glyph.to_string(), w / 2.0, h / 2.0)
                .map_err(js)?;
        }
        let pixels = ctx
            .get_image_data(0, 0, GLYPH_W, GLYPH_H)
            .map_err(js)?
            .data();
        layers.extend(pixels.as_chunks::<4>().0.iter().map(|p| p[0]));
    }

    let texture = gl
        .create_texture()
        .ok_or_else(|| anyhow!("cannot create a texture"))?;
    gl.active_texture(Gl::TEXTURE1);
    gl.bind_texture(Gl::TEXTURE_2D_ARRAY, Some(&texture));
    gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 1);
    gl.tex_image_3d_with_opt_u8_array(
        Gl::TEXTURE_2D_ARRAY,
        0,
        Gl::R8 as i32,
        GLYPH_W,
        GLYPH_H,
        GLYPHS.len() as i32,
        0,
        Gl::RED,
        Gl::UNSIGNED_BYTE,
        Some(&layers),
    )
    .map_err(js)?;
    gl.pixel_storei(Gl::UNPACK_ALIGNMENT, 4);
    gl.generate_mipmap(Gl::TEXTURE_2D_ARRAY);
    for (param, value) in [
        (Gl::TEXTURE_MIN_FILTER, Gl::LINEAR_MIPMAP_LINEAR),
        (Gl::TEXTURE_MAG_FILTER, Gl::LINEAR),
        (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
        (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
    ] {
        gl.tex_parameteri(Gl::TEXTURE_2D_ARRAY, param, value as i32);
    }
    gl.active_texture(Gl::TEXTURE0);
    Ok(texture)
}

fn compile(gl: &Gl, kind: u32, source: &str) -> Result<WebGlShader> {
    let shader = gl
        .create_shader(kind)
        .ok_or_else(|| anyhow!("cannot create a shader"))?;
    gl.shader_source(&shader, source);
    gl.compile_shader(&shader);
    if gl
        .get_shader_parameter(&shader, Gl::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Ok(shader)
    } else {
        Err(anyhow!(
            "shader: {}",
            gl.get_shader_info_log(&shader).unwrap_or_default()
        ))
    }
}

fn link(gl: &Gl, vertex: &WebGlShader, fragment: &WebGlShader) -> Result<WebGlProgram> {
    let program = gl
        .create_program()
        .ok_or_else(|| anyhow!("cannot create a program"))?;
    gl.attach_shader(&program, vertex);
    gl.attach_shader(&program, fragment);
    gl.link_program(&program);
    if gl
        .get_program_parameter(&program, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Ok(program)
    } else {
        Err(anyhow!(
            "program: {}",
            gl.get_program_info_log(&program).unwrap_or_default()
        ))
    }
}
