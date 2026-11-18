//! 3D models in the Space preview: STL, 3MF and OBJ. The file is read and turned into triangles
//! on a thread of its own; Direct3D draws them, lit, into a texture of the window's size, and
//! Direct2D puts that on screen like a picture. Drag to turn the model, the wheel zooms.
//!
//! It's drawn again only when the view changes, so a still model costs nothing.

use crate::gfx::Gpu;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use view_core::mesh::{self, Mesh};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{PCSTR, s};

/// Posted when a model has been read.
pub const WM_MODEL: u32 = WM_APP + 7;

/// The view's starting angles, in radians: a three-quarter view from the front, a little above.
const YAW: f32 = -0.6;
const PITCH: f32 = 0.45;
/// The camera's field of view, top to bottom.
const FOV: f32 = 0.5;
/// Vertices per buffer: one buffer that's too big can fail on a small GPU.
const PER_BUFFER: usize = 3 * 1_000_000;

struct Loaded {
    /// Position then normal, six floats per corner, centred on the model.
    vertices: Vec<f32>,
    /// The farthest corner from the centre.
    radius: f32,
    info: String,
}

enum State {
    Loading(Receiver<Result<Loaded, String>>),
    Ready { info: String, radius: f32, vertices: Option<Vec<f32>>, scene: Option<Scene> },
    Failed(String),
}

pub struct Model {
    pub path: PathBuf,
    hwnd: isize,
    state: State,
    yaw: f32,
    pitch: f32,
    zoom: f32,
    /// The theme it was last drawn for.
    dark: bool,
    /// The view or the colours changed since the last drawing.
    dirty: bool,
}

impl Model {
    pub fn open(path: &Path, hwnd: HWND) -> Model {
        let mut m = Model { path: path.to_path_buf(), hwnd: hwnd.0 as isize, state: State::Failed(String::new()), yaw: YAW, pitch: PITCH, zoom: 1.0, dark: true, dirty: true };
        m.load();
        m
    }

    fn load(&mut self) {
        let (send, recv) = channel();
        let path = self.path.clone();
        let hwnd = self.hwnd;
        let _ = std::thread::Builder::new().name("model".into()).spawn(move || {
            let result = read(&path);
            if send.send(result).is_ok() {
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_MODEL, WPARAM(0), LPARAM(0));
                }
            }
        });
        self.state = State::Loading(recv);
    }

    /// Take the model once it's read. True if something changed.
    pub fn poll(&mut self) -> bool {
        let State::Loading(rx) = &self.state else { return false };
        self.state = match rx.try_recv() {
            Ok(Ok(l)) => State::Ready { info: l.info, radius: l.radius, vertices: Some(l.vertices), scene: None },
            Ok(Err(e)) => State::Failed(e),
            Err(TryRecvError::Disconnected) => State::Failed("View couldn't read this model.".into()),
            Err(TryRecvError::Empty) => return false,
        };
        self.dirty = true;
        true
    }

    pub fn failed(&self) -> Option<&str> {
        match &self.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// "54,076 triangles · 55.8 × 71.7 × 18.2 mm"
    pub fn info(&self) -> Option<&str> {
        match &self.state {
            State::Ready { info, .. } => Some(info),
            _ => None,
        }
    }

    /// Turn by a drag of `dx`, `dy` pixels.
    pub fn turn(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.008;
        self.pitch = (self.pitch + dy * 0.008).clamp(-1.55, 1.55);
        self.dirty = true;
    }

    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.25, 40.0);
        self.dirty = true;
    }

    pub fn reset_view(&mut self) {
        (self.yaw, self.pitch, self.zoom) = (YAW, PITCH, 1.0);
        self.dirty = true;
    }

    /// The theme changed, or the GPU was replaced: draw again, from the file if need be.
    pub fn reset(&mut self, device_lost: bool) {
        self.dirty = true;
        if device_lost && matches!(self.state, State::Ready { .. }) {
            self.load();
        }
    }

    /// Draw the model into its texture if anything changed. Call before Direct2D starts the frame.
    pub fn prepare(&mut self, device: &ID3D11Device, gpu: &Gpu, width: u32, height: u32, dark: bool) {
        if dark != self.dark {
            self.dark = dark;
            self.dirty = true;
        }
        let (yaw, pitch, zoom) = (self.yaw, self.pitch, self.zoom);
        let State::Ready { radius, vertices, scene, .. } = &mut self.state else { return };
        if scene.is_none() {
            let Some(v) = vertices.take() else { return };
            match Scene::new(device, &v) {
                Some(s) => *scene = Some(s),
                None => {
                    self.state = State::Failed("View couldn't draw this model on this graphics card.".into());
                    return;
                }
            }
        }
        let Some(scene) = scene else { return };
        let resized = scene.target.as_ref().is_none_or(|t| (t.width, t.height) != (width, height));
        if resized {
            scene.target = Target::new(device, gpu, width, height);
        }
        if (self.dirty || resized) && scene.target.is_some() {
            scene.draw(*radius, yaw, pitch, zoom, dark);
            self.dirty = false;
        }
    }

    pub fn bitmap(&self) -> Option<&ID2D1Bitmap1> {
        match &self.state {
            State::Ready { scene: Some(s), .. } => s.target.as_ref().map(|t| &t.bitmap),
            _ => None,
        }
    }
}

