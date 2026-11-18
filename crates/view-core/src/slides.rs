//! PowerPoint slides as they look, for the preview: where each shape, picture, table and text
//! box sits, in what colours, fonts and sizes. A slide only says what's different about it, so
//! the rest is filled in the way PowerPoint does: from the slide's layout, the layout's master,
//! the master's theme, and the deck's defaults. Left out: charts and SmartArt (a labelled box
//! stands in), effects, rotation, and shapes beyond rectangles, rounded rectangles, ovals and
//! lines (other shapes are drawn as their box).

use crate::office::{attr_ns, relationships};
use crate::xml::{self, Event};
use crate::zip;
use std::collections::HashMap;

/// Points to EMUs, the unit slides are measured in.
pub const EMU_PER_PT: f32 = 12700.0;
const MAX_SLIDES: usize = 500;
const MAX_PICTURES: usize = 400;
const MAX_PICTURE_BYTES: usize = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba { r: 0, g: 0, b: 0, a: 1.0 };
    pub const WHITE: Rgba = Rgba { r: 255, g: 255, b: 255, a: 1.0 };
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Geom {
    Rect,
    /// A rounded rectangle; the corner's radius as a share of the shorter side.
    Round(f32),
    Ellipse,
    /// A line from one corner of the box to the other.
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Paint {
    Colour(Rgba),
    /// One of [`Deck::pictures`].
    Picture(usize),
}

pub struct Deck {
    /// The slides' size, in EMUs.
    pub width: f32,
    pub height: f32,
    pub slides: Vec<Slide>,
    /// The pictures the slides use, as stored (PNG, JPEG and so on).
    pub pictures: Vec<Vec<u8>>,
}

pub struct Slide {
    pub background: Paint,
    /// In the order they're drawn: the master's, the layout's, then the slide's own.
    pub items: Vec<Item>,
}

pub struct Item {
    /// Left, top, width and height, in EMUs.
    pub rect: [f32; 4],
    /// Turned over sideways, and upside down: for lines.
    pub flip: (bool, bool),
    pub kind: ItemKind,
}

pub enum ItemKind {
    Shape { geom: Geom, fill: Option<Paint>, line: Option<(Rgba, f32)> },
    /// A picture, and the share cut off each side of it: left, top, right, bottom.
    Picture { index: usize, crop: [f32; 4] },
    Text(TextBox),
    /// Something View doesn't draw, such as a chart: a box with what it is.
    Missing(&'static str),
}

pub struct TextBox {
    /// Space inside the box: left, top, right, bottom, in EMUs.
    pub inset: [f32; 4],
    pub anchor: Anchor,
    pub wrap: bool,
    /// The text shrinks to fit the box ("Shrink text on overflow").
    pub shrink: bool,
    /// What it's already shrunk to: 1 for full size.
    pub scale: f32,
    pub paras: Vec<Para>,
}

pub struct Para {
    pub align: Align,
    /// From the box's left inset to the text, in EMUs.
    pub margin: f32,
    /// The first line's indent from `margin`, in EMUs: negative hangs the bullet out.
    pub indent: f32,
    pub bullet: Option<Bullet>,
    /// Space before it, in points.
    pub before: f32,
    /// Line spacing, as a share of single.
    pub line: f32,
    pub runs: Vec<TextRun>,
    /// Its size in points when it has no text: an empty line still takes room.
    pub size: f32,
}

pub struct Bullet {
    pub text: String,
    pub colour: Rgba,
    pub size: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextRun {
    pub text: String,
    /// In points.
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub colour: Rgba,
    pub font: String,
}

impl Para {
    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

// ---- A small tree of the XML ----

#[derive(Default, Debug)]
struct Node {
    name: String,
    attrs: String,
    kids: Vec<Node>,
    text: String,
}

impl Node {
    fn parse(s: &str) -> Node {
        let mut stack: Vec<Node> = vec![Node::default()];
        xml::walk(s, |e| match e {
            Event::Start(n, a) => stack.push(Node { name: n.to_string(), attrs: a.to_string(), ..Node::default() }),
            Event::End(_) => {
                if stack.len() > 1 {
                    let n = stack.pop().unwrap();
                    stack.last_mut().unwrap().kids.push(n);
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t);
                }
            }
        });
        while stack.len() > 1 {
            let n = stack.pop().unwrap();
            stack.last_mut().unwrap().kids.push(n);
        }
        stack.pop().unwrap_or_default()
    }

    /// The document's own element, under the root.
    fn top(&self) -> Option<&Node> {
        self.kids.iter().find(|k| !k.name.is_empty())
    }

    fn get(&self, name: &str) -> Option<&Node> {
        self.kids.iter().find(|k| k.name == name)
    }

    fn path(&self, names: &[&str]) -> Option<&Node> {
        names.iter().try_fold(self, |n, x| n.get(x))
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.kids.iter().filter(move |k| k.name == name)
    }

    fn attr(&self, a: &str) -> Option<String> {
        xml::attr(&self.attrs, a).map(|c| c.into_owned())
    }

    fn num(&self, a: &str) -> Option<f32> {
        self.attr(a)?.trim().parse().ok()
    }

    fn flag(&self, a: &str) -> Option<bool> {
        self.attr(a).map(|v| matches!(v.as_str(), "1" | "true" | "on"))
    }
}

// ---- Colours ----

struct Theme {
    scheme: HashMap<String, Rgba>,
    major: String,
    minor: String,
}

impl Theme {
    fn read(root: Option<&Node>) -> Theme {
        let mut scheme = HashMap::new();
        let mut t = Theme { scheme: HashMap::new(), major: "Calibri Light".into(), minor: "Calibri".into() };
        let Some(el) = root.and_then(|r| r.top()).and_then(|r| r.get("themeElements")) else {
            t.scheme = default_scheme();
            return t;
        };
        if let Some(cs) = el.get("clrScheme") {
            for k in &cs.kids {
                let c = k.kids.iter().find_map(|c| match c.name.as_str() {
                    "srgbClr" => c.attr("val").and_then(|v| hex(&v)),
                    "sysClr" => c.attr("lastClr").and_then(|v| hex(&v)).or(Some(if c.attr("val").as_deref() == Some("window") { Rgba::WHITE } else { Rgba::BLACK })),
                    _ => None,
                });
                if let Some(c) = c {
                    scheme.insert(k.name.clone(), c);
                }
            }
        }
        if let Some(fs) = el.get("fontScheme") {
            if let Some(f) = fs.path(&["majorFont", "latin"]).and_then(|l| l.attr("typeface")).filter(|f| !f.is_empty()) {
                t.major = f;
            }
            if let Some(f) = fs.path(&["minorFont", "latin"]).and_then(|l| l.attr("typeface")).filter(|f| !f.is_empty()) {
                t.minor = f;
            }
        }
        t.scheme = if scheme.is_empty() { default_scheme() } else { scheme };
        t
    }
}

/// Office's own colours, for a deck without a theme.
fn default_scheme() -> HashMap<String, Rgba> {
    [("dk1", "000000"), ("lt1", "FFFFFF"), ("dk2", "44546A"), ("lt2", "E7E6E6"), ("accent1", "4472C4"), ("accent2", "ED7D31"), ("accent3", "A5A5A5"), ("accent4", "FFC000"), ("accent5", "5B9BD5"), ("accent6", "70AD47"), ("hlink", "0563C1"), ("folHlink", "954F72")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), hex(v).unwrap()))
        .collect()
}

/// A master's theme and colour map: what "tx1" and "accent2" mean on its slides.
struct Ctx {
    theme: Theme,
    map: HashMap<String, String>,
}

impl Ctx {
    fn scheme(&self, name: &str) -> Option<Rgba> {
        let n = self.map.get(name).map(String::as_str).unwrap_or(match name {
            "tx1" => "dk1",
            "bg1" => "lt1",
            "tx2" => "dk2",
            "bg2" => "lt2",
            _ => name,
        });
        self.theme.scheme.get(n).copied()
    }

    fn text(&self) -> Rgba {
        self.scheme("tx1").unwrap_or(Rgba::BLACK)
    }

    /// "+mj-lt" and "+mn-lt" are the theme's heading and body fonts.
    fn font(&self, name: &str) -> String {
        if name.starts_with("+mj") {
            self.theme.major.clone()
        } else if name.starts_with("+mn") {
            self.theme.minor.clone()
        } else {
            name.to_string()
        }
    }
}

fn hex(s: &str) -> Option<Rgba> {
    let s = s.trim();
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Rgba { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 1.0 })
}

