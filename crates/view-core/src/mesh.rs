//! 3D models for the preview: STL (binary and text), 3MF and OBJ, read into one list of
//! triangles. 3MF is a zip of XML, opened with [`crate::zip`], including the "production" layout
//! Bambu Studio and PrusaSlicer write, where the shapes sit in files of their own. No OS calls:
//! drawing is the platform's business.

use std::collections::HashMap;

/// More than this many triangles and the rest are left out, to keep memory sensible.
pub const MAX_TRIANGLES: usize = 2_500_000;
/// The most shapes a 3MF file places, counting each copy of a part.
const MAX_PLACED: usize = 100_000;

pub struct Mesh {
    /// Three corners per triangle.
    pub corners: Vec<[f32; 3]>,
    pub min: [f32; 3],
    pub max: [f32; 3],
    /// Millimetres per unit of the coordinates: STL and OBJ don't say, and are taken as 1.
    pub mm_per_unit: f32,
    /// Triangles were left out past [`MAX_TRIANGLES`].
    pub cut: bool,
}

impl Mesh {
    fn new() -> Mesh {
        Mesh { corners: Vec::new(), min: [f32::MAX; 3], max: [f32::MIN; 3], mm_per_unit: 1.0, cut: false }
    }

    pub fn triangles(&self) -> usize {
        self.corners.len() / 3
    }

    /// Width, depth and height in millimetres.
    pub fn size_mm(&self) -> [f32; 3] {
        if self.corners.is_empty() {
            return [0.0; 3];
        }
        [0, 1, 2].map(|i| (self.max[i] - self.min[i]) * self.mm_per_unit)
    }

    fn push(&mut self, a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> bool {
        if self.corners.len() >= MAX_TRIANGLES * 3 {
            self.cut = true;
            return false;
        }
        for p in [a, b, c] {
            if !p.iter().all(|v| v.is_finite()) {
                return true; // a broken triangle: skip it, keep going
            }
        }
        for p in [a, b, c] {
            for i in 0..3 {
                self.min[i] = self.min[i].min(p[i]);
                self.max[i] = self.max[i].max(p[i]);
            }
            self.corners.push(p);
        }
        true
    }

    fn done(self) -> Result<Mesh, String> {
        if self.corners.is_empty() { Err("There's nothing to show in this model.".into()) } else { Ok(self) }
    }
}

/// Read a model, choosing the format by extension (`stl`, `3mf` or `obj`).
pub fn read(ext: &str, data: &[u8]) -> Result<Mesh, String> {
    match ext.to_ascii_lowercase().as_str() {
        "stl" => stl(data),
        "3mf" => three_mf(data),
        "obj" => obj(data),
        _ => Err("View can't read this kind of model.".into()),
    }
}

// ---- STL ----

pub fn stl(data: &[u8]) -> Result<Mesh, String> {
    let count = data.get(80..84).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    // Binary files say how many triangles they hold, which fixes their length. Some start with
    // "solid" just like text ones, so the length decides.
    let exact = count.is_some_and(|n| 84 + n * 50 == data.len());
    let text = data.len() >= 5 && data[..5].eq_ignore_ascii_case(b"solid") && !exact && looks_textual(&data[..data.len().min(1024)]);
    if !text {
        let Some(n) = count else { return Err("This STL file is empty.".into()) };
        let n = n.min((data.len() - 84) / 50);
        let mut mesh = Mesh::new();
        let f = |b: &[u8], i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        for t in 0..n {
            let r = &data[84 + t * 50..84 + t * 50 + 50];
            let v = |k: usize| [f(r, 12 + k * 12), f(r, 16 + k * 12), f(r, 20 + k * 12)];
            if !mesh.push(v(0), v(1), v(2)) {
                break;
            }
        }
        return mesh.done();
    }
    let s = String::from_utf8_lossy(data);
    let mut mesh = Mesh::new();
    let mut words = s.split_ascii_whitespace();
    let mut corner = Vec::with_capacity(3);
    while let Some(w) = words.next() {
        if !w.eq_ignore_ascii_case("vertex") {
            continue;
        }
        let mut p = [0f32; 3];
        for v in &mut p {
            *v = words.next().and_then(|x| x.parse().ok()).unwrap_or(f32::NAN);
        }
        corner.push(p);
        if corner.len() == 3 {
            if !mesh.push(corner[0], corner[1], corner[2]) {
                break;
            }
            corner.clear();
        }
    }
    mesh.done()
}

fn looks_textual(b: &[u8]) -> bool {
    b.iter().all(|&c| c == b'\n' || c == b'\r' || c == b'\t' || (32..127).contains(&c))
}

// ---- OBJ ----

pub fn obj(data: &[u8]) -> Result<Mesh, String> {
    if !looks_textual(&data[..data.len().min(512)]) {
        return Err("This isn't a 3D model. Files ending .obj can also be compiled code.".into());
    }
    let s = String::from_utf8_lossy(data);
    let mut points: Vec<[f32; 3]> = Vec::new();
    let mut mesh = Mesh::new();
    let mut face = Vec::new();
    for line in s.lines() {
        let mut w = line.split_ascii_whitespace();
        match w.next() {
            Some("v") => {
                let mut p = [0f32; 3];
                for v in &mut p {
                    *v = w.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
                }
                points.push(p);
            }
            Some("f") => {
                face.clear();
                for c in w {
                    // "7", "7/2", "7//3" or "7/2/3"; negative counts back from the latest.
                    let Some(i) = c.split('/').next().and_then(|i| i.parse::<i64>().ok()) else { continue };
                    let i = if i < 0 { points.len() as i64 + i } else { i - 1 };
                    if let Some(p) = usize::try_from(i).ok().and_then(|i| points.get(i)) {
                        face.push(*p);
                    }
                }
                // A fan, for faces with more than three corners.
                for k in 1..face.len().saturating_sub(1) {
                    if !mesh.push(face[0], face[k], face[k + 1]) {
                        return mesh.done();
                    }
                }
            }
            _ => {}
        }
    }
    mesh.done()
}

// ---- 3MF ----

use crate::zip::Entry;

fn zip_entries(data: &[u8]) -> Option<Vec<Entry>> {
    crate::zip::entries(data)
}

fn unzip(data: &[u8], e: &Entry) -> Result<Vec<u8>, String> {
    crate::zip::read(data, e).map_err(|_| if e.method == 0 || e.method == 8 { "This 3MF file is damaged.".to_string() } else { "This 3MF file is packed in a way View can't read.".to_string() })
}

/// A 3MF transform: the first three rows turn, the last moves. Points are rows: p × M.
type Transform = [f32; 12];
const IDENTITY: Transform = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];

