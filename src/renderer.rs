//! Hybrid GPU pipeline, one render pass per frame:
//!
//! 1. clear with the chrome background (premultiplied, honoring opacity);
//! 2. WGSL instanced quads: pane backgrounds, cell backgrounds, underline /
//!    strikeout, cursor;
//! 3. `glyphon` text for every visible terminal, laid out on an exact
//!    monospace grid inside its pane viewport (HiDPI aware);
//! 4. `egui-wgpu`: sidebar, tab bars, footers and overlays on top.
//!
//! Text layout is cached per terminal row and only reshaped when that row's
//! content changes. Characters the primary font doesn't cover (emoji, Nerd
//! Font icons, CJK…) are drawn as standalone glyphs at their exact column, so
//! proportional fallback fonts can never shift the grid.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use alacritty_terminal::term::cell::Flags;
use glyphon::cosmic_text::{FeatureTag, FontFeatures};
use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight, Wrap, fontdb,
};
use winit::window::Window;

use crate::config::FontConfig;
use crate::session::{GridSnapshot, RenderCell, SessionId, SnapshotCursorShape};
use crate::theme::Rgb;

/// Compile-time guard: these only type-check if `glyphon`, `egui-wgpu` and
/// `egui-winit` are linked against the *same* `wgpu` / `winit` as we are.
#[allow(dead_code)]
fn assert_single_gpu_stack(device: &wgpu::Device, window: &winit::window::Window) {
    let _: &egui_wgpu::wgpu::Device = device;
    let _: &egui_winit::winit::window::Window = window;
    let _ = glyphon::Cache::new(device);
}

/// Size of one terminal cell in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorDraw {
    Hidden,
    /// Configured shape (pane focused, blink phase on).
    Focused,
    /// Hollow block (pane not focused).
    Unfocused,
}

/// One terminal to draw this frame. Coordinates are physical pixels.
pub struct PaneFrame<'a> {
    pub session: SessionId,
    /// The pane's terminal area (background fill + clip).
    pub area: [f32; 4],
    /// Top-left of the cell grid (area + padding).
    pub origin: [f32; 2],
    pub snapshot: &'a GridSnapshot,
    pub cursor: CursorDraw,
}

/// egui output to composite on top.
pub struct EguiFrame {
    pub primitives: Vec<egui::ClippedPrimitive>,
    pub textures: egui::TexturesDelta,
    pub pixels_per_point: f32,
}

// ---------------------------------------------------------------------------
// Quads
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Quad {
    rect: [f32; 4],
    color: [f32; 4],
}

impl Quad {
    fn new(x: f32, y: f32, w: f32, h: f32, color: Rgb, alpha: f32) -> Self {
        Self { rect: [x, y, w, h], color: rgba(color, alpha) }
    }
}

fn rgba(c: Rgb, alpha: f32) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, alpha]
}

const QUAD_SHADER: &str = r#"
struct Globals { screen: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;

struct Instance {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
};

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32, inst: Instance) -> VertexOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let p = inst.rect.xy + corners[index] * inst.rect.zw;
    let ndc = vec2<f32>(p.x / globals.screen.x * 2.0 - 1.0, 1.0 - p.y / globals.screen.y * 2.0);
    var out: VertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    // Premultiplied alpha: window opacity composites correctly.
    out.color = vec4<f32>(inst.color.rgb * inst.color.a, inst.color.a);
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

struct QuadPipeline {
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    capacity: usize,
}

