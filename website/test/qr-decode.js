// Reads a QR code back from the SVG the build writes, the way a camera would:
// rasterise its modules and hand the pixels to an independent decoder.
import jsQR from "jsqr";

export function decodeQr(svg, scale = 4) {
  const extent = Number(/viewBox="0 0 (\d+) \1"/.exec(svg)[1]);
  const path = /<path[^>]* d="([^"]+)"/.exec(svg)[1];
  const dark = new Set();
  for (const [, x, y, width] of path.matchAll(/M(\d+) (\d+)h(\d+)v1h-\d+z/g)) {
    for (let i = 0; i < Number(width); i++) dark.add(`${Number(x) + i},${y}`);
  }
  const side = extent * scale;
  const pixels = new Uint8ClampedArray(side * side * 4);
  for (let py = 0; py < side; py++) {
    for (let px = 0; px < side; px++) {
      const value = dark.has(`${Math.floor(px / scale)},${Math.floor(py / scale)}`) ? 17 : 255;
      pixels.set([value, value, value, 255], (py * side + px) * 4);
    }
  }
  return jsQR(pixels, side, side)?.data ?? null;
}