fn to_hsl(c: Rgba) -> (f32, f32, f32) {
    let (r, g, b) = (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    if max - min < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn from_hsl(h: f32, s: f32, l: f32, a: f32) -> Rgba {
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |t: f32| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    Rgba { r: f(h + 1.0 / 3.0), g: f(h), b: f(h - 1.0 / 3.0), a }
}

/// A colour element (`srgbClr`, `schemeClr` and the rest) with its changes applied: lighter,
/// darker, see-through. `ph` stands in for "phClr", the colour a style is used with.
fn colour(n: &Node, ctx: &Ctx, ph: Option<Rgba>) -> Option<Rgba> {
    let mut c = match n.name.as_str() {
        "srgbClr" => hex(&n.attr("val")?)?,
        "schemeClr" => {
            let v = n.attr("val")?;
            if v == "phClr" { ph.unwrap_or_else(|| ctx.text()) } else { ctx.scheme(&v)? }
        }
        "sysClr" => n.attr("lastClr").and_then(|v| hex(&v)).unwrap_or(if n.attr("val").as_deref() == Some("window") { Rgba::WHITE } else { Rgba::BLACK }),
        "prstClr" => match n.attr("val")?.as_str() {
            "white" => Rgba::WHITE,
            "black" => Rgba::BLACK,
            "red" => hex("FF0000")?,
            "green" => hex("008000")?,
            "blue" => hex("0000FF")?,
            "yellow" => hex("FFFF00")?,
            "gray" | "grey" => hex("808080")?,
            "orange" => hex("FFA500")?,
            _ => Rgba::BLACK,
        },
        "scrgbClr" => {
            let ch = |k: &str| {
                let v = (n.num(k).unwrap_or(0.0) / 100000.0).clamp(0.0, 1.0);
                // Linear light to sRGB.
                let s = if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
                (s * 255.0).round() as u8
            };
            Rgba { r: ch("r"), g: ch("g"), b: ch("b"), a: 1.0 }
        }
        _ => return None,
    };
    for m in &n.kids {
        let v = m.num("val").unwrap_or(100000.0) / 100000.0;
        match m.name.as_str() {
            "lumMod" => {
                let (h, s, l) = to_hsl(c);
                c = from_hsl(h, s, l * v, c.a);
            }
            "lumOff" => {
                let (h, s, l) = to_hsl(c);
                c = from_hsl(h, s, (l + v).clamp(0.0, 1.0), c.a);
            }
            "satMod" => {
                let (h, s, l) = to_hsl(c);
                c = from_hsl(h, (s * v).clamp(0.0, 1.0), l, c.a);
            }
            "tint" => {
                let t = |x: u8| (x as f32 * v + 255.0 * (1.0 - v)).round().clamp(0.0, 255.0) as u8;
                c = Rgba { r: t(c.r), g: t(c.g), b: t(c.b), a: c.a };
            }
            "shade" => {
                let t = |x: u8| (x as f32 * v).round().clamp(0.0, 255.0) as u8;
                c = Rgba { r: t(c.r), g: t(c.g), b: t(c.b), a: c.a };
            }
            "alpha" => c.a = v.clamp(0.0, 1.0),
            _ => {}
        }
    }
    Some(c)
}

/// The first colour element inside `n`.
fn colour_in(n: &Node, ctx: &Ctx, ph: Option<Rgba>) -> Option<Rgba> {
    n.kids.iter().find_map(|k| colour(k, ctx, ph))
}

/// A fill: not said, said to be none, or a colour or picture.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fill {
    Unset,
    None,
    Paint(Paint),
}

// ---- Reading the parts ----

struct Part {
    root: Node,
    rels: HashMap<String, String>,
}

impl Part {
    fn open(r: &Reader, path: &str) -> Option<Part> {
        let text = zip::read_text(r.data, r.entries, path)?;
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        let rels = relationships(r.data, r.entries, &format!("{dir}/_rels/{name}.rels"), dir);
        Some(Part { root: Node::parse(&text), rels })
    }

    fn top(&self) -> Option<&Node> {
        self.root.top()
    }

    fn tree(&self) -> Option<&Node> {
        self.top()?.path(&["cSld", "spTree"])
    }

    /// The part a relationship of this sort leads to: "slideLayout", "slideMaster", "theme".
    fn linked(&self, folder: &str) -> Option<&str> {
        self.rels.values().find(|t| t.contains(&format!("/{folder}/"))).map(String::as_str)
    }
}

struct Master {
    part: Part,
    ctx: Ctx,
    title: Levels,
    body: Levels,
    other: Levels,
}

struct Reader<'a> {
    data: &'a [u8],
    entries: &'a [zip::Entry],
    pictures: Vec<Vec<u8>>,
    by_path: HashMap<String, usize>,
    bytes: usize,
}

impl Reader<'_> {
    /// A picture's index in the deck, read the first time it's used.
    fn picture(&mut self, path: &str) -> Option<usize> {
        if let Some(&i) = self.by_path.get(path) {
            return Some(i);
        }
        let lower = path.to_ascii_lowercase();
        // Windows metafiles and SVG drawings: WIC can't draw them (an SVG comes with a PNG).
        if [".emf", ".wmf", ".svg", ".emz", ".wmz"].iter().any(|e| lower.ends_with(e)) || self.pictures.len() >= MAX_PICTURES {
            return None;
        }
        let e = zip::find(self.entries, path)?;
        if self.bytes + e.size as usize > MAX_PICTURE_BYTES {
            return None;
        }
        let bytes = zip::read(self.data, e).ok()?;
        self.bytes += bytes.len();
        self.pictures.push(bytes);
        let i = self.pictures.len() - 1;
        self.by_path.insert(path.to_string(), i);
        Some(i)
    }
}

// ---- Text styles ----

#[derive(Clone, Debug, PartialEq)]
enum BulletSpec {
    None,
    Char(String),
    Auto(String, u32),
}

/// What a level of text looks like, as far as one place says: unset fields come from the
/// place under it.
#[derive(Clone, Default, Debug)]
struct Lvl {
    align: Option<Align>,
    margin: Option<f32>,
    indent: Option<f32>,
    size: Option<f32>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strike: Option<bool>,
    caps: Option<bool>,
    colour: Option<Rgba>,
    font: Option<String>,
    bullet: Option<BulletSpec>,
    bullet_font: Option<String>,
    bullet_colour: Option<Rgba>,
    bullet_size: Option<f32>,
    /// Space before: points, or (negative) a share of the text size.
    before: Option<f32>,
    line: Option<f32>,
}

impl Lvl {
    /// `self`, with what it leaves unset taken from `under`.
    fn over(&self, under: &Lvl) -> Lvl {
        Lvl {
            align: self.align.or(under.align),
            margin: self.margin.or(under.margin),
            indent: self.indent.or(under.indent),
            size: self.size.or(under.size),
            bold: self.bold.or(under.bold),
            italic: self.italic.or(under.italic),
            underline: self.underline.or(under.underline),
            strike: self.strike.or(under.strike),
            caps: self.caps.or(under.caps),
            colour: self.colour.or(under.colour),
            font: self.font.clone().or_else(|| under.font.clone()),
            bullet: self.bullet.clone().or_else(|| under.bullet.clone()),
            bullet_font: self.bullet_font.clone().or_else(|| under.bullet_font.clone()),
            bullet_colour: self.bullet_colour.or(under.bullet_colour),
            bullet_size: self.bullet_size.or(under.bullet_size),
            before: self.before.or(under.before),
            line: self.line.or(under.line),
        }
    }
}

