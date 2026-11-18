//! Writes a folder of test images: `cargo run --release -p view-core --example fixtures -- <dir>`.
//!
//! Covers what View has to get right: EXIF orientation (all eight values, which must all look
//! the same once shown), transparency, 16-bit and indexed PNGs, colour profiles, animation
//! (GIF, APNG, WebP), very large and very small images, damaged files, Unicode names, and a
//! folder of hundreds.

use image::codecs::gif::{GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::{Delay, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use view_core::orient::Orient;

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("usage: fixtures <dir>"));
    fs::create_dir_all(&dir).unwrap();
    let only_big = std::env::args().any(|a| a == "--huge");
    if only_big {
        huge_png(&dir.join("huge 24000x16000.png"), 24000, 16000);
        return;
    }
    orientation(&dir);
    transparency(&dir);
    colour(&dir);
    animation(&dir);
    sizes(&dir);
    damaged(&dir);
    names(&dir);
    many(&dir.join("many"), 300);
    println!("wrote {}", dir.display());
}

/// An upright test card: red bar along the top, blue bar down the left, a green square in the
/// top-left corner. Shown correctly, every orientation fixture looks exactly like this.
fn card(w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        if x < w / 4 && y < h / 4 && x > w / 16 && y > h / 16 {
            Rgb([40, 200, 60])
        } else if y < h / 12 {
            Rgb([220, 40, 40])
        } else if x < w / 12 {
            Rgb([40, 80, 220])
        } else {
            let v = 200 - ((x + y) % 64) as u8;
            Rgb([v, v, v])
        }
    })
}

fn jpeg(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8).unwrap();
    out
}

