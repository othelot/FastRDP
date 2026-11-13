//! Remote screen widget: an iced `shader` widget that uploads the decoded I420 planes as three R8
//! textures and converts BT.709 limited-range YUV to RGB on the GPU (no per-frame CPU conversion).
//! The picture keeps its aspect ratio (letterboxed). The widget also captures keyboard/mouse input
//! and maps it to remote desktop pixels.
//!
//! Pointer: the server streams the remote hardware cursor (shape + hotspot) separately from the
//! video. The shader composites it (alpha and XOR pixels) at the LOCAL pointer position while the
//! pointer is over the picture (no network round trip in pointer motion), else at the remote position.
use std::collections::HashSet;
use std::sync::Arc;

use iced::advanced::graphics::Viewport;
use iced::keyboard;
use iced::mouse;
use iced::wgpu;
use iced::widget::shader::{self, Action};
use iced::{Event, Rectangle, window};
use protocol::{InputEvent, button};
use tokio::sync::mpsc::UnboundedSender;

use crate::keymap;
use crate::session::{InputCmd, VideoFrame};

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VideoFrame({}x{} #{})", self.width, self.height, self.frame_id)
    }
}

/// Largest rectangle with the frame's aspect ratio centred inside `bounds`.
pub fn fit(bounds: Rectangle, w: usize, h: usize) -> Rectangle {
    let (bw, bh) = (bounds.width, bounds.height);
    let s = (bw / w as f32).min(bh / h as f32);
    let (dw, dh) = (w as f32 * s, h as f32 * s);
    Rectangle { x: bounds.x + (bw - dw) / 2.0, y: bounds.y + (bh - dh) / 2.0, width: dw, height: dh }
}

pub struct VideoView<M> {
    pub frame: Option<Arc<VideoFrame>>,
    pub input: Option<UnboundedSender<InputCmd>>,
    pub cursor_shape: Option<Arc<protocol::CursorShape>>,
    pub cursor_pos: Option<protocol::CursorPos>,
    /// published on Ctrl+Alt+Break (not forwarded to the remote side)
    pub on_toggle_fullscreen: Option<M>,
    /// the OS draws the remote pointer over the picture (Windows native cursor): do not composite it there
    pub native_pointer: bool,
}

#[derive(Default)]
pub struct ViewState {
    keys_down: HashSet<(u16, bool)>,
    buttons_down: u8,
    last_pos: Option<(u16, u16)>,
    /// sub-unit wheel remainder (Windows wheel units), so slow smooth scrolling is not lost
    wheel_acc: (f32, f32),
}

/// iced/winit scroll delta -> Windows wheel units (120 = one notch), positive = up / right.
/// winit: y > 0 = up; x > 0 = content moves right = scroll LEFT (inverted vs. Windows HWHEEL).
/// Pixel deltas (macOS, Wayland touchpads, already with OS inertia): 100 px = one notch.
pub fn wheel_units(delta: &mouse::ScrollDelta) -> (f32, f32) {
    let (x, y) = match *delta {
        mouse::ScrollDelta::Lines { x, y } => (x * 120.0, y * 120.0),
        mouse::ScrollDelta::Pixels { x, y } => (x * 1.2, y * 1.2),
    };
    (-x, y)
}

impl<M> VideoView<M> {
    fn remote_pos(&self, bounds: Rectangle, cursor: mouse::Cursor) -> Option<(u16, u16)> {
        let f = self.frame.as_ref()?;
        let p = cursor.position()?;
        let r = fit(bounds, f.width, f.height);
        if !r.contains(p) {
            return None;
        }
        let x = ((p.x - r.x) / r.width * f.width as f32).clamp(0.0, (f.width - 1) as f32);
        let y = ((p.y - r.y) / r.height * f.height as f32).clamp(0.0, (f.height - 1) as f32);
        Some((x as u16, y as u16))
    }