/// The nine levels of a list style.
type Levels = Vec<Lvl>;

fn levels(list: Option<&Node>, ctx: &Ctx) -> Levels {
    let mut out = vec![Lvl::default(); 9];
    let Some(list) = list else { return out };
    let base = list.get("defPPr").map(|p| ppr(p, ctx)).unwrap_or_default();
    for (i, l) in out.iter_mut().enumerate() {
        *l = match list.get(&format!("lvl{}pPr", i + 1)) {
            Some(p) => ppr(p, ctx).over(&base),
            None => base.clone(),
        };
    }
    out
}

/// A paragraph's properties (`pPr`, or a level of a list style).
fn ppr(p: &Node, ctx: &Ctx) -> Lvl {
    let mut l = Lvl {
        align: p.attr("algn").map(|a| match a.as_str() {
            "ctr" => Align::Center,
            "r" => Align::Right,
            "just" | "dist" | "justLow" | "thaiDist" => Align::Justify,
            _ => Align::Left,
        }),
        margin: p.num("marL"),
        indent: p.num("indent"),
        ..Lvl::default()
    };
    for k in &p.kids {
        match k.name.as_str() {
            "defRPr" => rpr(k, &mut l, ctx),
            "buNone" => l.bullet = Some(BulletSpec::None),
            "buChar" => l.bullet = Some(BulletSpec::Char(k.attr("char").unwrap_or_else(|| "•".into()))),
            "buAutoNum" => l.bullet = Some(BulletSpec::Auto(k.attr("type").unwrap_or_else(|| "arabicPeriod".into()), k.num("startAt").unwrap_or(1.0) as u32)),
            "buFont" => l.bullet_font = k.attr("typeface"),
            "buClr" => l.bullet_colour = colour_in(k, ctx, None),
            "buSzPct" => l.bullet_size = k.num("val").map(|v| v / 100000.0),
            "lnSpc" => {
                if let Some(v) = k.get("spcPct").and_then(|s| s.num("val")) {
                    l.line = Some(v / 100000.0);
                }
            }
            "spcBef" => {
                if let Some(v) = k.get("spcPts").and_then(|s| s.num("val")) {
                    l.before = Some(v / 100.0);
                } else if let Some(v) = k.get("spcPct").and_then(|s| s.num("val")) {
                    l.before = Some(-v / 100000.0);
                }
            }
            _ => {}
        }
    }
    l
}

/// A run's properties (`rPr`, `defRPr`, `endParaRPr`), into `l`.
fn rpr(r: &Node, l: &mut Lvl, ctx: &Ctx) {
    if let Some(sz) = r.num("sz") {
        l.size = Some(sz / 100.0);
    }
    if let Some(b) = r.flag("b") {
        l.bold = Some(b);
    }
    if let Some(i) = r.flag("i") {
        l.italic = Some(i);
    }
    if let Some(u) = r.attr("u") {
        l.underline = Some(u != "none");
    }
    if let Some(s) = r.attr("strike") {
        l.strike = Some(s != "noStrike");
    }
    if let Some(c) = r.attr("cap") {
        l.caps = Some(c == "all");
    }
    for k in &r.kids {
        match k.name.as_str() {
            "solidFill" => l.colour = colour_in(k, ctx, None).or(l.colour),
            "latin" => l.font = k.attr("typeface").filter(|f| !f.is_empty()).or(l.font.take()),
            _ => {}
        }
    }
}

/// Level `i` through a stack of styles, the first on top.
fn level(stack: &[&Levels], i: usize) -> Lvl {
    stack.iter().rev().fold(Lvl::default(), |under, l| l.get(i).map_or(under.clone(), |top| top.over(&under)))
}

fn auto_number(scheme: &str, n: u32) -> String {
    let alpha = |upper: bool| {
        let mut s = String::new();
        let mut k = n.max(1);
        while k > 0 {
            let c = ((k - 1) % 26) as u8;
            s.insert(0, (if upper { b'A' } else { b'a' } + c) as char);
            k = (k - 1) / 26;
        }
        s
    };
    let roman = |upper: bool| {
        let table = [(1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"), (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"), (5, "v"), (4, "iv"), (1, "i")];
        let mut k = n.clamp(1, 3999);
        let mut s = String::new();
        for (v, t) in table {
            while k >= v {
                s.push_str(t);
                k -= v;
            }
        }
        if upper { s.to_uppercase() } else { s }
    };
    let (body, form) = if scheme.starts_with("alphaLc") {
        (alpha(false), &scheme[7..])
    } else if scheme.starts_with("alphaUc") {
        (alpha(true), &scheme[7..])
    } else if scheme.starts_with("romanLc") {
        (roman(false), &scheme[7..])
    } else if scheme.starts_with("romanUc") {
        (roman(true), &scheme[7..])
    } else {
        (n.to_string(), scheme.strip_prefix("arabic").unwrap_or("Period"))
    };
    match form {
        "ParenR" => format!("{body})"),
        "ParenBoth" => format!("({body})"),
        "Plain" => body,
        _ => format!("{body}."),
    }
}

/// A bullet character, as it looks in the symbol font it's from.
fn bullet_char(c: &str, font: Option<&str>) -> String {
    let font = font.unwrap_or("").to_ascii_lowercase();
    let first = c.chars().next().unwrap_or('•');
    if font.contains("wingdings") {
        return match first {
            '§' => "▪",
            'Ø' => "➢",
            'ü' => "✓",
            'q' => "❑",
            'v' => "❖",
            'n' => "■",
            'l' => "●",
            'è' | 'ê' | 'à' => "➔",
            _ => "•",
        }
        .to_string();
    }
    if font.contains("symbol") {
        return if first == '·' || first == '\u{F0B7}' { "•".into() } else { first.to_string() };
    }
    match first {
        '\u{F0A7}' => "▪".into(),
        '\u{F0B7}' => "•".into(),
        '\u{F0D8}' => "➢".into(),
        '\u{F0FC}' => "✓".into(),
        _ => c.to_string(),
    }
}

// ---- Text boxes ----

