// A QR code as an inline SVG, generated when the site builds.
import { encode } from "uqr";

const escapeHtml = (text) =>
  text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// Dark modules on a white field with the standard four-module quiet zone, in
// every theme: phone cameras read dark-on-light codes most reliably.
export function qrSvg(text, { label, quietZone = 4 } = {}) {
  const { data, size } = encode(text, { ecc: "M", border: 0 });
  const extent = size + 2 * quietZone;
  // One horizontal run per stretch of dark modules keeps the path short.
  let path = "";
  data.forEach((row, y) => {
    for (let x = 0; x < size; x++) {
      if (!row[x]) continue;
      let end = x;
      while (end + 1 < size && row[end + 1]) end++;
      path += `M${x + quietZone} ${y + quietZone}h${end - x + 1}v1h${-(end - x + 1)}z`;
      x = end;
    }
  });
  const name = label ? ` role="img" aria-label="${escapeHtml(label)}"` : ` aria-hidden="true"`;
  return `<svg class="qr" viewBox="0 0 ${extent} ${extent}" shape-rendering="crispEdges"${name}><rect width="${extent}" height="${extent}" fill="#fff" /><path fill="#111" d="${path}" /></svg>`;
}