fn parse_transform(s: Option<&str>) -> Transform {
    let Some(s) = s else { return IDENTITY };
    let v: Vec<f32> = s.split_ascii_whitespace().filter_map(|x| x.parse().ok()).collect();
    v.try_into().unwrap_or(IDENTITY)
}

fn apply(m: &Transform, p: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|c| p[0] * m[c] + p[1] * m[3 + c] + p[2] * m[6 + c] + m[9 + c])
}

/// `a` then `b`.
fn then(a: &Transform, b: &Transform) -> Transform {
    let mut m = [0f32; 12];
    for r in 0..4 {
        for c in 0..3 {
            let mut v = if r == 3 { b[9 + c] } else { 0.0 };
            for k in 0..3 {
                v += a[r * 3 + k] * b[k * 3 + c];
            }
            m[r * 3 + c] = v;
        }
    }
    m
}

#[derive(Default)]
struct Object {
    vertices: Vec<[f32; 3]>,
    triangles: Vec<[u32; 3]>,
    /// Other objects it's made of: which file (None for this one), which object, and where.
    components: Vec<(Option<String>, String, Transform)>,
}

#[derive(Default)]
struct Model {
    objects: HashMap<String, Object>,
    /// What gets built: as components are.
    build: Vec<(Option<String>, String, Transform)>,
    unit_mm: f32,
}