/// A little-endian TIFF block with just an Orientation tag, as an APP1 Exif segment.
fn exif_app1(orientation: u16) -> Vec<u8> {
    let mut tiff = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&(orientation as u32).to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    let mut seg = vec![0xFF, 0xE1];
    let body_len = 2 + 6 + tiff.len();
    seg.extend_from_slice(&(body_len as u16).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend(tiff);
    seg
}

/// Put a segment right after the JPEG's SOI marker.
fn with_segment(jpeg: &[u8], seg: &[u8]) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(seg);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn orientation(dir: &Path) {
    let d = dir.join("orientation");
    fs::create_dir_all(&d).unwrap();
    let upright = card(240, 360);
    for v in 1..=8u32 {
        let o = Orient::from_exif(v);
        // Stored so that turning it by `o` gives the upright card.
        let (sw, sh) = if o.swaps_axes() { (360, 240) } else { (240, 360) };
        let stored = RgbImage::from_fn(sw, sh, |x, y| {
            let (ux, uy) = o.map(x, y, sw, sh);
            *upright.get_pixel(ux, uy)
        });
        let data = with_segment(&jpeg(&stored, 92), &exif_app1(v as u16));
        fs::write(d.join(format!("exif-{v}.jpg")), data).unwrap();
    }
}

fn transparency(dir: &Path) {
    let d = dir.join("transparency");
    fs::create_dir_all(&d).unwrap();
    // A soft disc fading out, over nothing.
    let disc = RgbaImage::from_fn(512, 512, |x, y| {
        let (dx, dy) = (x as f32 - 256.0, y as f32 - 256.0);
        let r = (dx * dx + dy * dy).sqrt();
        let a = (1.0 - (r - 160.0) / 80.0).clamp(0.0, 1.0);
        Rgba([255, (x / 2) as u8, 40, (a * 255.0) as u8])
    });
    disc.save(d.join("alpha disc.png")).unwrap();
    // 16 bits a channel with alpha.
    let deep = image::ImageBuffer::<Rgba<u16>, Vec<u16>>::from_fn(300, 200, |x, y| Rgba([(x * 218) as u16, (y * 327) as u16, 30000, if x < 150 { 65535 } else { 20000 }]));
    deep.save(d.join("16-bit rgba.png")).unwrap();
    // Grey, and grey with alpha.
    image::GrayImage::from_fn(300, 200, |x, _| image::Luma([(x * 255 / 299) as u8])).save(d.join("grey.png")).unwrap();
    image::GrayAlphaImage::from_fn(300, 200, |x, y| image::LumaA([128, ((x + y) % 256) as u8])).save(d.join("grey alpha.png")).unwrap();
    // An icon with several sizes.
    let mut frames = Vec::new();
    let mut bufs = Vec::new();
    for s in [16u32, 32, 48, 256] {
        let img = RgbaImage::from_fn(s, s, |x, y| if (x + y) % 8 < 4 { Rgba([20, 160, 80, 255]) } else { Rgba([0, 0, 0, 0]) });
        bufs.push((s, img.into_raw()));
    }
    for (s, raw) in &bufs {
        frames.push(image::codecs::ico::IcoFrame::as_png(raw, *s, *s, image::ExtendedColorType::Rgba8).unwrap());
    }
    let f = fs::File::create(d.join("icon.ico")).unwrap();
    image::codecs::ico::IcoEncoder::new(f).encode_images(&frames).unwrap();
    disc.save(d.join("alpha.bmp")).unwrap();
    disc.save(d.join("alpha.tiff")).unwrap();
    // Lossless WebP with alpha.
    let mut webp = Vec::new();
    image_webp::WebPEncoder::new(&mut webp).encode(disc.as_raw(), 512, 512, image_webp::ColorType::Rgba8).unwrap();
    fs::write(d.join("alpha.webp"), webp).unwrap();
}

/// Windows' own sRGB profile with its red and green primaries swapped. Pure red stored under it
/// shows as green once colour-managed, so it's plain to see whether the profile was applied.
/// (A profile written from scratch is easy to get subtly wrong, and Windows refuses those.)
fn swapped_profile() -> Vec<u8> {
    let mut p = fs::read("C:/Windows/System32/spool/drivers/color/sRGB Color Space Profile.icm").expect("Windows' sRGB profile");
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let count = be(&p[128..]);
    let find = |sig: &[u8; 4]| (0..count).map(|i| 132 + 12 * i).find(|&e| &p[e..e + 4] == sig).expect("tag");
    let (r, g, d) = (find(b"rXYZ"), find(b"gXYZ"), find(b"desc"));
    // Swap where the two tags point.
    let r_entry: Vec<u8> = p[r + 4..r + 12].to_vec();
    let g_entry: Vec<u8> = p[g + 4..g + 12].to_vec();
    p[r + 4..r + 12].copy_from_slice(&g_entry);
    p[g + 4..g + 12].copy_from_slice(&r_entry);
    // Rename it in place, so nothing takes it for plain sRGB.
    let off = be(&p[d + 4..]);
    let len = be(&p[off + 8..]);
    let name = b"Swapped test profile";
    for i in 0..len.saturating_sub(1) {
        p[off + 12 + i] = *name.get(i).unwrap_or(&b' ');
    }
    p[84..100].fill(0); // the profile ID is a checksum, now wrong
    p
}

fn colour(dir: &Path) {
    let d = dir.join("colour");
    fs::create_dir_all(&d).unwrap();
    let icc = swapped_profile();
    let red = RgbImage::from_pixel(300, 200, Rgb([255, 0, 0]));
    // PNG with an iCCP chunk.
    let f = fs::File::create(d.join("red stored, shows green.png")).unwrap();
    let mut enc = image::codecs::png::PngEncoder::new(f);
    enc.set_icc_profile(icc.clone()).unwrap();
    enc.write_image(red.as_raw(), 300, 200, image::ExtendedColorType::Rgb8).unwrap();
    // JPEG with an APP2 ICC segment.
    let mut out = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut out, 95);
    enc.set_icc_profile(icc.clone()).unwrap();
    enc.write_image(red.as_raw(), 300, 200, image::ExtendedColorType::Rgb8).unwrap();
    fs::write(d.join("red stored, shows green.jpg"), out).unwrap();
    // The same, untagged: stays red.
    red.save(d.join("red, no profile.png")).unwrap();
}