/// A text body (`txBody`), with what it doesn't say from `bodies` (body properties, nearest
/// first) and `stack` (list styles, nearest first).
fn text_box(tx: &Node, bodies: &[&Node], stack: &[&Levels], ctx: &Ctx) -> Option<TextBox> {
    let own_list = levels(tx.get("lstStyle"), ctx);
    let mut full: Vec<&Levels> = vec![&own_list];
    full.extend_from_slice(stack);
    let own_body = tx.get("bodyPr");
    let chain: Vec<&Node> = own_body.into_iter().chain(bodies.iter().copied()).collect();
    let battr = |name: &str| chain.iter().find_map(|b| b.attr(name));
    let bnum = |name: &str, d: f32| battr(name).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    let autofit = chain.iter().find_map(|b| b.kids.iter().find(|k| matches!(k.name.as_str(), "normAutofit" | "spAutoFit" | "noAutofit")));
    let shrink = autofit.is_some_and(|a| a.name == "normAutofit");
    let scale = autofit.filter(|a| a.name == "normAutofit").and_then(|a| a.num("fontScale")).map_or(1.0, |v| v / 100000.0);
    let reduce = autofit.filter(|a| a.name == "normAutofit").and_then(|a| a.num("lnSpcReduction")).map_or(0.0, |v| v / 100000.0);

    let mut paras = Vec::new();
    let mut counters = [0u32; 9];
    for p in tx.all("p") {
        let ppr_node = p.get("pPr");
        let lvl_i = ppr_node.and_then(|n| n.num("lvl")).unwrap_or(0.0).clamp(0.0, 8.0) as usize;
        let base = level(&full, lvl_i);
        let pl = ppr_node.map(|n| ppr(n, ctx)).unwrap_or_default().over(&base);
        let mut runs = Vec::new();
        let run_style = |r: Option<&Node>| {
            let mut l = Lvl::default();
            if let Some(r) = r {
                rpr(r, &mut l, ctx);
            }
            l.over(&pl)
        };
        let make = |text: &str, l: &Lvl| {
            let text = if l.caps == Some(true) { text.to_uppercase() } else { text.to_string() };
            TextRun {
                text,
                size: l.size.unwrap_or(18.0),
                bold: l.bold.unwrap_or(false),
                italic: l.italic.unwrap_or(false),
                underline: l.underline.unwrap_or(false),
                strike: l.strike.unwrap_or(false),
                colour: l.colour.unwrap_or_else(|| ctx.text()),
                font: ctx.font(l.font.as_deref().unwrap_or("+mn-lt")),
            }
        };
        for k in &p.kids {
            match k.name.as_str() {
                "r" | "fld" => {
                    let l = run_style(k.get("rPr"));
                    let text: String = k.all("t").map(|t| t.text.as_str()).collect();
                    if !text.is_empty() {
                        runs.push(make(&text, &l));
                    }
                }
                "br" => {
                    let l = run_style(k.get("rPr"));
                    runs.push(make("\u{2028}", &l));
                }
                _ => {}
            }
        }
        let end = run_style(p.get("endParaRPr"));
        let size = runs.first().map_or(end.size.unwrap_or(18.0), |r| r.size);
        let has_text = runs.iter().any(|r| !r.text.trim().is_empty());
        let bullet = match (&pl.bullet, has_text) {
            (Some(BulletSpec::Char(c)), true) => {
                counters[lvl_i] = 0;
                Some(bullet_char(c, pl.bullet_font.as_deref()))
            }
            (Some(BulletSpec::Auto(scheme, start)), true) => {
                counters[lvl_i] = if counters[lvl_i] == 0 { *start } else { counters[lvl_i] + 1 };
                Some(auto_number(scheme, counters[lvl_i]))
            }
            _ => {
                if has_text {
                    counters[lvl_i] = 0;
                }
                None
            }
        };
        for c in counters.iter_mut().skip(lvl_i + 1) {
            *c = 0;
        }
        let bullet = bullet.map(|text| Bullet {
            text,
            colour: pl.bullet_colour.or_else(|| runs.first().map(|r| r.colour)).unwrap_or_else(|| ctx.text()),
            size: size * pl.bullet_size.unwrap_or(1.0),
        });
        let before = match pl.before {
            Some(b) if b < 0.0 => -b * size,
            Some(b) => b,
            None => 0.0,
        };
        paras.push(Para {
            align: pl.align.unwrap_or(Align::Left),
            margin: pl.margin.unwrap_or(0.0),
            indent: pl.indent.unwrap_or(0.0),
            bullet,
            before,
            line: (pl.line.unwrap_or(1.0) - reduce).max(0.5),
            runs,
            size,
        });
    }
    if paras.iter().all(|p| p.runs.iter().all(|r| r.text.trim().is_empty())) {
        return None;
    }
    let anchor = match battr("anchor").as_deref() {
        Some("ctr") => Anchor::Middle,
        Some("b") => Anchor::Bottom,
        _ => Anchor::Top,
    };
    Some(TextBox {
        inset: [bnum("lIns", 91440.0), bnum("tIns", 45720.0), bnum("rIns", 91440.0), bnum("bIns", 45720.0)],
        anchor,
        wrap: battr("wrap").as_deref() != Some("none"),
        shrink,
        scale,
        paras,
    })
}

// ---- Shapes ----

/// Where a group puts its shapes: x' = x * sx + tx.
#[derive(Clone, Copy)]
struct Xf {
    sx: f32,
    sy: f32,
    tx: f32,
    ty: f32,
}

impl Xf {
    const ONE: Xf = Xf { sx: 1.0, sy: 1.0, tx: 0.0, ty: 0.0 };

    fn apply(&self, r: [f32; 4]) -> [f32; 4] {
        [r[0] * self.sx + self.tx, r[1] * self.sy + self.ty, r[2] * self.sx, r[3] * self.sy]
    }
}

/// A transform's box and flips (`xfrm`).
fn xfrm(x: &Node) -> Option<([f32; 4], (bool, bool))> {
    let off = x.get("off")?;
    let ext = x.get("ext")?;
    Some(([off.num("x")?, off.num("y")?, ext.num("cx")?, ext.num("cy")?], (x.flag("flipH").unwrap_or(false), x.flag("flipV").unwrap_or(false))))
}

/// A shape's placeholder: its type and number.
fn placeholder(shape: &Node) -> Option<(String, Option<u32>)> {
    let nv = shape.kids.iter().find(|k| k.name.starts_with("nv"))?;
    let ph = nv.path(&["nvPr", "ph"])?;
    Some((ph.attr("type").unwrap_or_else(|| "obj".into()), ph.num("idx").map(|v| v as u32)))
}

/// Placeholder types that take their look from the same place.
fn family(t: &str) -> &str {
    match t {
        "title" | "ctrTitle" => "title",
        "body" | "subTitle" | "obj" | "tbl" | "chart" | "dgm" | "media" | "clipArt" | "pic" => "body",
        other => other,
    }
}

/// The shape in `tree` that's the same placeholder: by number if it has one, else by type.
fn find_placeholder<'a>(tree: &'a Node, ty: &str, idx: Option<u32>, by_number: bool) -> Option<&'a Node> {
    let mut found: Vec<(&Node, String, Option<u32>)> = Vec::new();
    collect_placeholders(tree, &mut found);
    if by_number {
        if let Some(i) = idx.filter(|&i| i != 0) {
            if let Some((n, _, _)) = found.iter().find(|(_, _, j)| *j == Some(i)) {
                return Some(n);
            }
        }
    }
    let want = family(ty);
    // The same type first ("ctrTitle" for "ctrTitle"), then the same family.
    found.iter().find(|(_, t, _)| t == ty).or_else(|| found.iter().find(|(_, t, _)| family(t) == want)).map(|(n, _, _)| *n)
}

fn collect_placeholders<'a>(tree: &'a Node, out: &mut Vec<(&'a Node, String, Option<u32>)>) {
    for k in &tree.kids {
        match k.name.as_str() {
            "sp" | "pic" | "graphicFrame" => {
                if let Some((t, i)) = placeholder(k) {
                    out.push((k, t, i));
                }
            }
            "grpSp" => collect_placeholders(k, out),
            _ => {}
        }
    }
}

/// Where a slide's shapes look for what they leave out.
struct Sources<'a> {
    layout: Option<&'a Node>,
    master: Option<&'a Master>,
    defaults: &'a Levels,
    /// Title and body styles for a deck with no master.
    plain: &'a (Levels, Levels),
    /// The slide's size, for placing placeholders nothing places.
    size: (f32, f32),
    /// Drawing a layout or master's own shapes: their placeholders are prompts, not drawn.
    skip_placeholders: bool,
}

fn fill(n: &Node, ctx: &Ctx, part: &Part, r: &mut Reader, ph: Option<Rgba>) -> Fill {
    for k in &n.kids {
        match k.name.as_str() {
            "noFill" => return Fill::None,
            "solidFill" => return colour_in(k, ctx, ph).map_or(Fill::Unset, |c| Fill::Paint(Paint::Colour(c))),
            "gradFill" => {
                // The colour halfway along, near enough.
                let stops: Vec<Rgba> = k.get("gsLst").map(|l| l.all("gs").filter_map(|g| colour_in(g, ctx, ph)).collect()).unwrap_or_default();
                if let (Some(a), Some(b)) = (stops.first(), stops.last()) {
                    let m = |x: u8, y: u8| ((x as u16 + y as u16) / 2) as u8;
                    return Fill::Paint(Paint::Colour(Rgba { r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b), a: (a.a + b.a) / 2.0 }));
                }
            }
            "pattFill" => {
                if let Some(c) = k.get("fgClr").and_then(|f| colour_in(f, ctx, ph)) {
                    return Fill::Paint(Paint::Colour(c));
                }
            }
            "blipFill" => {
                if let Some(i) = blip(k, part, r) {
                    return Fill::Paint(Paint::Picture(i));
                }
            }
            _ => {}
        }
    }
    Fill::Unset
}