    /// Where to draw the pointer hotspot (remote pixels), or None when no pointer is shown.
    /// With input, the pointer follows the local one and only while that is over the picture: off
    /// it (letterbox bars, the full-screen connection bar on top) the local pointer is shown, and
    /// drawing the remote one at its last position as well made two pointers.
    fn pointer_at(&self, bounds: Rectangle, cursor: mouse::Cursor) -> Option<(i32, i32)> {
        let pos = self.cursor_pos?;
        self.cursor_shape.as_ref()?;
        if !pos.visible {
            return None;
        }
        if self.input.is_some() {
            return self.remote_pos(bounds, cursor).map(|(x, y)| (x as i32, y as i32));
        }
        Some((pos.x, pos.y))
    }
}

fn mouse_button(b: mouse::Button) -> Option<u8> {
    Some(match b {
        mouse::Button::Left => button::LEFT,
        mouse::Button::Right => button::RIGHT,
        mouse::Button::Middle => button::MIDDLE,
        mouse::Button::Back => button::X1,
        mouse::Button::Forward => button::X2,
        mouse::Button::Other(_) => return None,
    })
}

impl<Message: Clone> shader::Program<Message> for VideoView<Message> {
    type State = ViewState;
    type Primitive = VideoPrimitive;

    fn update(&self, state: &mut ViewState, event: &Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<Action<Message>> {
        // macOS: the native remote pointer is (re)applied on every mouse move, like Windows App
        #[cfg(target_os = "macos")]
        if self.native_pointer && matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorEntered | mouse::Event::CursorLeft)) {
            crate::mac_cursor::refresh();
        }
        let input = self.input.as_ref()?;
        let send = |e: InputEvent| {
            let _ = input.send(InputCmd::Event(e));
        };
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed { physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Pause), modifiers, .. })
                if modifiers.control() && modifiers.alt() && self.on_toggle_fullscreen.is_some() =>
            {
                // Ctrl+Alt+Break (mstsc): local full-screen toggle
                Some(Action::publish(self.on_toggle_fullscreen.clone().unwrap()).and_capture())
            }
            Event::Keyboard(keyboard::Event::KeyReleased { physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Pause), modifiers, .. })
                if modifiers.control() && modifiers.alt() && self.on_toggle_fullscreen.is_some() =>
            {
                Some(Action::capture())
            }
            Event::Keyboard(keyboard::Event::KeyPressed { physical_key, .. }) => {
                let (scancode, extended) = keymap::scancode(physical_key)?;
                state.keys_down.insert((scancode, extended));
                send(InputEvent::Key { scancode, extended, down: true });
                Some(Action::capture())
            }
            Event::Keyboard(keyboard::Event::KeyReleased { physical_key, .. }) => {
                let (scancode, extended) = keymap::scancode(physical_key)?;
                state.keys_down.remove(&(scancode, extended));
                send(InputEvent::Key { scancode, extended, down: false });
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let (x, y) = self.remote_pos(bounds, cursor)?;
                if state.last_pos != Some((x, y)) {
                    state.last_pos = Some((x, y));
                    let _ = input.send(InputCmd::Move { x, y });
                }
                // redraw now: the pointer is composited locally at the new position
                Some(Action::request_redraw().and_capture())
            }
            Event::Mouse(mouse::Event::ButtonPressed(b)) | Event::Mouse(mouse::Event::ButtonReleased(b)) => {
                let down = matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)));
                let button = mouse_button(*b)?;
                // presses only inside the picture; releases always (drag ending outside)
                let (x, y) = match self.remote_pos(bounds, cursor) {
                    Some(p) => p,
                    None if !down && state.buttons_down & (1 << button) != 0 => state.last_pos.unwrap_or((0, 0)),
                    None => return None,
                };
                if down {
                    state.buttons_down |= 1 << button;
                } else {
                    state.buttons_down &= !(1 << button);
                }
                send(InputEvent::Button { button, down, x, y });
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let (x, y) = self.remote_pos(bounds, cursor)?;
                let (h, v) = wheel_units(delta);
                state.wheel_acc.0 += h;
                state.wheel_acc.1 += v;
                let (dh, dv) = (state.wheel_acc.0.trunc(), state.wheel_acc.1.trunc());
                state.wheel_acc.0 -= dh;
                state.wheel_acc.1 -= dv;
                if dv != 0.0 {
                    send(InputEvent::Wheel { delta: dv.clamp(-32768.0, 32767.0) as i16, horizontal: false, x, y });
                }
                if dh != 0.0 {
                    send(InputEvent::Wheel { delta: dh.clamp(-32768.0, 32767.0) as i16, horizontal: true, x, y });
                }
                Some(Action::capture())
            }
            Event::Window(window::Event::Unfocused) => {
                // never leave keys/buttons stuck on the remote side
                if !state.keys_down.is_empty() || state.buttons_down != 0 {
                    send(InputEvent::ReleaseAll);
                    state.keys_down.clear();
                    state.buttons_down = 0;
                }
                None
            }
            _ => None,
        }
    }

    fn draw(&self, _state: &ViewState, cursor: mouse::Cursor, bounds: Rectangle) -> VideoPrimitive {
        let native_here = self.native_pointer && self.input.is_some() && self.remote_pos(bounds, cursor).is_some();
        let pointer = if native_here { None } else { self.pointer_at(bounds, cursor).and_then(|at| Some((self.cursor_shape.clone()?, at))) };
        VideoPrimitive { frame: self.frame.clone(), pointer }
    }

    fn mouse_interaction(&self, _state: &ViewState, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        // Hide the local pointer only while the shader actually draws the remote one over the
        // picture. If the remote pointer is reported hidden (e.g. a session without a mouse device,
        // where Windows hides the cursor) or nothing is known yet, keep the local pointer so the
        // user never ends up without any pointer.
        if !self.native_pointer && self.remote_pos(bounds, cursor).is_some() && self.pointer_at(bounds, cursor).is_some() {
            mouse::Interaction::Hidden
        } else {
            mouse::Interaction::default()
        }
    }
}