impl QuadPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("termy quads"),
            source: wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()),
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("termy globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("termy globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("termy globals"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("termy quads"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("termy quads"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Quad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let capacity = 1024;
        let instances = Self::instance_buffer(device, capacity);
        Self { pipeline, globals, bind_group, instances, capacity }
    }

    fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("termy quad instances"),
            size: (capacity * std::mem::size_of::<Quad>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, screen: [f32; 2], quads: &[Quad]) {
        queue.write_buffer(&self.globals, 0, bytemuck::cast_slice(&[screen[0], screen[1], 0.0, 0.0]));
        if quads.len() > self.capacity {
            self.capacity = quads.len().next_power_of_two();
            self.instances = Self::instance_buffer(device, self.capacity);
        }
        if !quads.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(quads));
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, count: usize) {
        if count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..6, 0..count as u32);
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// A shaped piece of a row starting at `col`.
struct Segment {
    col: usize,
    buffer: Buffer,
}

#[derive(Default)]
struct RowCache {
    hash: u64,
    segments: Vec<Segment>,
}

/// Font state + per-session row caches.
struct Fonts {
    system: FontSystem,
    swash: SwashCache,
    family: String,
    /// The primary face, to test which characters it covers (cmap lookup).
    primary: Option<Arc<glyphon::Font>>,
    coverage: HashMap<char, bool>,
    features: Option<FontFeatures>,
    metrics: CellMetrics,
    rows: HashMap<SessionId, Vec<RowCache>>,
}

impl Fonts {
    fn new(config: &FontConfig, scale: f32) -> Self {
        let mut fonts = Self {
            system: FontSystem::new(),
            swash: SwashCache::new(),
            family: String::new(),
            primary: None,
            coverage: HashMap::new(),
            features: None,
            metrics: CellMetrics { width: 1.0, height: 1.0, font_size: 1.0 },
            rows: HashMap::new(),
        };
        fonts.configure(config, scale);
        fonts
    }

    /// Resolves the family (falling back to the system monospace font),
    /// measures the cell and invalidates caches.
    fn configure(&mut self, config: &FontConfig, scale: f32) {
        let db = self.system.db();
        let query = |family: fontdb::Family<'_>| {
            db.query(&fontdb::Query {
                families: &[family],
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            })
        };
        let id = query(fontdb::Family::Name(&config.family)).or_else(|| {
            log::warn!("font family {:?} not found, using the system monospace font", config.family);
            query(fontdb::Family::Monospace)
        });
        self.family = id
            .and_then(|id| db.face(id))
            .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
            .unwrap_or_else(|| config.family.clone());
        self.primary = id.and_then(|id| self.system.get_font(id, fontdb::Weight::NORMAL));
        self.coverage.clear();
        self.features = (!config.ligatures).then(|| {
            let mut f = FontFeatures::new();
            f.disable(FeatureTag::STANDARD_LIGATURES)
                .disable(FeatureTag::CONTEXTUAL_LIGATURES)
                .disable(FeatureTag::CONTEXTUAL_ALTERNATES);
            f
        });

        let font_size = (config.size as f32 * scale).max(4.0);
        let height = (font_size * config.line_height as f32).round().max(1.0);
        let probe = "0123456789";
        let mut buffer = Buffer::new(&mut self.system, Metrics::new(font_size, height));
        buffer.set_wrap(Wrap::None);
        let attrs = Attrs::new().family(Family::Name(&self.family));
        buffer.set_text(probe, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.system, false);
        let width = buffer
            .layout_runs()
            .next()
            .map(|run| run.line_w / probe.len() as f32)
            .filter(|w| *w > 0.0)
            .unwrap_or(font_size * 0.6);
        self.metrics = CellMetrics { width, height, font_size };
        self.rows.clear();
    }

    fn covers(&mut self, c: char) -> bool {
        if c.is_ascii() {
            return true;
        }
        let Some(font) = &self.primary else { return true };
        *self.coverage.entry(c).or_insert_with(|| font.as_swash().charmap().map(c) != 0)
    }

    fn attrs(&self, fg: Rgb, flags: Flags) -> Attrs<'_> {
        let mut attrs = Attrs::new()
            .family(Family::Name(&self.family))
            .color(GlyphColor::rgb(fg.r, fg.g, fg.b));
        if flags.contains(Flags::BOLD) {
            attrs = attrs.weight(Weight::BOLD);
        }
        if flags.contains(Flags::ITALIC) {
            attrs = attrs.style(Style::Italic);
        }
        if let Some(features) = &self.features {
            attrs = attrs.font_features(features.clone());
        }
        attrs
    }

    fn shape(&mut self, spans: &[(String, Rgb, Flags)]) -> Buffer {
        let CellMetrics { width, height, font_size } = self.metrics;
        let mut buffer = Buffer::new(&mut self.system, Metrics::new(font_size, height));
        buffer.set_wrap(Wrap::None);
        buffer.set_monospace_width(Some(width));
        let attrs: Vec<Attrs<'_>> = spans.iter().map(|(_, fg, flags)| self.attrs(*fg, *flags)).collect();
        let default = Attrs::new().family(Family::Name(&self.family));
        buffer.set_rich_text(
            spans.iter().zip(attrs).map(|((text, _, _), a)| (text.as_str(), a)),
            &default,
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut self.system, false);
        buffer
    }

    /// Splits a row into shaped segments: runs of characters the primary
    /// font covers (ligatures work inside a run) and isolated cells for
    /// everything else.
    fn build_row(&mut self, cells: &[RenderCell]) -> Vec<Segment> {
        let mut segments = Vec::new();
        let mut run: Vec<(String, Rgb, Flags)> = Vec::new();
        let mut run_start = 0;
        let mut run_has_ink = false;

        let flush = |fonts: &mut Fonts, run: &mut Vec<(String, Rgb, Flags)>, start: usize, ink: bool, out: &mut Vec<Segment>| {
            if ink {
                out.push(Segment { col: start, buffer: fonts.shape(run) });
            }
            run.clear();
        };

        for (col, cell) in cells.iter().enumerate() {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let style = cell.flags & (Flags::BOLD | Flags::ITALIC);
            let isolated = cell.flags.contains(Flags::WIDE_CHAR) || !self.covers(cell.c);
            if isolated {
                flush(self, &mut run, run_start, run_has_ink, &mut segments);
                run_has_ink = false;
                let mut text = cell.c.to_string();
                text.extend(cell.zerowidth.iter().flat_map(|z| z.iter()));
                let buffer = self.shape(&[(text, cell.fg, style)]);
                segments.push(Segment { col, buffer });
                continue;
            }
            if run.is_empty() {
                run_start = col;
            }
            run_has_ink |= cell.c != ' ';
            let same_style = run.last().is_some_and(|(_, fg, f)| *fg == cell.fg && *f == style);
            if !same_style {
                run.push((String::new(), cell.fg, style));
            }
            let text = &mut run.last_mut().expect("run has a span").0;
            text.push(cell.c);
            text.extend(cell.zerowidth.iter().flat_map(|z| z.iter()));
        }
        flush(self, &mut run, run_start, run_has_ink, &mut segments);
        segments
    }

    /// Updates the cache of `session` to match `snap` (only changed rows are
    /// reshaped). `cursor_text` recolors the character under a block cursor.
    fn sync(&mut self, session: SessionId, snap: &GridSnapshot, cursor_text: Option<(usize, usize, Rgb)>) {
        let mut rows = self.rows.remove(&session).unwrap_or_default();
        rows.resize_with(snap.rows, RowCache::default);
        for (row, cache) in rows.iter_mut().enumerate() {
            let mut cells = snap.cells[row * snap.cols..(row + 1) * snap.cols].to_vec();
            if let Some((r, c, color)) = cursor_text
                && r == row
                && let Some(cell) = cells.get_mut(c)
            {
                cell.fg = color;
            }
            let hash = row_hash(&cells, self.metrics.width);
            if cache.hash != hash || cache.segments.is_empty() && cells.iter().any(|c| c.c != ' ') {
                cache.segments = self.build_row(&cells);
                cache.hash = hash;
            }
        }
        self.rows.insert(session, rows);
    }
}