/// The picture a `blipFill` shows.
fn blip(bf: &Node, part: &Part, r: &mut Reader) -> Option<usize> {
    let b = bf.get("blip")?;
    let id = attr_ns(&b.attrs, "r:embed")?;
    let path = part.rels.get(&id)?;
    r.picture(path)
}

/// A shape's style colour (`p:style`, for shapes drawn from the Insert menu): fill, line, font.
fn style_ref(shape: &Node, which: &str, ctx: &Ctx) -> Option<Rgba> {
    let s = shape.get("style")?.get(which)?;
    if which != "fontRef" && s.num("idx").unwrap_or(0.0) == 0.0 {
        return None;
    }
    colour_in(s, ctx, None)
}

fn line_of(sppr: Option<&Node>, shape: &Node, ctx: &Ctx) -> Option<(Rgba, f32)> {
    match sppr.and_then(|s| s.get("ln")) {
        Some(ln) => {
            let width = ln.num("w").unwrap_or(9525.0);
            if ln.get("noFill").is_some() {
                return None;
            }
            let c = ln.get("solidFill").and_then(|f| colour_in(f, ctx, None)).or_else(|| ln.get("gradFill").is_none().then(|| style_ref(shape, "lnRef", ctx)).flatten())?;
            Some((c, width))
        }
        None => style_ref(shape, "lnRef", ctx).map(|c| (c, 9525.0)),
    }
}

fn geom(sppr: Option<&Node>) -> Geom {
    let Some(g) = sppr.and_then(|s| s.get("prstGeom")) else { return Geom::Rect };
    match g.attr("prst").as_deref() {
        Some("roundRect") => {
            let adj = g.get("avLst").and_then(|l| l.get("gd")).and_then(|gd| gd.attr("fmla")).and_then(|f| f.strip_prefix("val ").and_then(|v| v.trim().parse::<f32>().ok()));
            Geom::Round(adj.unwrap_or(16667.0) / 100000.0)
        }
        Some("ellipse") => Geom::Ellipse,
        Some(p) if p == "line" || p.contains("Connector") => Geom::Line,
        _ => Geom::Rect,
    }
}

fn hidden(shape: &Node) -> bool {
    shape.kids.iter().find(|k| k.name.starts_with("nv")).and_then(|nv| nv.get("cNvPr")).and_then(|c| c.flag("hidden")).unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn shapes(tree: &Node, xf: Xf, part: &Part, src: &Sources, ctx: &Ctx, r: &mut Reader, out: &mut Vec<Item>) {
    for k in &tree.kids {
        match k.name.as_str() {
            "sp" | "cxnSp" => shape(k, xf, part, src, ctx, r, out),
            "pic" => picture(k, xf, part, src, ctx, r, out),
            "grpSp" => {
                if hidden(k) {
                    continue;
                }
                let inner = k.path(&["grpSpPr", "xfrm"]).and_then(|x| {
                    let ([ox, oy, ex, ey], _) = xfrm(x)?;
                    let (co, ce) = (x.get("chOff")?, x.get("chExt")?);
                    let (cx, cy, cw, ch) = (co.num("x")?, co.num("y")?, ce.num("cx")?, ce.num("cy")?);
                    let (sx, sy) = (if cw > 0.0 { ex / cw } else { 1.0 }, if ch > 0.0 { ey / ch } else { 1.0 });
                    let g = Xf { sx, sy, tx: ox - cx * sx, ty: oy - cy * sy };
                    Some(Xf { sx: g.sx * xf.sx, sy: g.sy * xf.sy, tx: g.tx * xf.sx + xf.tx, ty: g.ty * xf.sy + xf.ty })
                });
                shapes(k, inner.unwrap_or(xf), part, src, ctx, r, out);
            }
            "graphicFrame" => frame(k, xf, part, src, ctx, r, out),
            "AlternateContent" => {
                if let Some(alt) = k.get("Fallback").or_else(|| k.get("Choice")) {
                    shapes(alt, xf, part, src, ctx, r, out);
                }
            }
            _ => {}
        }
    }
}

/// A placeholder's matching shapes on the layout and master, nearest first.
fn inherited<'a>(k: &Node, src: &Sources<'a>) -> (Option<(String, Option<u32>)>, Vec<&'a Node>) {
    let ph = placeholder(k);
    let mut from = Vec::new();
    if let Some((t, i)) = &ph {
        if let Some(n) = src.layout.and_then(|l| find_placeholder(l, t, *i, true)) {
            from.push(n);
        }
        if let Some(n) = src.master.and_then(|m| m.part.tree()).and_then(|tree| find_placeholder(tree, t, *i, false)) {
            from.push(n);
        }
    }
    (ph, from)
}

/// Where PowerPoint's own layouts put a placeholder, for a deck that doesn't say.
fn default_place(ty: &str, (w, h): (f32, f32)) -> Option<[f32; 4]> {
    Some(match ty {
        "ctrTitle" => [w * 0.125, h * 0.25, w * 0.75, h * 0.25],
        "subTitle" => [w * 0.125, h * 0.52, w * 0.75, h * 0.2],
        "title" => [w * 0.07, h * 0.05, w * 0.86, h * 0.18],
        "dt" | "ftr" | "sldNum" | "hdr" => return None,
        _ => [w * 0.07, h * 0.26, w * 0.86, h * 0.66],
    })
}

/// Title and body styles like PowerPoint's own, for a deck without a master.
fn plain_styles() -> (Levels, Levels) {
    let title = Lvl { size: Some(44.0), font: Some("+mj-lt".into()), bullet: Some(BulletSpec::None), line: Some(0.9), ..Lvl::default() };
    let body: Levels = (0..9)
        .map(|i| Lvl {
            size: Some([28.0, 24.0, 20.0, 18.0, 18.0, 18.0, 18.0, 18.0, 18.0][i]),
            margin: Some(228600.0 * (2 * i + 1) as f32),
            indent: Some(-228600.0),
            bullet: Some(BulletSpec::Char(if i % 2 == 0 { "•" } else { "–" }.into())),
            before: Some(10.0),
            line: Some(0.9),
            ..Lvl::default()
        })
        .collect();
    (vec![title; 9], body)
}

/// The master's list style for a placeholder of this type.
fn master_style<'a>(ty: Option<&str>, src: &Sources<'a>) -> Option<&'a Levels> {
    let Some(m) = src.master else {
        return match ty.map(family) {
            Some("title") => Some(&src.plain.0),
            Some("body") => Some(&src.plain.1),
            _ => None,
        };
    };
    Some(match ty.map(family) {
        Some("title") => &m.title,
        Some("body") => &m.body,
        Some(_) => &m.other,
        None => return None,
    })
}

fn shape(k: &Node, xf: Xf, part: &Part, src: &Sources, ctx: &Ctx, r: &mut Reader, out: &mut Vec<Item>) {
    if hidden(k) {
        return;
    }
    let (ph, from) = inherited(k, src);
    if ph.is_some() && src.skip_placeholders {
        return;
    }
    let sppr = k.get("spPr");
    let place = sppr.and_then(|s| s.get("xfrm")).and_then(xfrm).or_else(|| from.iter().find_map(|n| n.get("spPr").and_then(|s| s.get("xfrm")).and_then(xfrm)));
    let place = place.or_else(|| ph.as_ref().and_then(|(t, _)| default_place(t, src.size)).map(|r| (r, (false, false))));
    let Some((rect, flip)) = place else { return };
    let rect = xf.apply(rect);
    let g = geom(sppr);
    // The fill: its own, a placeholder's, or its style's.
    let mut f = sppr.map_or(Fill::Unset, |s| fill(s, ctx, part, r, None));
    if f == Fill::Unset {
        for n in &from {
            if let Some(s) = n.get("spPr") {
                f = fill(s, ctx, part, r, None);
                if f != Fill::Unset {
                    break;
                }
            }
        }
    }
    let paint = match f {
        Fill::Paint(p) => Some(p),
        Fill::None => None,
        Fill::Unset => style_ref(k, "fillRef", ctx).map(Paint::Colour),
    };
    let line = line_of(sppr, k, ctx);
    if g == Geom::Line {
        if let Some(l) = line {
            out.push(Item { rect, flip, kind: ItemKind::Shape { geom: g, fill: None, line: Some(l) } });
        }
        return;
    }
    if paint.is_some() || line.is_some() {
        out.push(Item { rect, flip, kind: ItemKind::Shape { geom: g, fill: paint, line } });
    }
    let Some(tx) = k.get("txBody") else { return };
    let own_ty = ph.as_ref().map(|(t, _)| t.as_str());
    let inherited_lists: Vec<Levels> = from.iter().map(|n| levels(n.path(&["txBody", "lstStyle"]), ctx)).collect();
    let font_ref = style_ref(k, "fontRef", ctx).map(|c| {
        let l = Lvl { colour: Some(c), ..Lvl::default() };
        vec![l; 9]
    });
    let mut stack: Vec<&Levels> = inherited_lists.iter().collect();
    if let Some(f) = &font_ref {
        stack.push(f);
    }
    if let Some(m) = master_style(own_ty, src) {
        stack.push(m);
    }
    stack.push(src.defaults);
    let bodies: Vec<&Node> = from.iter().filter_map(|n| n.path(&["txBody", "bodyPr"])).collect();
    if let Some(tb) = text_box(tx, &bodies, &stack, ctx) {
        out.push(Item { rect, flip, kind: ItemKind::Text(tb) });
    }
}