#[derive(Debug)]
pub struct VideoPrimitive {
    frame: Option<Arc<VideoFrame>>,
    /// pointer shape + hotspot position in remote pixels
    pointer: Option<(Arc<protocol::CursorShape>, (i32, i32))>,
}

const CURSOR_TEX: u32 = 256;

pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    cursor_tex: wgpu::Texture,
    cursor_view: wgpu::TextureView,
    cursor_uploaded: Option<u32>,
    uniforms: wgpu::Buffer,
    planes: Option<Planes>,
    uploaded: Option<u32>,
    target: Option<[f32; 4]>,
}

struct Planes {
    size: (u32, u32),
    tex: [wgpu::Texture; 3],
    bind_group: wgpu::BindGroup,
}

const SHADER: &str = r#"
@group(0) @binding(0) var ty: texture_2d<f32>;
@group(0) @binding(1) var tu: texture_2d<f32>;
@group(0) @binding(2) var tv: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;
@group(0) @binding(4) var cur: texture_2d<f32>;
struct U { rect: vec4<f32>, video: vec4<f32> };   // pointer x,y,w,h (video px); video w,h, enabled
@group(0) @binding(5) var<uniform> params: U;

struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex fn vs_main(@builtin(vertex_index) i: u32) -> VOut {
    var p = array<vec2<f32>, 4>(vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(-1.0, 1.0), vec2(1.0, 1.0));
    var t = array<vec2<f32>, 4>(vec2(0.0, 1.0), vec2(1.0, 1.0), vec2(0.0, 0.0), vec2(1.0, 0.0));
    var o: VOut;
    o.pos = vec4(p[i], 0.0, 1.0);
    o.uv = t[i];
    return o;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3(2.4)), c / 12.92, c <= vec3(0.04045));
}