/// Elements and their attributes, in order. Enough XML for 3MF: no DTDs, and entities only
/// matter in names, which aren't used.
fn scan_xml(s: &str, mut each: impl FnMut(&str, &str, bool)) {
    let b = s.as_bytes();
    let mut i = 0;
    while let Some(k) = s[i..].find('<') {
        i += k + 1;
        match b.get(i) {
            Some(b'?') | Some(b'!') => {
                // Declarations and comments.
                let close = if s[i..].starts_with("!--") { "-->" } else { ">" };
                match s[i..].find(close) {
                    Some(e) => i += e + close.len(),
                    None => return,
                }
                continue;
            }
            Some(b'/') => {
                let Some(e) = s[i..].find('>') else { return };
                let name = local(s[i + 1..i + e].trim());
                each(name, "", true);
                i += e + 1;
                continue;
            }
            _ => {}
        }
        // The tag runs to the next '>' outside quotes.
        let mut j = i;
        let mut quote = 0u8;
        while j < b.len() {
            let c = b[j];
            if quote != 0 {
                if c == quote {
                    quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        let tag = &s[i..j.min(b.len())];
        let empty = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let name_end = tag.find(|c: char| c.is_ascii_whitespace()).unwrap_or(tag.len());
        each(local(&tag[..name_end]), &tag[name_end..], false);
        if empty {
            each(local(&tag[..name_end]), "", true);
        }
        i = (j + 1).min(b.len());
    }
}

/// A name without its namespace prefix.
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// An attribute's value, by its name without prefix.
fn attr<'a>(attrs: &'a str, want: &str) -> Option<&'a str> {
    let b = attrs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let k0 = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = &attrs[k0..i];
        while i < b.len() && (b[i] == b'=' || b[i].is_ascii_whitespace()) {
            i += 1;
        }
        let q = *b.get(i)?;
        if q != b'"' && q != b'\'' {
            return None;
        }
        let v0 = i + 1;
        let v1 = v0 + attrs[v0..].find(q as char)?;
        if local(key) == want {
            return Some(&attrs[v0..v1]);
        }
        i = v1 + 1;
    }
    None
}

fn num(a: &str, k: &str) -> f32 {
    attr(a, k).and_then(|v| v.trim().parse().ok()).unwrap_or(f32::NAN)
}

fn parse_model(xml: &str) -> Model {
    let mut m = Model { unit_mm: 1.0, ..Default::default() };
    let mut current: Option<(String, Object)> = None;
    let mut in_build = false;
    scan_xml(xml, |name, a, end| match (name, end) {
        ("model", false) => {
            m.unit_mm = match attr(a, "unit").unwrap_or("millimeter") {
                "micron" => 0.001,
                "centimeter" => 10.0,
                "inch" => 25.4,
                "foot" => 304.8,
                "meter" => 1000.0,
                _ => 1.0,
            };
        }
        ("object", false) => current = Some((attr(a, "id").unwrap_or("").to_string(), Object::default())),
        ("object", true) => {
            if let Some((id, o)) = current.take() {
                m.objects.insert(id, o);
            }
        }
        ("vertex", false) => {
            if let Some((_, o)) = &mut current {
                o.vertices.push([num(a, "x"), num(a, "y"), num(a, "z")]);
            }
        }
        ("triangle", false) => {
            if let Some((_, o)) = &mut current {
                let v = |k| attr(a, k).and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(u32::MAX);
                o.triangles.push([v("v1"), v("v2"), v("v3")]);
            }
        }
        ("component", false) => {
            if let Some((_, o)) = &mut current {
                o.components.push((attr(a, "path").map(str::to_string), attr(a, "objectid").unwrap_or("").to_string(), parse_transform(attr(a, "transform"))));
            }
        }
        ("build", false) => in_build = true,
        ("build", true) => in_build = false,
        ("item", false) if in_build => {
            m.build.push((attr(a, "path").map(str::to_string), attr(a, "objectid").unwrap_or("").to_string(), parse_transform(attr(a, "transform"))));
        }
        _ => {}
    });
    m
}