fn animation(dir: &Path) {
    let d = dir.join("animation");
    fs::create_dir_all(&d).unwrap();
    let frame = |i: u32, n: u32, w: u32, h: u32| {
        RgbaImage::from_fn(w, h, |x, y| {
            let on = (x * n / w) == i;
            if on { Rgba([255, 180, 0, 255]) } else if (x + y) % 2 == 0 { Rgba([30, 30, 60, 255]) } else { Rgba([0, 0, 0, 0]) }
        })
    };
    // GIF, looping for ever, 8 frames at 100 ms.
    let mut gif = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut gif);
        enc.set_repeat(Repeat::Infinite).unwrap();
        for i in 0..8 {
            enc.encode_frame(image::Frame::from_parts(frame(i, 8, 320, 120), 0, 0, Delay::from_numer_denom_ms(100, 1))).unwrap();
        }
    }
    fs::write(d.join("spinner.gif"), &gif).unwrap();
    // APNG, 6 frames, plays twice.
    {
        let f = fs::File::create(d.join("plays twice.apng")).unwrap();
        let mut enc = png::Encoder::new(f, 320, 120);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(6, 2).unwrap();
        enc.set_frame_delay(1, 5).unwrap();
        let mut w = enc.write_header().unwrap();
        for i in 0..6 {
            w.write_image_data(frame(i, 6, 320, 120).as_raw()).unwrap();
        }
        w.finish().unwrap();
    }
    // Animated WebP, built from lossless frames.
    let mut frames = Vec::new();
    for i in 0..5 {
        let mut one = Vec::new();
        image_webp::WebPEncoder::new(&mut one).encode(frame(i, 5, 320, 120).as_raw(), 320, 120, image_webp::ColorType::Rgba8).unwrap();
        frames.push(one[12..].to_vec()); // drop the RIFF header: the VP8L chunk
    }
    fs::write(d.join("animated.webp"), animated_webp(320, 120, &frames, 150)).unwrap();
    // A big GIF: 120 frames of 1280 × 720, more than View keeps in memory at once on small budgets.
    let mut big = Vec::new();
    {
        let mut enc = GifEncoder::new_with_speed(&mut big, 30);
        enc.set_repeat(Repeat::Infinite).unwrap();
        for i in 0..120 {
            let img = RgbaImage::from_fn(1280, 720, |x, y| {
                let v = ((x + i * 10) / 40 + y / 40) % 2;
                if v == 0 { Rgba([200, 60, 60, 255]) } else { Rgba([40, 40, 40, 255]) }
            });
            enc.encode_frame(image::Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(40, 1))).unwrap();
        }
    }
    fs::write(d.join("long 1280x720.gif"), big).unwrap();
}