fn picture(k: &Node, xf: Xf, part: &Part, src: &Sources, ctx: &Ctx, r: &mut Reader, out: &mut Vec<Item>) {
    if hidden(k) {
        return;
    }
    let (ph, from) = inherited(k, src);
    if ph.is_some() && src.skip_placeholders {
        return;
    }
    let place = k.path(&["spPr", "xfrm"]).and_then(xfrm).or_else(|| from.iter().find_map(|n| n.path(&["spPr", "xfrm"]).and_then(xfrm)));
    let Some((rect, flip)) = place else { return };
    let rect = xf.apply(rect);
    let Some(bf) = k.get("blipFill") else { return };
    let crop = bf.get("srcRect").map_or([0.0; 4], |s| ["l", "t", "r", "b"].map(|a| (s.num(a).unwrap_or(0.0) / 100000.0).clamp(-1.0, 1.0)));
    match blip(bf, part, r) {
        Some(index) => out.push(Item { rect, flip, kind: ItemKind::Picture { index, crop } }),
        None => {
            if let Some(l) = line_of(k.get("spPr"), k, ctx) {
                out.push(Item { rect, flip, kind: ItemKind::Shape { geom: Geom::Rect, fill: None, line: Some(l) } });
            }
        }
    }
}

/// A table, chart or diagram.
fn frame(k: &Node, xf: Xf, part: &Part, src: &Sources, ctx: &Ctx, r: &mut Reader, out: &mut Vec<Item>) {
    if hidden(k) {
        return;
    }
    let (ph, from) = inherited(k, src);
    if ph.is_some() && src.skip_placeholders {
        return;
    }
    let place = k.get("xfrm").and_then(xfrm).or_else(|| from.iter().find_map(|n| n.get("xfrm").and_then(xfrm)));
    let Some((rect, _)) = place else { return };
    let rect = xf.apply(rect);
    let Some(data) = k.path(&["graphic", "graphicData"]) else { return };
    if let Some(tbl) = data.get("tbl") {
        table(tbl, rect, xf, src, ctx, part, r, out);
    } else {
        let uri = data.attr("uri").unwrap_or_default();
        let what = if uri.contains("chart") {
            "Chart"
        } else if uri.contains("diagram") {
            "Diagram"
        } else if uri.contains("ole") {
            "Embedded object"
        } else {
            "Graphic"
        };
        // An embedded object often carries a picture of itself.
        if let Some(i) = data.kids.iter().find_map(|n| find_blip(n)).and_then(|b| part.rels.get(&b)).and_then(|p| r.picture(p)) {
            out.push(Item { rect, flip: (false, false), kind: ItemKind::Picture { index: i, crop: [0.0; 4] } });
            return;
        }
        out.push(Item { rect, flip: (false, false), kind: ItemKind::Missing(what) });
    }
}

fn find_blip(n: &Node) -> Option<String> {
    if n.name == "blip" {
        return attr_ns(&n.attrs, "r:embed");
    }
    n.kids.iter().find_map(find_blip)
}

#[allow(clippy::too_many_arguments)]
fn table(tbl: &Node, rect: [f32; 4], xf: Xf, src: &Sources, ctx: &Ctx, part: &Part, r: &mut Reader, out: &mut Vec<Item>) {
    let cols: Vec<f32> = tbl.get("tblGrid").map(|g| g.all("gridCol").map(|c| c.num("w").unwrap_or(0.0) * xf.sx).collect()).unwrap_or_default();
    if cols.is_empty() {
        return;
    }
    let pr = tbl.get("tblPr");
    let first_row = pr.and_then(|p| p.flag("firstRow")).unwrap_or(false);
    let banded = pr.and_then(|p| p.flag("bandRow")).unwrap_or(false);
    let styled = pr.is_some_and(|p| p.get("tableStyleId").is_some()) || first_row || banded;
    let accent = ctx.scheme("accent1").unwrap_or(Rgba::BLACK);
    let light = ctx.scheme("lt1").unwrap_or(Rgba::WHITE);
    let tint = |v: f32| {
        let t = |x: u8| (x as f32 * v + 255.0 * (1.0 - v)).round() as u8;
        Rgba { r: t(accent.r), g: t(accent.g), b: t(accent.b), a: 1.0 }
    };
    let rows: Vec<&Node> = tbl.all("tr").collect();
    let heights: Vec<f32> = rows.iter().map(|tr| tr.num("h").unwrap_or(370840.0) * xf.sy).collect();
    let header = Lvl { colour: Some(light), bold: Some(true), ..Lvl::default() };
    let header_levels = vec![header; 9];
    let mut y = rect[1];
    for (ri, tr) in rows.iter().enumerate() {
        let mut x = rect[0];
        for (ci, tc) in tr.all("tc").enumerate() {
            let w = cols.get(ci).copied().unwrap_or(0.0);
            if tc.flag("hMerge") == Some(true) || tc.flag("vMerge") == Some(true) {
                x += w;
                continue;
            }
            let span = tc.num("gridSpan").unwrap_or(1.0).max(1.0) as usize;
            let down = tc.num("rowSpan").unwrap_or(1.0).max(1.0) as usize;
            let cw: f32 = cols.iter().skip(ci).take(span).sum();
            let ch: f32 = heights.iter().skip(ri).take(down).sum();
            let cell = [x, y, cw, ch];
            let tcpr = tc.get("tcPr");
            let own = tcpr.map_or(Fill::Unset, |p| fill(p, ctx, part, r, None));
            let is_head = styled && first_row && ri == 0;
            let paint = match own {
                Fill::Paint(p) => Some(p),
                Fill::None => None,
                Fill::Unset if is_head => Some(Paint::Colour(accent)),
                Fill::Unset if styled => Some(Paint::Colour(tint(if banded && (ri + first_row as usize) % 2 == 1 { 0.2 } else { 0.4 }))),
                Fill::Unset => None,
            };
            let border = if styled { Some((light, 12700.0)) } else { Some((ctx.text(), 9525.0)) };
            out.push(Item { rect: cell, flip: (false, false), kind: ItemKind::Shape { geom: Geom::Rect, fill: paint, line: border } });
            if let Some(tx) = tc.get("txBody") {
                let mut stack: Vec<&Levels> = Vec::new();
                if is_head {
                    stack.push(&header_levels);
                }
                stack.push(src.defaults);
                let mut tb = text_box(tx, &[], &stack, ctx);
                if let Some(tb) = &mut tb {
                    let m = |a: &str, d: f32| tcpr.and_then(|p| p.num(a)).unwrap_or(d);
                    tb.inset = [m("marL", 91440.0), m("marT", 45720.0), m("marR", 91440.0), m("marB", 45720.0)];
                    tb.anchor = match tcpr.and_then(|p| p.attr("anchor")).as_deref() {
                        Some("ctr") => Anchor::Middle,
                        Some("b") => Anchor::Bottom,
                        _ => Anchor::Top,
                    };
                }
                if let Some(tb) = tb {
                    out.push(Item { rect: cell, flip: (false, false), kind: ItemKind::Text(tb) });
                }
            }
            x += w;
        }
        y += heights[ri];
    }
}