fn read(path: &Path) -> Result<Loaded, String> {
    // Past this, it would be more triangles than the preview shows anyway, at a cost in memory.
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 1 << 30) {
        return Err("This model is too big to preview.".into());
    }
    let data = std::fs::read(path).map_err(|e| format!("View couldn't read this file: {e}"))?;
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let mesh = mesh::read(&ext, &data)?;
    drop(data);
    Ok(vertices(&mesh))
}

/// Corners with their triangle's normal, centred, for flat shading the way slicers show models.
fn vertices(m: &Mesh) -> Loaded {
    let c = [0, 1, 2].map(|i| (m.min[i] + m.max[i]) / 2.0);
    let mut out = Vec::with_capacity(m.corners.len() * 6);
    let mut radius: f32 = 0.0;
    for t in m.corners.chunks_exact(3) {
        let [a, b, d] = [t[0], t[1], t[2]].map(|p| [p[0] - c[0], p[1] - c[1], p[2] - c[2]]);
        let (u, v) = ([b[0] - a[0], b[1] - a[1], b[2] - a[2]], [d[0] - a[0], d[1] - a[1], d[2] - a[2]]);
        let n = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let n = if len > 0.0 { n.map(|x| x / len) } else { [0.0, 0.0, 1.0] };
        for p in [a, b, d] {
            radius = radius.max((p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt());
            out.extend_from_slice(&[p[0], p[1], p[2], n[0], n[1], n[2]]);
        }
    }
    let [w, d, h] = m.size_mm();
    let size = |v: f32| if v >= 1000.0 { format!("{v:.0}") } else if v >= 10.0 { format!("{v:.1}") } else { format!("{v:.2}") };
    let mut info = format!("{} triangles  ·  {} × {} × {} mm", group(m.triangles()), size(w), size(d), size(h));
    if m.cut {
        info.push_str("  ·  the rest left out");
    }
    Loaded { vertices: out, radius: radius.max(1e-6), info }
}

fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ---- Drawing ----

const SHADER: &str = r#"
cbuffer View : register(b0) {
    row_major float4x4 world;
    row_major float4x4 proj;
    float4 colour;
};
struct In { float3 p : POSITION; float3 n : NORMAL; };
struct Out { float4 p : SV_POSITION; float3 n : NORMAL; float3 v : TEXCOORD0; };
Out vs(In i) {
    Out o;
    float4 w = mul(float4(i.p, 1), world);
    o.v = w.xyz;
    o.p = mul(w, proj);
    o.n = mul(float4(i.n, 0), world).xyz;
    return o;
}
float4 ps(Out i) : SV_TARGET {
    float3 n = normalize(i.n);
    float3 e = normalize(-i.v);
    // Files often get their triangles' winding wrong: light whichever side faces the eye.
    if (dot(n, e) < 0) n = -n;
    float3 key = normalize(float3(-0.5, 0.8, -0.6));
    float3 fill = normalize(float3(0.7, -0.2, -0.6));
    float d = saturate(dot(n, key)) * 0.72 + saturate(dot(n, fill)) * 0.22;
    float spec = pow(saturate(dot(n, normalize(key + e))), 48) * 0.22;
    float rim = pow(1 - saturate(dot(n, e)), 3) * 0.12;
    return float4(colour.rgb * (0.26 + d) + spec + rim, 1);
}
"#;

#[repr(C)]
struct Constants {
    world: [f32; 16],
    proj: [f32; 16],
    colour: [f32; 4],
}

struct Scene {
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    buffers: Vec<(ID3D11Buffer, u32)>,
    constants: ID3D11Buffer,
    raster: ID3D11RasterizerState,
    depth: ID3D11DepthStencilState,
    target: Option<Target>,
}

struct Target {
    width: u32,
    height: u32,
    rtv: ID3D11RenderTargetView,
    colour: ID3D11Texture2D,
    dsv: ID3D11DepthStencilView,
    resolved: ID3D11Texture2D,
    bitmap: ID2D1Bitmap1,
}

fn compile(entry: PCSTR, target: PCSTR) -> Option<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let ok = unsafe { D3DCompile(SHADER.as_ptr() as *const _, SHADER.len(), PCSTR::null(), None, None::<&ID3DInclude>, entry, target, D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &mut code, Some(&mut errors)) };
    if ok.is_err() {
        if let Some(e) = errors {
            let msg = unsafe { std::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize()) };
            crate::util::trace(&format!("shader: {}", String::from_utf8_lossy(msg)));
        }
        return None;
    }
    let code = code?;
    Some(unsafe { std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()) }.to_vec())
}