pub fn three_mf(data: &[u8]) -> Result<Mesh, String> {
    let entries = zip_entries(data).ok_or("This 3MF file is damaged.")?;
    let find = |name: &str| {
        let name = name.trim_start_matches('/');
        entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    };
    // The main model, as the package's relationships name it.
    let root = find("_rels/.rels")
        .and_then(|e| unzip(data, e).ok())
        .and_then(|rels| {
            let rels = String::from_utf8_lossy(&rels).into_owned();
            let mut target = None;
            scan_xml(&rels, |name, a, end| {
                if name == "Relationship" && !end && attr(a, "Type").is_some_and(|t| t.ends_with("/3dmodel")) && target.is_none() {
                    target = attr(a, "Target").map(|t| t.trim_start_matches('/').to_string());
                }
            });
            target
        })
        .or_else(|| find("3D/3dmodel.model").map(|e| e.name.clone()))
        .or_else(|| entries.iter().find(|e| e.name.to_ascii_lowercase().ends_with(".model")).map(|e| e.name.clone()))
        .ok_or("This 3MF file has no model in it.")?;

    let mut models: HashMap<String, Model> = HashMap::new();
    let load = |path: &str, models: &mut HashMap<String, Model>| -> bool {
        let key = path.trim_start_matches('/').to_ascii_lowercase();
        if models.contains_key(&key) {
            return true;
        }
        let Some(e) = find(&key) else { return false };
        let Ok(bytes) = unzip(data, e) else { return false };
        models.insert(key, parse_model(&String::from_utf8_lossy(&bytes)));
        true
    };
    if !load(&root, &mut models) {
        return Err("This 3MF file is damaged.".into());
    }
    let root_key = root.trim_start_matches('/').to_ascii_lowercase();
    let mut mesh = Mesh::new();
    mesh.mm_per_unit = models[&root_key].unit_mm;
    // At most as many shapes as can be placed (see below).
    let root_model = &models[&root_key];
    let mut build: Vec<_> = root_model.build.iter().take(MAX_PLACED).cloned().collect();
    if build.is_empty() {
        // Nothing to build: show every shape.
        build = root_model.objects.keys().take(MAX_PLACED).map(|id| (None, id.clone(), IDENTITY)).collect();
    }
    // Shapes inside shapes, depth first, with each level's placement applied on the way down.
    let mut stack: Vec<(String, String, Transform, u8)> = build.into_iter().rev().map(|(p, id, t)| (p.map_or(root_key.clone(), |p| p.trim_start_matches('/').to_ascii_lowercase()), id, t, 0)).collect();
    // A shape that holds itself several times over would branch without end, so the number
    // of shapes placed is capped as well as the depth.
    let mut placed = 0usize;
    while let Some((file, id, t, depth)) = stack.pop() {
        placed += 1;
        if placed > MAX_PLACED {
            break;
        }
        if depth > 16 || !load(&file, &mut models) {
            continue;
        }
        let Some(o) = models[&file].objects.get(&id) else { continue };
        for tri in &o.triangles {
            let (Some(a), Some(b), Some(c)) = (o.vertices.get(tri[0] as usize), o.vertices.get(tri[1] as usize), o.vertices.get(tri[2] as usize)) else { continue };
            if !mesh.push(apply(&t, *a), apply(&t, *b), apply(&t, *c)) {
                return mesh.done();
            }
        }
        for (p, cid, ct) in o.components.iter().rev().take(MAX_PLACED.saturating_sub(stack.len())) {
            let f = p.as_ref().map_or(file.clone(), |p| p.trim_start_matches('/').to_ascii_lowercase());
            stack.push((f, cid.clone(), then(ct, &t), depth + 1));
        }
    }
    mesh.done()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_stl(tris: &[[[f32; 3]; 3]]) -> Vec<u8> {
        let mut b = vec![0u8; 80];
        b[..5].copy_from_slice(b"solid"); // as some exporters do
        b.extend((tris.len() as u32).to_le_bytes());
        for t in tris {
            b.extend([0u8; 12]);
            for p in t {
                for v in p {
                    b.extend(v.to_le_bytes());
                }
            }
            b.extend([0u8; 2]);
        }
        b
    }

    #[test]
    fn binary_stl_even_when_it_says_solid() {
        let m = stl(&binary_stl(&[[[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 20.0, 5.0]]])).unwrap();
        assert_eq!(m.triangles(), 1);
        assert_eq!(m.size_mm(), [10.0, 20.0, 5.0]);
    }

    #[test]
    fn text_stl() {
        let s = "solid cube\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 0\n   vertex 1 0 0\n   vertex 1 1 2.5e0\n  endloop\n endfacet\nendsolid cube\n";
        let m = stl(s.as_bytes()).unwrap();
        assert_eq!(m.triangles(), 1);
        assert_eq!(m.max, [1.0, 1.0, 2.5]);
    }

    #[test]
    fn obj_faces_fan_and_count_back() {
        let s = "# square\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1/1/1 2/2/2 3/3/3 4/4/4\nf -4 -3 -2\n";
        assert_eq!(obj(s.as_bytes()).unwrap().triangles(), 3);
        assert!(obj(&[0x4C, 0x01, 0x00, 0x00, 0xFF]).is_err());
    }

    /// A zip holding `files`, the first stored and the rest deflated.
    fn zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut dir = Vec::new();
        for (i, (name, body)) in files.iter().enumerate() {
            let (method, packed) = if i == 0 { (0u16, body.as_bytes().to_vec()) } else { (8u16, miniz_oxide::deflate::compress_to_vec(body.as_bytes(), 6)) };
            let offset = out.len() as u32;
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20, 0, 0, 0]);
            out.extend(method.to_le_bytes());
            out.extend([0u8; 8]); // time, date, crc
            out.extend((packed.len() as u32).to_le_bytes());
            out.extend((body.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(&packed);
            dir.extend(0x0201_4b50u32.to_le_bytes());
            dir.extend([20, 0, 20, 0, 0, 0]);
            dir.extend(method.to_le_bytes());
            dir.extend([0u8; 8]);
            dir.extend((packed.len() as u32).to_le_bytes());
            dir.extend((body.len() as u32).to_le_bytes());
            dir.extend((name.len() as u16).to_le_bytes());
            dir.extend([0u8; 12]); // extra, comment, disk, attributes
            dir.extend(offset.to_le_bytes());
            dir.extend(name.as_bytes());
        }
        let at = out.len() as u32;
        out.extend(&dir);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((dir.len() as u32).to_le_bytes());
        out.extend(at.to_le_bytes());
        out.extend([0u8; 2]);
        out
    }

    #[test]
    fn three_mf_with_parts_in_other_files() {
        let rels = r#"<?xml version="1.0"?><Relationships><Relationship Target="/3D/3dmodel.model" Id="r0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/></Relationships>"#;
        let root = r#"<?xml version="1.0" encoding="UTF-8"?>
            <model unit="centimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02" xmlns:p="http://schemas.microsoft.com/3dmanufacturing/production/2015/06">
            <resources><object id="2" type="model"><components>
              <component p:path="/3D/Objects/part.model" objectid="1" transform="1 0 0 0 1 0 0 0 1 5 0 0"/>
            </components></object></resources>
            <!-- placed on the plate -->
            <build><item objectid="2" transform="2 0 0 0 2 0 0 0 2 100 100 0"/></build></model>"#;
        let part = r#"<model><resources><object id="1"><mesh>
            <vertices><vertex x="0" y="0" z="0"/><vertex x="1" y="0" z="0"/><vertex x='0' y='1' z='3'/></vertices>
            <triangles><triangle v1="0" v2="1" v3="2"/></triangles></mesh></object></resources></model>"#;
        let data = zip(&[("_rels/.rels", rels), ("3D/3dmodel.model", root), ("3D/Objects/part.model", part)]);
        let m = three_mf(&data).unwrap();
        assert_eq!(m.triangles(), 1);
        // Moved 5 along, then doubled and put at (100, 100).
        assert_eq!(m.min, [110.0, 100.0, 0.0]);
        assert_eq!(m.max, [112.0, 102.0, 6.0]);
        assert_eq!(m.size_mm(), [20.0, 20.0, 60.0]);
    }

    #[test]
    fn a_shape_holding_itself_stops() {
        // Ten copies of itself at every level would be 10^16 shapes before the depth cap.
        let parts = "<component objectid=\"1\"/>".repeat(10);
        let root = format!("<model><resources><object id=\"1\"><components>{parts}</components></object></resources><build><item objectid=\"1\"/></build></model>");
        let data = zip(&[("3D/3dmodel.model", &root)]);
        let started = std::time::Instant::now();
        let _ = three_mf(&data);
        assert!(started.elapsed().as_secs() < 5);
    }

    #[test]
    fn transforms_compose_in_order() {
        let turn: Transform = [0.0, 1.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]; // x → y
        let shift: Transform = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 10.0, 0.0, 0.0];
        assert_eq!(apply(&then(&turn, &shift), [1.0, 0.0, 0.0]), [10.0, 1.0, 0.0]);
        assert_eq!(apply(&then(&shift, &turn), [1.0, 0.0, 0.0]), [0.0, 11.0, 0.0]);
    }
}