/// A background (`p:bg`), if the part has one.
fn background(top: &Node, ctx: &Ctx, part: &Part, r: &mut Reader) -> Option<Paint> {
    let bg = top.path(&["cSld", "bg"])?;
    if let Some(pr) = bg.get("bgPr") {
        return match fill(pr, ctx, part, r, None) {
            Fill::Paint(p) => Some(p),
            _ => None,
        };
    }
    let rf = bg.get("bgRef")?;
    colour_in(rf, ctx, None).map(Paint::Colour)
}

// ---- The deck ----

/// A PowerPoint file's slides, ready to draw.
pub fn read(data: &[u8]) -> Result<Deck, String> {
    let entries = zip::entries(data).ok_or("This presentation is damaged, or isn't really one.")?;
    if entries.iter().any(|e| e.name.eq_ignore_ascii_case("EncryptedPackage")) {
        return Err("This presentation is locked with a password.".into());
    }
    let mut r = Reader { data, entries: &entries, pictures: Vec::new(), by_path: HashMap::new(), bytes: 0 };
    let deck = Part::open(&r, "ppt/presentation.xml").ok_or("This presentation is damaged.")?;
    let top = deck.top().ok_or("This presentation is damaged.")?;
    let (width, height) = top.get("sldSz").map_or((12192000.0, 6858000.0), |s| (s.num("cx").unwrap_or(12192000.0), s.num("cy").unwrap_or(6858000.0)));
    let mut order: Vec<String> = top.get("sldIdLst").map(|l| l.all("sldId").filter_map(|s| attr_ns(&s.attrs, "r:id")).filter_map(|id| deck.rels.get(&id).cloned()).collect()).unwrap_or_default();
    if order.is_empty() {
        let mut found: Vec<(u32, String)> = entries
            .iter()
            .filter_map(|e| {
                let n = e.name.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?.parse().ok()?;
                Some((n, e.name.clone()))
            })
            .collect();
        found.sort();
        order = found.into_iter().map(|(_, n)| n).collect();
    }
    order.truncate(MAX_SLIDES);

    let plain = plain_styles();
    let mut masters: HashMap<String, Master> = HashMap::new();
    let mut layouts: HashMap<String, (Part, String)> = HashMap::new();
    let mut slides = Vec::new();
    for path in &order {
        let Some(slide) = Part::open(&r, path) else { continue };
        let Some(stop) = slide.top() else { continue };
        // Its layout, and the layout's master, read once each.
        let layout_path = slide.linked("slideLayouts").map(str::to_string);
        if let Some(lp) = &layout_path {
            if !layouts.contains_key(lp) {
                if let Some(part) = Part::open(&r, lp) {
                    let mp = part.linked("slideMasters").unwrap_or_default().to_string();
                    layouts.insert(lp.clone(), (part, mp));
                }
            }
        }
        let master_path = layout_path.as_ref().and_then(|lp| layouts.get(lp)).map(|(_, mp)| mp.clone()).unwrap_or_default();
        if !master_path.is_empty() && !masters.contains_key(&master_path) {
            if let Some(part) = Part::open(&r, &master_path) {
                let theme_part = part.linked("theme").and_then(|t| Part::open(&r, t));
                let theme = Theme::read(theme_part.as_ref().map(|p| &p.root));
                let map = part.top().and_then(|t| t.get("clrMap")).map(|m| {
                    xml_attrs(&m.attrs)
                })
                .unwrap_or_default();
                let ctx = Ctx { theme, map };
                let styles = part.top().and_then(|t| t.get("txStyles"));
                let title = levels(styles.and_then(|s| s.get("titleStyle")), &ctx);
                let body = levels(styles.and_then(|s| s.get("bodyStyle")), &ctx);
                let other = levels(styles.and_then(|s| s.get("otherStyle")), &ctx);
                masters.insert(master_path.clone(), Master { part, ctx, title, body, other });
            }
        }
        let master = masters.get(&master_path);
        let layout = layout_path.as_ref().and_then(|lp| layouts.get(lp)).map(|(p, _)| p);
        let fallback;
        let ctx = match master {
            Some(m) => &m.ctx,
            None => {
                fallback = Ctx { theme: Theme::read(None), map: HashMap::new() };
                &fallback
            }
        };
        let defaults = levels(top.get("defaultTextStyle"), ctx);

        let background = background(stop, ctx, &slide, &mut r)
            .or_else(|| layout.and_then(|l| l.top().and_then(|t| background(t, ctx, l, &mut r))))
            .or_else(|| master.and_then(|m| m.part.top().and_then(|t| background(t, ctx, &m.part, &mut r))))
            .unwrap_or(Paint::Colour(ctx.scheme("bg1").unwrap_or(Rgba::WHITE)));

        let mut items = Vec::new();
        let shows = |t: Option<&Node>| t.and_then(|t| t.flag("showMasterSp")).unwrap_or(true);
        let layout_tree = layout.and_then(|l| l.tree());
        if shows(Some(stop)) {
            if let Some(m) = master {
                if shows(layout.and_then(|l| l.top())) {
                    if let Some(tree) = m.part.tree() {
                        let src = Sources { layout: None, master: Some(m), defaults: &defaults, plain: &plain, size: (width, height), skip_placeholders: true };
                        shapes(tree, Xf::ONE, &m.part, &src, ctx, &mut r, &mut items);
                    }
                }
            }
            if let (Some(l), Some(tree)) = (layout, layout_tree) {
                let src = Sources { layout: None, master, defaults: &defaults, plain: &plain, size: (width, height), skip_placeholders: true };
                shapes(tree, Xf::ONE, l, &src, ctx, &mut r, &mut items);
            }
        }
        if let Some(tree) = slide.tree() {
            let src = Sources { layout: layout_tree, master, defaults: &defaults, plain: &plain, size: (width, height), skip_placeholders: false };
            shapes(tree, Xf::ONE, &slide, &src, ctx, &mut r, &mut items);
        }
        slides.push(Slide { background, items });
    }
    if slides.is_empty() {
        return Err("This presentation has no slides.".into());
    }
    Ok(Deck { width, height, slides, pictures: r.pictures })
}