fn row_hash(cells: &[RenderCell], cell_width: f32) -> u64 {
    let mut h = DefaultHasher::new();
    cell_width.to_bits().hash(&mut h);
    for cell in cells {
        cell.c.hash(&mut h);
        cell.zerowidth.hash(&mut h);
        cell.fg.hash(&mut h);
        (cell.flags & (Flags::BOLD | Flags::ITALIC | Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER)).bits().hash(&mut h);
    }
    h.finish()
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    transparent_supported: bool,
    quads: QuadPipeline,
    fonts: Fonts,
    /// Shared glyphon pipeline cache; kept alive alongside the atlas.
    _cache: Cache,
    atlas: TextAtlas,
    viewport: Viewport,
    text: TextRenderer,
    egui: egui_wgpu::Renderer,
    scale: f32,
    /// Development aid (`TERMY_CAPTURE`): save the next frame as a BMP.
    capture: Option<std::path::PathBuf>,
}

impl Renderer {
    pub fn new(
        window: Arc<Window>,
        display: Box<dyn wgpu::wgt::WgpuHasDisplayHandle>,
        font: &FontConfig,
        transparent: bool,
    ) -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(display));
        let surface = instance.create_surface(window.clone()).map_err(|e| format!("surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no suitable GPU adapter: {e}"))?;
        log::info!("GPU: {:?}", adapter.get_info());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("termy"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| format!("GPU device: {e}"))?;

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        // Non-sRGB target: quads, glyphon (`ColorMode::Web`) and egui all blend
        // in gamma space, like browsers and most terminals.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or("surface has no formats")?;
        let transparent_modes = [wgpu::CompositeAlphaMode::PreMultiplied, wgpu::CompositeAlphaMode::PostMultiplied];
        let transparent_supported = transparent_modes.iter().any(|m| caps.alpha_modes.contains(m));
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("surface not supported by adapter")?;
        config.format = format;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.alpha_mode = Self::alpha_mode(&caps.alpha_modes, transparent);
        config.desired_maximum_frame_latency = 2;
        if std::env::var_os("TERMY_CAPTURE").is_some() && caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
            config.usage |= wgpu::TextureUsages::COPY_SRC;
        }
        surface.configure(&device, &config);

        let cache = Cache::new(&device);
        let mut atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);
        let viewport = Viewport::new(&device, &cache);
        let text = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let quads = QuadPipeline::new(&device, format);
        let egui = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        let scale = window.scale_factor() as f32;

        Ok(Self {
            surface,
            device,
            queue,
            config,
            transparent_supported,
            quads,
            fonts: Fonts::new(font, scale),
            _cache: cache,
            atlas,
            viewport,
            text,
            egui,
            scale,
            capture: None,
        })
    }

    fn alpha_mode(modes: &[wgpu::CompositeAlphaMode], transparent: bool) -> wgpu::CompositeAlphaMode {
        use wgpu::CompositeAlphaMode::*;
        let wanted: &[wgpu::CompositeAlphaMode] = if transparent { &[PreMultiplied, PostMultiplied, Auto] } else { &[Opaque, Auto] };
        wanted.iter().copied().find(|m| modes.contains(m)).unwrap_or(modes[0])
    }

    pub fn transparency_supported(&self) -> bool {
        self.transparent_supported
    }

    /// Switches between opaque and transparent composition (opacity < 1).
    pub fn set_transparent(&mut self, adapter_modes_transparent: bool) {
        let mode = if adapter_modes_transparent && self.transparent_supported {
            if self.config.alpha_mode == wgpu::CompositeAlphaMode::Opaque {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else {
                self.config.alpha_mode
            }
        } else {
            wgpu::CompositeAlphaMode::Opaque
        };
        if mode != self.config.alpha_mode {
            self.config.alpha_mode = mode;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_font(&mut self, font: &FontConfig, scale: f32) {
        self.scale = scale;
        self.fonts.configure(font, scale);
    }

    pub fn cell_metrics(&self) -> CellMetrics {
        self.fonts.metrics
    }

    /// Installed monospace families (font picker), sorted and deduplicated.
    pub fn monospace_families(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .fonts
            .system
            .db()
            .faces()
            .filter(|f| f.monospaced)
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort_unstable_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    }

    /// Raw bytes of the primary font, so egui can use the same face.
    pub fn font_bytes(&self) -> Option<Vec<u8>> {
        let db = self.fonts.system.db();
        let id = db.query(&fontdb::Query {
            families: &[fontdb::Family::Name(&self.fonts.family)],
            ..Default::default()
        })?;
        db.with_face_data(id, |data, _| data.to_vec())
    }

    /// Draws one frame. `clear` is the chrome background; `opacity` applies
    /// to every background (text stays opaque).
    /// Saves the next rendered frame to `path` (BMP).
    pub fn capture_next_frame(&mut self, path: std::path::PathBuf) {
        self.capture = Some(path);
    }

    pub fn render(&mut self, clear: Rgb, opacity: f32, panes: &[PaneFrame<'_>], mut egui_frame: EguiFrame) -> Result<(), String> {
        // egui textures don't depend on the frame: upload them first so an
        // early return below never drops an unapplied delta.
        for (id, deltas) in &egui_frame.textures.set {
            for delta in deltas {
                self.egui.update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        let free: Vec<egui::TextureId> = egui_frame.textures.free.iter().copied().collect();
        egui_frame.textures.clear();
        let result = self.render_frame(clear, opacity, panes, &egui_frame);
        for id in &free {
            self.egui.free_texture(id);
        }
        result
    }

    fn render_frame(&mut self, clear: Rgb, opacity: f32, panes: &[PaneFrame<'_>], egui_frame: &EguiFrame) -> Result<(), String> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => return Err("surface validation error".into()),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let (width, height) = (self.config.width, self.config.height);
        let m = self.fonts.metrics;
        let line = (self.scale).round().max(1.0);

        // Geometry + text caches.
        let mut quads = Vec::new();
        for pane in panes {
            let snap = pane.snapshot;
            let [ax, ay, aw, ah] = pane.area;
            quads.push(Quad::new(ax, ay, aw, ah, snap.background, opacity));
            let [ox, oy] = pane.origin;
            for row in 0..snap.rows {
                let y = oy + row as f32 * m.height;
                let cells = &snap.cells[row * snap.cols..(row + 1) * snap.cols];
                // Merge equal-background runs into one quad.
                let mut col = 0;
                while col < cells.len() {
                    let bg = cells[col].bg;
                    let start = col;
                    while col < cells.len() && cells[col].bg == bg {
                        col += 1;
                    }
                    if bg != snap.background {
                        quads.push(Quad::new(ox + start as f32 * m.width, y, (col - start) as f32 * m.width, m.height, bg, opacity));
                    }
                }
                for (col, cell) in cells.iter().enumerate() {
                    let x = ox + col as f32 * m.width;
                    let w = if cell.flags.contains(Flags::WIDE_CHAR) { 2.0 * m.width } else { m.width };
                    if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                        quads.push(Quad::new(x, y + m.height - 2.0 * line, w, line, cell.fg, 1.0));
                    }
                    if cell.flags.contains(Flags::STRIKEOUT) {
                        quads.push(Quad::new(x, y + (m.height / 2.0).round(), w, line, cell.fg, 1.0));
                    }
                }
            }

            // Prompt marks sit in the left padding (over column 0 without padding).
            let bar = (2.0 * line).max(2.0);
            for &(row, color) in &snap.prompt_marks {
                let x = ax + ((ox - ax - bar) / 2.0).max(0.0);
                quads.push(Quad::new(x, oy + row as f32 * m.height, bar, m.height, color, 1.0));
            }

            let mut cursor_text = None;
            if let Some(cursor) = snap.cursor.filter(|_| pane.cursor != CursorDraw::Hidden) {
                let x = ox + cursor.col as f32 * m.width;
                let y = oy + cursor.row as f32 * m.height;
                let w = if cursor.wide { 2.0 * m.width } else { m.width };
                let shape = if pane.cursor == CursorDraw::Unfocused { SnapshotCursorShape::HollowBlock } else { cursor.shape };
                let c = cursor.color;
                match shape {
                    SnapshotCursorShape::Block => {
                        quads.push(Quad::new(x, y, w, m.height, c, 1.0));
                        cursor_text = Some((cursor.row, cursor.col, cursor.text_color));
                    }
                    SnapshotCursorShape::Beam => quads.push(Quad::new(x, y, (2.0 * line).max(2.0), m.height, c, 1.0)),
                    SnapshotCursorShape::Underline => quads.push(Quad::new(x, y + m.height - 2.0 * line, w, 2.0 * line, c, 1.0)),
                    SnapshotCursorShape::HollowBlock => {
                        quads.push(Quad::new(x, y, w, line, c, 1.0));
                        quads.push(Quad::new(x, y + m.height - line, w, line, c, 1.0));
                        quads.push(Quad::new(x, y, line, m.height, c, 1.0));
                        quads.push(Quad::new(x + w - line, y, line, m.height, c, 1.0));
                    }
                }
            }
            self.fonts.sync(pane.session, snap, cursor_text);
        }
        // Forget sessions that are no longer visible.
        self.fonts.rows.retain(|id, _| panes.iter().any(|p| p.session == *id));
        self.quads.upload(&self.device, &self.queue, [width as f32, height as f32], &quads);

        // Text areas borrow the row caches; prepare needs the other fields.
        self.viewport.update(&self.queue, Resolution { width, height });
        let Fonts { system, swash, rows, .. } = &mut self.fonts;
        let mut areas = Vec::new();
        for pane in panes {
            let Some(cache) = rows.get(&pane.session) else { continue };
            let [ax, ay, aw, ah] = pane.area;
            let bounds = TextBounds {
                left: ax.floor() as i32,
                top: ay.floor() as i32,
                right: (ax + aw).ceil() as i32,
                bottom: (ay + ah).ceil() as i32,
            };
            let [ox, oy] = pane.origin;
            for (row, cache) in cache.iter().enumerate() {
                for seg in &cache.segments {
                    areas.push(TextArea {
                        buffer: &seg.buffer,
                        left: ox + seg.col as f32 * m.width,
                        top: oy + row as f32 * m.height,
                        scale: 1.0,
                        bounds,
                        default_color: GlyphColor::rgb(255, 255, 255),
                        custom_glyphs: &[],
                    });
                }
            }
        }
        if let Err(e) = self.text.prepare(&self.device, &self.queue, system, &mut self.atlas, &self.viewport, areas, swash) {
            log::warn!("text prepare: {e}");
        }

        // egui resources.
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [width, height], pixels_per_point: egui_frame.pixels_per_point };
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("termy frame") });
        let extra = self.egui.update_buffers(&self.device, &self.queue, &mut encoder, &egui_frame.primitives, &screen);

        {
            let [r, g, b, _] = rgba(clear, 1.0);
            let a = opacity as f64;
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("termy pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: r as f64 * a, g: g as f64 * a, b: b as f64 * a, a }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.quads.draw(&mut pass, quads.len());
            if let Err(e) = self.text.render(&self.atlas, &self.viewport, &mut pass) {
                log::warn!("text render: {e}");
            }
            self.egui.render(&mut pass, &egui_frame.primitives, &screen);
        }

        let readback = self.capture.take().filter(|_| self.config.usage.contains(wgpu::TextureUsages::COPY_SRC)).map(|path| {
            let padded = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("termy capture"),
                size: (padded * height) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                frame.texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: None },
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
            (path, buffer, padded)
        });

        self.queue.submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
        if let Some((path, buffer, padded)) = readback {
            buffer.map_async(wgpu::MapMode::Read, .., |_| {});
            let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
            match buffer.get_mapped_range(..) {
                Ok(data) => {
                    let bgra = matches!(self.config.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
                    match write_bmp(&path, &data, width, height, padded, bgra) {
                        Ok(()) => log::info!("captured frame to {}", path.display()),
                        Err(e) => log::error!("capture: {e}"),
                    }
                }
                Err(e) => log::error!("capture map: {e}"),
            }
        }
        self.queue.present(frame);
        self.atlas.trim();
        Ok(())
    }
}

/// Minimal 32-bit BMP writer (top-down), no image crate needed.
fn write_bmp(path: &std::path::Path, data: &[u8], width: u32, height: u32, stride: u32, bgra: bool) -> std::io::Result<()> {
    let row = width * 4;
    let size = 54 + row * height;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(-(height as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in 0..height {
        let line = &data[(y * stride) as usize..(y * stride + row) as usize];
        for px in line.as_chunks::<4>().0 {
            let (b, g, r) = if bgra { (px[0], px[1], px[2]) } else { (px[2], px[1], px[0]) };
            out.extend_from_slice(&[b, g, r, 255]);
        }
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FontConfig;

    fn cell(c: char, fg: Rgb) -> RenderCell {
        RenderCell { c, zerowidth: None, fg, bg: Rgb::default(), flags: Flags::empty() }
    }

    #[test]
    fn cell_metrics_scale_with_dpi() {
        let config = FontConfig::default();
        let one = Fonts::new(&config, 1.0).metrics;
        let two = Fonts::new(&config, 2.0).metrics;
        assert!(one.width > 3.0 && one.height >= config.size as f32);
        assert!((two.width / one.width - 2.0).abs() < 0.1, "{one:?} vs {two:?}");
        assert_eq!(two.font_size, 2.0 * one.font_size);
    }

    #[test]
    fn rows_split_into_styled_runs_and_isolated_glyphs() {
        let mut fonts = Fonts::new(&FontConfig::default(), 1.0);
        let white = Rgb::new(255, 255, 255);
        let red = Rgb::new(255, 0, 0);
        // "ab" white, "cd" red, then a private-use (Nerd Font) icon nobody's
        // primary font covers, then "e".
        let mut cells: Vec<RenderCell> = "abcd".chars().enumerate().map(|(i, c)| cell(c, if i < 2 { white } else { red })).collect();
        cells.push(cell('\u{F0001}', white));
        cells.push(cell(' ', white));
        cells.push(cell('e', white));
        cells.push(cell(' ', white));
        let segments = fonts.build_row(&cells);
        let cols: Vec<usize> = segments.iter().map(|s| s.col).collect();
        assert_eq!(cols, [0, 4, 5], "runs: [abcd] [icon] [ e ]");
        // Blank rows produce no segments at all.
        assert!(fonts.build_row(&[cell(' ', white), cell(' ', white)]).is_empty());
    }

    #[test]
    fn row_cache_only_reshapes_changed_rows() {
        let mut fonts = Fonts::new(&FontConfig::default(), 1.0);
        let white = Rgb::new(200, 200, 200);
        let mk = |text: &str| GridSnapshot {
            cols: 4,
            rows: 2,
            cells: text.chars().map(|c| cell(c, white)).collect(),
            cursor: None,
            display_offset: 0,
            background: Rgb::default(),
            prompt_marks: Vec::new(),
        };
        fonts.sync(1, &mk("ab  cd  "), None);
        let first_row_hash = fonts.rows[&1][0].hash;
        let second_row_hash = fonts.rows[&1][1].hash;
        fonts.sync(1, &mk("ab  xy  "), None);
        assert_eq!(fonts.rows[&1][0].hash, first_row_hash);
        assert_ne!(fonts.rows[&1][1].hash, second_row_hash);
        // The block cursor recolors its character → that row is rebuilt.
        fonts.sync(1, &mk("ab  xy  "), Some((0, 0, Rgb::new(0, 0, 0))));
        assert_ne!(fonts.rows[&1][0].hash, first_row_hash);
    }
}