fn animated_webp(w: u32, h: u32, vp8l_chunks: &[Vec<u8>], ms: u32) -> Vec<u8> {
    let le24 = |v: u32| [v as u8, (v >> 8) as u8, (v >> 16) as u8];
    let mut body = b"WEBP".to_vec();
    body.extend_from_slice(b"VP8X");
    body.extend_from_slice(&10u32.to_le_bytes());
    body.push(0x02 | 0x10); // animation, alpha
    body.extend_from_slice(&[0, 0, 0]);
    body.extend_from_slice(&le24(w - 1));
    body.extend_from_slice(&le24(h - 1));
    body.extend_from_slice(b"ANIM");
    body.extend_from_slice(&6u32.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // background, loop for ever
    for chunk in vp8l_chunks {
        let mut f = Vec::new();
        f.extend_from_slice(&le24(0));
        f.extend_from_slice(&le24(0));
        f.extend_from_slice(&le24(w - 1));
        f.extend_from_slice(&le24(h - 1));
        f.extend_from_slice(&le24(ms));
        f.push(0x02); // don't blend
        f.extend_from_slice(chunk);
        body.extend_from_slice(b"ANMF");
        body.extend_from_slice(&(f.len() as u32).to_le_bytes());
        body.extend_from_slice(&f);
        if f.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

fn sizes(dir: &Path) {
    let d = dir.join("sizes");
    fs::create_dir_all(&d).unwrap();
    RgbImage::from_pixel(1, 1, Rgb([255, 0, 255])).save(d.join("1x1.png")).unwrap();
    RgbImage::from_fn(4000, 3, |x, _| Rgb([(x % 256) as u8, 0, 0])).save(d.join("wide 4000x3.png")).unwrap();
    // A photo-sized JPEG with detail everywhere, for timing.
    let photo = RgbImage::from_fn(8000, 6000, |x, y| {
        let n = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) >> 24;
        Rgb([((x / 31) % 256) as u8 ^ n as u8 / 8, ((y / 23) % 256) as u8, ((x + y) / 47 % 256) as u8])
    });
    fs::write(d.join("photo 8000x6000.jpg"), jpeg(&photo, 90)).unwrap();
    let mid = image::imageops::resize(&photo, 4000, 3000, image::imageops::FilterType::Triangle);
    fs::write(d.join("photo 4000x3000.jpg"), jpeg(&mid, 90)).unwrap();
    mid.save(d.join("photo 4000x3000.png")).unwrap();
}

/// A PNG too big to hold whole in memory on most machines, written row by row.
fn huge_png(path: &Path, w: u32, h: u32) {
    let f = std::io::BufWriter::new(fs::File::create(path).unwrap());
    let mut enc = png::Encoder::new(f, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fastest);
    let mut writer = enc.write_header().unwrap();
    let mut stream = writer.stream_writer().unwrap();
    let mut row = vec![0u8; w as usize * 3];
    for y in 0..h {
        for x in 0..w {
            let i = x as usize * 3;
            let cell = ((x / 1000) + (y / 1000)) % 2 == 0;
            row[i] = if cell { 230 } else { 30 };
            row[i + 1] = (y * 255 / h) as u8;
            row[i + 2] = (x * 255 / w) as u8;
        }
        stream.write_all(&row).unwrap();
    }
    stream.finish().unwrap();
}

fn damaged(dir: &Path) {
    let d = dir.join("damaged");
    fs::create_dir_all(&d).unwrap();
    let good = jpeg(&card(800, 600), 90);
    fs::write(d.join("cut off half way.jpg"), &good[..good.len() / 2]).unwrap();
    fs::write(d.join("empty.png"), b"").unwrap();
    fs::write(d.join("not an image.gif"), b"This is just text pretending to be a GIF.").unwrap();
    let noise: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    fs::write(d.join("random bytes.jpg"), noise).unwrap();
    // A PNG that's really a JPEG: the extension lies.
    fs::write(d.join("really a jpeg.png"), &good).unwrap();
    // A PNG whose header claims 100000 × 100000.
    let mut liar = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    liar.extend_from_slice(&13u32.to_be_bytes());
    liar.extend_from_slice(b"IHDR");
    liar.extend_from_slice(&ihdr);
    liar.extend_from_slice(&crc(b"IHDR", &ihdr).to_be_bytes());
    fs::write(d.join("claims 100000x100000.png"), liar).unwrap();
}

fn crc(kind: &[u8], data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data) {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn names(dir: &Path) {
    let d = dir.join("unicode names");
    fs::create_dir_all(&d).unwrap();
    let img = card(400, 300);
    for name in ["写真 テスト.jpg", "Ñandú – café.jpg", "🐱 cat.jpg", "IMG 2.jpg", "IMG 10.jpg", "IMG 1.jpg"] {
        fs::write(d.join(name), jpeg(&img, 85)).unwrap();
    }
}

fn many(dir: &Path, n: u32) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..n {
        let img = RgbImage::from_fn(1600, 1000, |x, y| Rgb([((x + i * 7) % 256) as u8, ((y + i * 3) % 256) as u8, (i * 37 % 256) as u8]));
        fs::write(dir.join(format!("frame {:03}.jpg", i + 1)), jpeg(&img, 80)).unwrap();
    }
}