/// Every attribute of an element, by name without prefix.
fn xml_attrs(attrs: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = attrs.trim();
    while let Some(eq) = rest.find('=') {
        let key = xml::local(rest[..eq].trim()).to_string();
        let after = rest[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
        let Some(end) = after[1..].find(q) else { break };
        out.insert(key, after[1..1 + end].to_string());
        rest = after[2 + end..].trim_start();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::tests::zip;

    const P: &str = r#"xmlns:p="p" xmlns:a="a" xmlns:r="r""#;

    fn deck() -> Vec<u8> {
        let rels = |items: &[(&str, &str)]| {
            let body: String = items.iter().map(|(id, t)| format!(r#"<Relationship Id="{id}" Target="{t}"/>"#)).collect();
            format!("<Relationships>{body}</Relationships>")
        };
        let presentation = format!(r#"<p:presentation {P}><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="12192000" cy="6858000"/><p:defaultTextStyle><a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr></p:defaultTextStyle></p:presentation>"#);
        let theme = r#"<a:theme><a:themeElements><a:clrScheme><a:dk1><a:srgbClr val="111111"/></a:dk1><a:lt1><a:srgbClr val="FEFEFE"/></a:lt1><a:accent1><a:srgbClr val="FF0000"/></a:accent1></a:clrScheme><a:fontScheme><a:majorFont><a:latin typeface="Georgia"/></a:majorFont><a:minorFont><a:latin typeface="Verdana"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#;
        let master = format!(
            r#"<p:sldMaster {P}><p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg><p:spTree>
            <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="100" y="200"/><a:ext cx="3000" cy="400"/></a:xfrm></p:spPr></p:sp>
            <p:sp><p:nvSpPr><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="500" cy="50"/></a:xfrm><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></p:spPr></p:sp>
            </p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1"/>
            <p:txStyles><p:titleStyle><a:lvl1pPr algn="ctr"><a:defRPr sz="4400"><a:solidFill><a:schemeClr val="tx1"/></a:solidFill><a:latin typeface="+mj-lt"/></a:defRPr></a:lvl1pPr></p:titleStyle>
            <p:bodyStyle><a:lvl1pPr marL="228600" indent="-228600"><a:buChar char="•"/><a:defRPr sz="2800"/></a:lvl1pPr><a:lvl2pPr><a:buAutoNum type="arabicPeriod"/><a:defRPr sz="2400"/></a:lvl2pPr></p:bodyStyle></p:txStyles></p:sldMaster>"#
        );
        let layout = format!(r#"<p:sldLayout {P}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="100" y="700"/><a:ext cx="3000" cy="2000"/></a:xfrm></p:spPr></p:sp></p:spTree></p:cSld></p:sldLayout>"#);
        let slide = format!(
            r#"<p:sld {P}><p:cSld><p:spTree>
            <p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Hello</a:t></a:r></a:p></p:txBody></p:sp>
            <p:sp><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr><a:normAutofit fontScale="50000"/></a:bodyPr><a:p><a:r><a:rPr b="1"/><a:t>One</a:t></a:r></a:p><a:p><a:pPr lvl="1"/><a:r><a:t>Two</a:t></a:r></a:p><a:p><a:pPr lvl="1"/><a:r><a:t>Three</a:t></a:r></a:p></p:txBody></p:sp>
            <p:pic><p:nvPicPr><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId3"/><a:srcRect l="10000"/></p:blipFill><p:spPr><a:xfrm><a:off x="5" y="6"/><a:ext cx="7" cy="8"/></a:xfrm></p:spPr></p:pic>
            <p:graphicFrame><p:nvGraphicFramePr><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="0" y="3000"/><a:ext cx="2000" cy="600"/></p:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table"><a:tbl><a:tblPr firstRow="1"/><a:tblGrid><a:gridCol w="1000"/><a:gridCol w="1000"/></a:tblGrid><a:tr h="300"><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>A</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:bodyPr/><a:p><a:r><a:t>B</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>
            </p:spTree></p:cSld></p:sld>"#
        );
        let parts: Vec<(&str, String)> = vec![
            ("ppt/presentation.xml", presentation),
            ("ppt/_rels/presentation.xml.rels", rels(&[("rId2", "slides/slide1.xml")])),
            ("ppt/slides/slide1.xml", slide),
            ("ppt/slides/_rels/slide1.xml.rels", rels(&[("rId1", "../slideLayouts/slideLayout1.xml"), ("rId3", "../media/image1.png")])),
            ("ppt/slideLayouts/slideLayout1.xml", layout),
            ("ppt/slideLayouts/_rels/slideLayout1.xml.rels", rels(&[("rId1", "../slideMasters/slideMaster1.xml")])),
            ("ppt/slideMasters/slideMaster1.xml", master),
            ("ppt/slideMasters/_rels/slideMaster1.xml.rels", rels(&[("rId1", "../theme/theme1.xml")])),
            ("ppt/theme/theme1.xml", theme.to_string()),
            ("ppt/media/image1.png", "not really a png".to_string()),
        ];
        let files: Vec<(&str, &[u8])> = parts.iter().map(|(n, b)| (*n, b.as_bytes())).collect();
        zip(&files)
    }

    #[test]
    fn slides_take_what_they_leave_out_from_layout_master_and_theme() {
        let d = read(&deck()).unwrap();
        assert_eq!((d.width, d.height), (12192000.0, 6858000.0));
        assert_eq!(d.slides.len(), 1);
        let s = &d.slides[0];
        assert_eq!(s.background, Paint::Colour(hex("FEFEFE").unwrap()));
        // The master's red bar comes first, under the slide's own shapes.
        let ItemKind::Shape { fill, .. } = &s.items[0].kind else { panic!() };
        assert_eq!(*fill, Some(Paint::Colour(hex("FF0000").unwrap())));
        // The title: placed by the master, in its heading font, centred and dark.
        let title = s.items.iter().find_map(|i| match &i.kind {
            ItemKind::Text(t) if t.paras[0].text() == "Hello" => Some((i.rect, t)),
            _ => None,
        });
        let (rect, t) = title.unwrap();
        assert_eq!(rect, [100.0, 200.0, 3000.0, 400.0]);
        assert_eq!(t.paras[0].align, Align::Center);
        assert_eq!(t.paras[0].runs[0].size, 44.0);
        assert_eq!(t.paras[0].runs[0].font, "Georgia");
        assert_eq!(t.paras[0].runs[0].colour, hex("111111").unwrap());
        // The body: placed by the layout, bulleted and numbered by the master, shrunk to fit.
        let (rect, body) = s.items.iter().find_map(|i| match &i.kind {
            ItemKind::Text(t) if t.paras[0].text() == "One" => Some((i.rect, t)),
            _ => None,
        }).unwrap();
        assert_eq!(rect, [100.0, 700.0, 3000.0, 2000.0]);
        assert!(body.shrink);
        assert_eq!(body.scale, 0.5);
        assert_eq!(body.paras[0].bullet.as_ref().unwrap().text, "•");
        assert!(body.paras[0].runs[0].bold);
        assert_eq!(body.paras[0].runs[0].font, "Verdana");
        assert_eq!(body.paras[0].margin, 228600.0);
        assert_eq!(body.paras[1].bullet.as_ref().unwrap().text, "1.");
        assert_eq!(body.paras[2].bullet.as_ref().unwrap().text, "2.");
        assert_eq!(body.paras[2].runs[0].size, 24.0);
        // The picture, cropped.
        assert!(s.items.iter().any(|i| matches!(i.kind, ItemKind::Picture { index: 0, crop } if crop[0] == 0.1) && i.rect == [5.0, 6.0, 7.0, 8.0]));
        assert_eq!(d.pictures.len(), 1);
        // The table: a red heading row with light, bold text.
        let cells: Vec<&Item> = s.items.iter().filter(|i| i.rect[1] == 3000.0).collect();
        assert!(cells.iter().any(|i| matches!(&i.kind, ItemKind::Shape { fill: Some(Paint::Colour(c)), .. } if *c == hex("FF0000").unwrap())));
        let b = cells.iter().find_map(|i| match &i.kind {
            ItemKind::Text(t) if t.paras[0].text() == "B" => Some(t),
            _ => None,
        }).unwrap();
        assert!(b.paras[0].runs[0].bold);
        assert_eq!(b.paras[0].runs[0].colour, hex("FEFEFE").unwrap());
    }

    #[test]
    fn colours_numbers_and_symbols() {
        let ctx = Ctx { theme: Theme::read(None), map: HashMap::new() };
        let n = Node::parse(r#"<a:schemeClr val="accent1"><a:lumMod val="50000"/></a:schemeClr>"#);
        let c = colour(n.top().unwrap(), &ctx, None).unwrap();
        assert!(c.r < 0x44 && c.b < 0xC4);
        let n = Node::parse(r#"<a:srgbClr val="000000"><a:alpha val="25000"/></a:srgbClr>"#);
        assert_eq!(colour(n.top().unwrap(), &ctx, None).unwrap().a, 0.25);
        assert_eq!(auto_number("alphaUcParenR", 28), "AB)");
        assert_eq!(auto_number("romanLcPeriod", 4), "iv.");
        assert_eq!(auto_number("arabicParenBoth", 3), "(3)");
        assert_eq!(bullet_char("§", Some("Wingdings")), "▪");
        assert_eq!(xml_attrs(r#" bg1="lt1" tx1='dk1'"#).get("tx1").unwrap(), "dk1");
    }
}