impl Scene {
    fn new(device: &ID3D11Device, vertices: &[f32]) -> Option<Scene> {
        unsafe {
            let vs_code = compile(s!("vs"), s!("vs_4_0"))?;
            let ps_code = compile(s!("ps"), s!("ps_4_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vs_code, None, Some(&mut vs)).ok()?;
            let mut ps = None;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps)).ok()?;
            let elements = [
                D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("POSITION"), SemanticIndex: 0, Format: DXGI_FORMAT_R32G32B32_FLOAT, InputSlot: 0, AlignedByteOffset: 0, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, InstanceDataStepRate: 0 },
                D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("NORMAL"), SemanticIndex: 0, Format: DXGI_FORMAT_R32G32B32_FLOAT, InputSlot: 0, AlignedByteOffset: 12, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, InstanceDataStepRate: 0 },
            ];
            let mut layout = None;
            device.CreateInputLayout(&elements, &vs_code, Some(&mut layout)).ok()?;
            let mut buffers = Vec::new();
            for chunk in vertices.chunks(PER_BUFFER * 6) {
                let desc = D3D11_BUFFER_DESC { ByteWidth: (chunk.len() * 4) as u32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32, ..Default::default() };
                let init = D3D11_SUBRESOURCE_DATA { pSysMem: chunk.as_ptr() as *const _, ..Default::default() };
                let mut b = None;
                device.CreateBuffer(&desc, Some(&init), Some(&mut b)).ok()?;
                buffers.push((b?, (chunk.len() / 6) as u32));
            }
            let desc = D3D11_BUFFER_DESC { ByteWidth: size_of::<Constants>() as u32, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let mut constants = None;
            device.CreateBuffer(&desc, None, Some(&mut constants)).ok()?;
            let rd = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: true.into(), MultisampleEnable: true.into(), AntialiasedLineEnable: false.into(), ..Default::default() };
            let mut raster = None;
            device.CreateRasterizerState(&rd, Some(&mut raster)).ok()?;
            let dd = D3D11_DEPTH_STENCIL_DESC { DepthEnable: true.into(), DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL, DepthFunc: D3D11_COMPARISON_LESS, ..Default::default() };
            let mut depth = None;
            device.CreateDepthStencilState(&dd, Some(&mut depth)).ok()?;
            Some(Scene { context: device.GetImmediateContext().ok()?, vs: vs?, ps: ps?, layout: layout?, buffers, constants: constants?, raster: raster?, depth: depth?, target: None })
        }
    }

    fn draw(&self, radius: f32, yaw: f32, pitch: f32, zoom: f32, dark: bool) {
        let Some(t) = &self.target else { return };
        let aspect = t.width as f32 / t.height.max(1) as f32;
        // Far enough back that the whole model fits, across the narrower way.
        let half = ((FOV / 2.0).tan() * aspect.min(1.0)).atan();
        let dist = radius / half.sin() * 1.06 / zoom;
        let (near, far) = ((dist - radius * 1.5).max(dist * 0.002), dist + radius * 1.5);
        // Models are Z-up; the camera looks along +Z with +Y up. Swapping Y and Z does both.
        let swap = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let (sy, cy) = yaw.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        let ry = [cy, 0.0, -sy, 0.0, 0.0, 1.0, 0.0, 0.0, sy, 0.0, cy, 0.0, 0.0, 0.0, 0.0, 1.0];
        let rx = [1.0, 0.0, 0.0, 0.0, 0.0, cp, -sp, 0.0, 0.0, sp, cp, 0.0, 0.0, 0.0, 0.0, 1.0];
        let back = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, dist, 1.0];
        let world = mul(&mul(&mul(&swap, &ry), &rx), &back);
        let ys = 1.0 / (FOV / 2.0).tan();
        let q = far / (far - near);
        let proj = [ys / aspect, 0.0, 0.0, 0.0, 0.0, ys, 0.0, 0.0, 0.0, 0.0, q, 1.0, 0.0, 0.0, -near * q, 0.0];
        let colour = if dark { [0.70, 0.73, 0.78, 1.0] } else { [0.62, 0.66, 0.72, 1.0] };
        let constants = Constants { world, proj, colour };
        unsafe {
            let c = &self.context;
            c.UpdateSubresource(&self.constants, 0, None, &constants as *const _ as *const _, 0, 0);
            c.ClearRenderTargetView(&t.rtv, &[0.0, 0.0, 0.0, 0.0]);
            c.ClearDepthStencilView(&t.dsv, D3D11_CLEAR_DEPTH.0, 1.0, 0);
            c.OMSetRenderTargets(Some(&[Some(t.rtv.clone())]), &t.dsv);
            c.OMSetDepthStencilState(&self.depth, 0);
            c.OMSetBlendState(None, None, 0xFFFF_FFFF);
            let vp = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: t.width as f32, Height: t.height as f32, MinDepth: 0.0, MaxDepth: 1.0 };
            c.RSSetViewports(Some(&[vp]));
            c.RSSetState(&self.raster);
            c.IASetInputLayout(&self.layout);
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            let stride = 24u32;
            let offset = 0u32;
            for (b, count) in &self.buffers {
                c.IASetVertexBuffers(0, 1, Some(&Some(b.clone())), Some(&stride), Some(&offset));
                c.Draw(*count, 0);
            }
            c.OMSetRenderTargets(None, None);
            c.ResolveSubresource(&t.resolved, 0, &t.colour, 0, DXGI_FORMAT_B8G8R8A8_UNORM);
            c.Flush();
        }
    }
}