@fragment fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let y = (textureSample(ty, smp, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (textureSample(tu, smp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (textureSample(tv, smp, in.uv).r - 128.0 / 255.0) * (255.0 / 224.0);
    var rgb = clamp(vec3(y + 1.5748 * v, y - 0.1873 * u - 0.4681 * v, y + 1.8556 * u), vec3(0.0), vec3(1.0));
    if (params.video.z > 0.5) {
        let p = in.uv * params.video.xy - params.rect.xy;
        if (p.x >= 0.0 && p.y >= 0.0 && p.x < params.rect.z && p.y < params.rect.w) {
            let c = textureLoad(cur, vec2<i32>(p), 0);
            if (c.a > 0.0) {
                rgb = mix(rgb, c.rgb, c.a);
            } else if (any(c.rgb > vec3(0.0))) {
                rgb = abs(rgb - c.rgb); // XOR pixel (white = invert)
            }
        }
    }
    if (SRGB_TARGET) { rgb = srgb_to_linear(rgb); }
    return vec4(rgb, 1.0);
}
"#;

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        // An sRGB render target re-encodes our (already gamma-encoded) output: linearise first.
        let src = SHADER.replace("SRGB_TARGET", if format.is_srgb() { "true" } else { "false" });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("fastrdp yuv"), source: wgpu::ShaderSource::Wgsl(src.into()) });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fastrdp yuv"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fastrdp yuv"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fastrdp yuv"),
            layout: Some(&pl),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::REPLACE), write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let cursor_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fastrdp cursor"),
            size: wgpu::Extent3d { width: CURSOR_TEX, height: CURSOR_TEX, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let cursor_view = cursor_tex.create_view(&Default::default());
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fastrdp cursor uniforms"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        VideoPipeline { pipeline, layout, sampler, cursor_tex, cursor_view, cursor_uploaded: None, uniforms, planes: None, uploaded: None, target: None }
    }
}

impl VideoPipeline {
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &VideoFrame) {
        let (w, h) = (f.width as u32, f.height as u32);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if self.planes.as_ref().map(|p| p.size) != Some((w, h)) {
            let mk = |w, h| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("fastrdp plane"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let tex = [mk(w, h), mk(cw, ch), mk(cw, ch)];
            let views: Vec<_> = tex.iter().map(|t| t.create_view(&Default::default())).collect();
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("fastrdp yuv"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&views[0]) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[1]) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&views[2]) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&self.cursor_view) },
                    wgpu::BindGroupEntry { binding: 5, resource: self.uniforms.as_entire_binding() },
                ],
            });
            self.planes = Some(Planes { size: (w, h), tex, bind_group });
        }
        let p = self.planes.as_ref().unwrap();
        for (i, (data, pw, ph)) in [(&f.y, w, h), (&f.u, cw, ch), (&f.v, cw, ch)].into_iter().enumerate() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &p.tex[i], mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(pw), rows_per_image: Some(ph) },
                wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
            );
        }
        self.uploaded = Some(f.frame_id);
    }

    fn upload_cursor(&mut self, queue: &wgpu::Queue, c: &protocol::CursorShape) {
        let (w, h) = (c.width as u32, c.height as u32);
        if w == 0 || h == 0 || w > CURSOR_TEX || h > CURSOR_TEX || c.rgba.len() < (w * h * 4) as usize {
            return;
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &self.cursor_tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &c.rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.cursor_uploaded = Some(c.id);
    }
}

