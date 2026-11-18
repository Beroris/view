// Draws View's icon from code: a teal rounded square with a white picture in it (a frame, a
// mountain, a sun). Writes platforms/windows/res/app.ico, assets/icon.png and assets/icon.svg.
// Run with: node scripts/gen_icon.mjs
import { writeFileSync } from "node:fs";
import { deflateSync, crc32 } from "node:zlib";

const TILE = [0x0d, 0x94, 0x88]; // teal: Prism is violet, Orbit amber
const at = (p) => new URL(`../${p}`, import.meta.url);

// The design, in a 64 × 64 box.
const FRAME = { x0: 12, y0: 16, x1: 52, y1: 48, r: 6.5, stroke: 4.2 };
const SUN = { x: 40.5, y: 26.5, r: 3.8 };
const MOUNTAIN = [
  [14, 46],
  [26.5, 30],
  [33.5, 38.5],
  [38.5, 33],
  [50, 46],
];

function roundedRect(px, py, x0, y0, x1, y1, r) {
  const cx = (x0 + x1) / 2, cy = (y0 + y1) / 2;
  const qx = Math.abs(px - cx) - ((x1 - x0) / 2 - r);
  const qy = Math.abs(py - cy) - ((y1 - y0) / 2 - r);
  return Math.hypot(Math.max(qx, 0), Math.max(qy, 0)) + Math.min(Math.max(qx, qy), 0) - r;
}

function inPolygon(px, py, pts) {
  let inside = false;
  for (let i = 0, j = pts.length - 1; i < pts.length; j = i++) {
    const [xi, yi] = pts[i], [xj, yj] = pts[j];
    if (yi > py !== yj > py && px < ((xj - xi) * (py - yi)) / (yj - yi) + xi) inside = !inside;
  }
  return inside;
}

// Is this point of the 64-unit design inside the tile, and is it white?
function sample(u, v) {
  const tile = roundedRect(u, v, 0, 0, 64, 64, 14) <= 0;
  const f = FRAME;
  const outer = roundedRect(u, v, f.x0, f.y0, f.x1, f.y1, f.r);
  const inner = roundedRect(u, v, f.x0 + f.stroke, f.y0 + f.stroke, f.x1 - f.stroke, f.y1 - f.stroke, f.r - f.stroke + 1);
  const frame = outer <= 0 && inner > 0;
  const sun = Math.hypot(u - SUN.x, v - SUN.y) <= SUN.r;
  const mountain = inner <= 0 && inPolygon(u, v, MOUNTAIN);
  return [tile, frame || sun || mountain];
}

// Straight-alpha RGBA, top-down, 8 × 8 samples a pixel.
function render(size) {
  const px = new Uint8Array(size * size * 4);
  const n = 8;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let cover = 0, white = 0;
      for (let j = 0; j < n; j++) {
        for (let i = 0; i < n; i++) {
          const [t, w] = sample(((x + (i + 0.5) / n) * 64) / size, ((y + (j + 0.5) / n) * 64) / size);
          if (t) {
            cover++;
            if (w) white++;
          }
        }
      }
      const k = (y * size + x) * 4;
      const a = cover / (n * n);
      const mix = cover ? white / cover : 0;
      for (let c = 0; c < 3; c++) px[k + c] = Math.round(TILE[c] + (255 - TILE[c]) * mix);
      px[k + 3] = Math.round(a * 255);
    }
  }
  return px;
}

function png(size) {
  const px = render(size);
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) Buffer.from(px.buffer, y * size * 4, size * 4).copy(raw, y * (size * 4 + 1) + 1);
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type), data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(td));
    return Buffer.concat([len, td, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr.set([8, 6, 0, 0, 0], 8);
  return Buffer.concat([Buffer.from("\x89PNG\r\n\x1a\n", "latin1"), chunk("IHDR", ihdr), chunk("IDAT", deflateSync(raw, { level: 9 })), chunk("IEND", Buffer.alloc(0))]);
}

// Small sizes as BMP-style DIBs, 256 as PNG (what Windows itself does).
function ico(sizes) {
  const header = Buffer.alloc(6 + sizes.length * 16);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(sizes.length, 4);
  let offset = header.length;
  const blobs = sizes.map((size, n) => {
    let blob;
    if (size >= 256) {
      blob = png(size);
    } else {
      const px = render(size);
      const andRow = Math.ceil(size / 32) * 4;
      blob = Buffer.alloc(40 + size * size * 4 + andRow * size);
      blob.writeUInt32LE(40, 0);
      blob.writeInt32LE(size, 4);
      blob.writeInt32LE(size * 2, 8);
      blob.writeUInt16LE(1, 12);
      blob.writeUInt16LE(32, 14);
      blob.writeUInt32LE(size * size * 4, 20);
      for (let y = 0; y < size; y++) {
        const src = (size - 1 - y) * size * 4;
        for (let x = 0; x < size; x++) {
          const s = src + x * 4, d = 40 + (y * size + x) * 4;
          blob[d] = px[s + 2];
          blob[d + 1] = px[s + 1];
          blob[d + 2] = px[s];
          blob[d + 3] = px[s + 3];
        }
      }
    }
    const e = 6 + n * 16;
    header.writeUInt8(size >= 256 ? 0 : size, e);
    header.writeUInt8(size >= 256 ? 0 : size, e + 1);
    header.writeUInt16LE(1, e + 4);
    header.writeUInt16LE(32, e + 6);
    header.writeUInt32LE(blob.length, e + 8);
    header.writeUInt32LE(offset, e + 12);
    offset += blob.length;
    return blob;
  });
  return Buffer.concat([header, ...blobs]);
}

const hex = (c) => "#" + c.map((v) => v.toString(16).padStart(2, "0")).join("");
const f = FRAME;
const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <rect width="64" height="64" rx="14" fill="${hex(TILE)}"/>
  <rect x="${f.x0 + f.stroke / 2}" y="${f.y0 + f.stroke / 2}" width="${f.x1 - f.x0 - f.stroke}" height="${f.y1 - f.y0 - f.stroke}" rx="${f.r - f.stroke / 2}" fill="none" stroke="#fff" stroke-width="${f.stroke}"/>
  <clipPath id="in"><rect x="${f.x0 + f.stroke}" y="${f.y0 + f.stroke}" width="${f.x1 - f.x0 - 2 * f.stroke}" height="${f.y1 - f.y0 - 2 * f.stroke}" rx="${f.r - f.stroke + 1}"/></clipPath>
  <polygon clip-path="url(#in)" fill="#fff" points="${MOUNTAIN.map((p) => p.join(",")).join(" ")}"/>
  <circle cx="${SUN.x}" cy="${SUN.y}" r="${SUN.r}" fill="#fff"/>
</svg>
`;

writeFileSync(at("platforms/windows/res/app.ico"), ico([16, 20, 24, 32, 40, 48, 64, 256]));
writeFileSync(at("assets/icon.png"), png(256));
writeFileSync(at("assets/icon.svg"), svg);
console.log("wrote app.ico, icon.png, icon.svg");