fn mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut m = [0f32; 16];
    for r in 0..4 {
        for c in 0..4 {
            m[r * 4 + c] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum();
        }
    }
    m
}

impl Target {
    fn new(device: &ID3D11Device, gpu: &Gpu, width: u32, height: u32) -> Option<Target> {
        let (width, height) = (width.max(1), height.max(1));
        let samples = [8u32, 4, 2].into_iter().find(|&n| unsafe { device.CheckMultisampleQualityLevels(DXGI_FORMAT_B8G8R8A8_UNORM, n) }.is_ok_and(|q| q > 0)).unwrap_or(1);
        let desc = |format, samples, bind: u32| D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: samples, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe {
            let mut colour = None;
            device.CreateTexture2D(&desc(DXGI_FORMAT_B8G8R8A8_UNORM, samples, D3D11_BIND_RENDER_TARGET.0 as u32), None, Some(&mut colour)).ok()?;
            let colour = colour?;
            let mut rtv = None;
            device.CreateRenderTargetView(&colour, None, Some(&mut rtv)).ok()?;
            let mut depth = None;
            device.CreateTexture2D(&desc(DXGI_FORMAT_D32_FLOAT, samples, D3D11_BIND_DEPTH_STENCIL.0 as u32), None, Some(&mut depth)).ok()?;
            let mut dsv = None;
            device.CreateDepthStencilView(&depth?, None, Some(&mut dsv)).ok()?;
            let mut resolved = None;
            device.CreateTexture2D(&desc(DXGI_FORMAT_B8G8R8A8_UNORM, 1, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32), None, Some(&mut resolved)).ok()?;
            let resolved = resolved?;
            let bitmap = gpu.wrap_texture_premultiplied(&resolved)?;
            Some(Target { width, height, rtv: rtv?, colour, dsv: dsv?, resolved, bitmap })
        }
    }
}