impl shader::Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(&self, pipeline: &mut VideoPipeline, device: &wgpu::Device, queue: &wgpu::Queue, bounds: &Rectangle, viewport: &Viewport) {
        pipeline.target = None;
        let Some(f) = self.frame.as_ref() else { return };
        if pipeline.uploaded != Some(f.frame_id) || pipeline.planes.as_ref().map(|p| p.size) != Some((f.width as u32, f.height as u32)) {
            pipeline.upload(device, queue, f);
        }
        let mut u = [0f32; 8];
        u[4] = f.width as f32;
        u[5] = f.height as f32;
        if let Some((shape, (hx, hy))) = &self.pointer {
            if pipeline.cursor_uploaded != Some(shape.id) {
                pipeline.upload_cursor(queue, shape);
            }
            if pipeline.cursor_uploaded == Some(shape.id) {
                u[0] = (*hx - shape.hot_x as i32) as f32;
                u[1] = (*hy - shape.hot_y as i32) as f32;
                u[2] = shape.width as f32;
                u[3] = shape.height as f32;
                u[6] = 1.0;
            }
        }
        let bytes: Vec<u8> = u.iter().flat_map(|v| v.to_le_bytes()).collect();
        queue.write_buffer(&pipeline.uniforms, 0, &bytes);
        let s = viewport.scale_factor();
        let r = fit(*bounds, f.width, f.height);
        pipeline.target = Some([r.x * s, r.y * s, r.width * s, r.height * s]);
    }

    fn draw(&self, pipeline: &VideoPipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        if let (Some(p), Some(t)) = (pipeline.planes.as_ref(), pipeline.target) {
            pass.set_viewport(t[0], t[1], t[2].max(1.0), t[3].max(1.0), 0.0, 1.0);
            pass.set_pipeline(&pipeline.pipeline);
            pass.set_bind_group(0, &p.bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shader_is_valid_wgsl() {
        for srgb in ["true", "false"] {
            let src = super::SHADER.replace("SRGB_TARGET", srgb);
            let module = naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
            naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
                .validate(&module)
                .unwrap_or_else(|e| panic!("{e:?}"));
        }
    }

    #[test]
    fn local_pointer_is_hidden_only_while_the_remote_one_is_drawn() {
        use iced::widget::shader::Program;
        use protocol::{CursorPos, CursorShape};
        let frame = std::sync::Arc::new(crate::session::VideoFrame {
            width: 320,
            height: 180,
            y: vec![0; 320 * 180],
            u: vec![128; 80 * 90],
            v: vec![128; 80 * 90],
            frame_id: 1,
        });
        let shape = std::sync::Arc::new(CursorShape { id: 1, width: 1, height: 1, hot_x: 0, hot_y: 0, rgba: vec![255; 4] });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let bounds = iced::Rectangle { x: 0.0, y: 0.0, width: 640.0, height: 360.0 };
        let over = iced::mouse::Cursor::Available(iced::Point::new(100.0, 100.0));
        let outside = iced::mouse::Cursor::Unavailable;
        let view = |pos: Option<CursorPos>, shape: Option<std::sync::Arc<CursorShape>>| super::VideoView::<()> {
            frame: Some(frame.clone()),
            input: Some(tx.clone()),
            cursor_shape: shape,
            cursor_pos: pos,
            on_toggle_fullscreen: None,
            native_pointer: false,
        };
        let interaction = |v: &super::VideoView<()>, c| <super::VideoView<()> as Program<()>>::mouse_interaction(v, &super::ViewState::default(), bounds, c);
        let visible = Some(CursorPos { visible: true, x: 5, y: 5, shape_id: 1 });
        let hidden = Some(CursorPos { visible: false, x: 5, y: 5, shape_id: 1 });
        assert_eq!(interaction(&view(visible, Some(shape.clone())), over), iced::mouse::Interaction::Hidden);
        // remote pointer hidden (no mouse device in the session / app hid it): keep the local one
        assert_eq!(interaction(&view(hidden, Some(shape.clone())), over), iced::mouse::Interaction::default());
        // nothing known yet / position without shape
        assert_eq!(interaction(&view(None, None), over), iced::mouse::Interaction::default());
        assert_eq!(interaction(&view(visible, None), over), iced::mouse::Interaction::default());
        // outside the picture: always the local pointer, and the remote one is not drawn as well
        assert_eq!(interaction(&view(visible, Some(shape.clone())), outside), iced::mouse::Interaction::default());
        let drawn =
            |c| <super::VideoView<()> as Program<()>>::draw(&view(visible, Some(shape.clone())), &super::ViewState::default(), c, bounds).pointer.is_some();
        assert!(drawn(over) && !drawn(outside), "one pointer at a time");
    }

    #[test]
    fn wheel_conversion() {
        use iced::mouse::ScrollDelta;
        assert_eq!(super::wheel_units(&ScrollDelta::Lines { x: 0.0, y: 1.0 }), (-0.0, 120.0), "one notch up");
        assert_eq!(super::wheel_units(&ScrollDelta::Lines { x: 0.0, y: -0.25 }).1, -30.0, "high-resolution wheel");
        assert_eq!(super::wheel_units(&ScrollDelta::Lines { x: -1.0, y: 0.0 }).0, 120.0, "winit x<0 = scroll right = HWHEEL > 0");
        assert!((super::wheel_units(&ScrollDelta::Pixels { x: 0.0, y: 50.0 }).1 - 60.0).abs() < 0.01);
    }

    #[test]
    fn fit_letterboxes() {
        let r = super::fit(iced::Rectangle { x: 0.0, y: 0.0, width: 1000.0, height: 1000.0 }, 1920, 1080);
        assert!((r.width - 1000.0).abs() < 0.01 && r.x.abs() < 0.01);
        assert!((r.height - 562.5).abs() < 0.01 && (r.y - 218.75).abs() < 0.01);
    }
}
