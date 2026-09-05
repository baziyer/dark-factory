import { readFileSync, writeFileSync, statSync } from 'node:fs';
import { deflateSync, inflateSync } from 'node:zlib';
import { createHash } from 'node:crypto';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';

// One 16-colour palette, including transparency. "u" in grids is a uniform slot.
const palette = {
  '.': '#00000000', o: '#0b0b0b', a: '#edbc91', b: '#b77c58',
  h: '#493329', c: '#e0743a', t: '#3ec7a0', s: '#8b95a5',
  y: '#f2c94c', r: '#ff5f56', k: '#5ad38a', d: '#191d24',
  m: '#303741', l: '#59616e', w: '#85583b', p: '#e5d5ad',
};
const rgba = Object.fromEntries(Object.entries(palette).map(([key, hex]) =>
  [key, Buffer.from(hex.slice(1) + (hex.length === 7 ? 'ff' : ''), 'hex')]));
const grid = text => {
  const rows = text.trim().split('\n').map(row => row.trim());
  assert(rows.every(row => row.length === rows[0].length), 'Ragged pixel grid');
  return rows;
};
const tint = (rows, colour) => rows.map(row => row.replaceAll('u', colour));
const mirror = rows => rows.map(row => [...row].reverse().join(''));
const blank = () => Array.from({ length: 16 }, () => Array(16).fill('.'));
function draw(target, rows, x = 0, y = 0) {
  rows.forEach((row, dy) => [...row].forEach((pixel, dx) => {
    assert(x + dx >= 0 && x + dx < 16 && y + dy >= 0 && y + dy < 16, 'Grid overflow');
    if (pixel !== '.') {
      assert(pixel in palette, `Unknown palette key ${pixel}`);
      target[y + dy][x + dx] = pixel;
    }
  }));
}

const head = grid(`
  .oooo.
  ouuuuo
  ohbaoa
  .obaa.
  ..ba..
`);
const hat = grid(`
  ..yy..
  .yyyy.
  oyyyyo
`);
const torso = grid(`
  .uuuu.
  ouuuuo
  ouuuuo
  ouuluo
  oommoo
`);
const legs = grid(`
  omoomo
  om..mo
  oo..oo
`);
const relaxedArm = grid(`
  .ou
  oau
  oba
  .oo
`);
const foldedArms = grid(`
  ouuuuuuo
  ouaaaaao
  .oobbuo.
`);
const raisedArm = grid(`
  .aa.
  oaao
  ouuo
  ouuo
  .ouo
  .ouu
  ..oo
`);
const typingArms = grid(`
  ouu...uo
  .ouaaaao
  ..ooooo.
`);
const keyboard = grid(`
  olllllo
  ommmmmo
`);
const clipboard = grid(`
  .mm.
  oppo
  opmo
  oppo
  .oo.
`);
const alert = grid(`
  r
  r
  .
  r
`);

function person(role, colour, activity, n) {
  const pixels = blank();
  const bob = activity === 'idle' ? n : 0;
  const lean = activity === 'busy' ? n : 0;
  draw(pixels, legs, 5, 12 + bob);
  draw(pixels, tint(torso, colour), 5, 8 + bob);
  if (activity === 'waiting') {
    draw(pixels, tint(foldedArms, colour), 4, 9);
  } else if (activity === 'busy') {
    draw(pixels, keyboard, 8, 12);
    draw(pixels, tint(typingArms, colour), 4 + lean, 9);
  } else {
    draw(pixels, tint(relaxedArm, colour), 3, 9 + bob);
    draw(pixels, tint(mirror(relaxedArm), colour), 10, 9 + bob);
    if (activity === 'needs-you') {
      draw(pixels, tint(raisedArm, colour), 2, 5);
      draw(pixels, alert, 12, 0);
    }
  }
  draw(pixels, tint(head, colour), 5 + lean, 3 + bob);
  if (role === 'overseer') {
    draw(pixels, hat, 5 + lean, 2 + bob);
    draw(pixels, clipboard, 10, 9 + bob);
  }
  return pixels;
}

// Complete tile silhouettes; repeatable structure rather than random texture.
const floor = grid(`
  oooooooooooooooo
  oddddddddddddddo
  odldddddddddlddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  oddddddddddddddo
  odldddddddddlddo
  oddddddddddddddo
  oooooooooooooooo
`);
const wall = grid(`
  llllllllllllllll
  mmmmmmmmmmmmmmmm
  mmmmmmmommmmmmmm
  mmmmmmmommmmmmmm
  mmmmmmmommmmmmmm
  mmmmmmmommmmmmmm
  oooooooooooooooo
  mmmommmmmmmommmm
  mmmommmmmmmommmm
  mmmommmmmmmommmm
  mmmommmmmmmommmm
  oooooooooooooooo
  mmmmmmmommmmmmmm
  mmmmmmmommmmmmmm
  dddddddddddddddd
  oooooooooooooooo
`);
const door = grid(`
  ..llllllllllll..
  ..lmmmmmmmmmml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooyoml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmooooooooml..
  ..lmmmmmmmmmml..
  ..oooooooooooo..
`);
const machine = grid(`
  ................
  ..oooooooooooo..
  .ollllllllllllo.
  .olmmmmmmmmmmmo.
  .omooooooooommo.
  .omodddddddommo.
  .omodkkkdddouuo.
  .omodddddddouuo.
  .omooooooooommo.
  .ommmmmmmmmmmmo.
  .ollllllllllllo.
  .omomomomomommo.
  .ommmmmmmmmmmmo.
  .oooooooooooooo.
  ..ommo....ommo..
  ..oooo....oooo..
`);
const desk = grid(`
  ................
  ................
  ................
  .....oooooo.....
  .....ommmmo.....
  .....oddddo.....
  .....oooooo.....
  .......oo.......
  .oooooooooooooo.
  ollllllllllllllo
  ommmmmmmmmmmmmmo
  oooooooooooooooo
  .ommo......ommo.
  .ommo......ommo.
  .ommo......ommo.
  .oooo......oooo.
`);
const crate = grid(`
  ................
  .oooooooooooooo.
  .owwwwwwwwwwwwo.
  .owhhhhhhhhhwho.
  .owwbhhhhhhwwho.
  .owhwwhhhhwwhho.
  .owhhwwhhwwhhho.
  .owhhhwwwwhhhho.
  .owhhhhwwhhhhho.
  .owhhhwwwwhhhho.
  .owhhwwhhwwhhho.
  .owhwwhhhhwwhho.
  .owwwhhhhhhwwho.
  .owwwwwwwwwwwwo.
  .oooooooooooooo.
  ................
`);
const pad = grid(`
  ................
  .mmmmmmmmmmmmmm.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .m............m.
  .mmmmmmmmmmmmmm.
  ................
`);

const sprites = new Map();
const providers = { claude_code: 'c', codex: 't', shell: 's' };
const activities = { busy: 2, waiting: 1, 'needs-you': 1, idle: 2 };
for (const role of ['worker', 'overseer']) {
  for (const [provider, colour] of Object.entries(providers)) {
    for (const [activity, count] of Object.entries(activities)) {
      for (let n = 0; n < count; n++) {
        sprites.set(`${role}.${provider}.${activity}.${n}`, person(role, colour, activity, n));
      }
    }
  }
}
function tile(name, rows) {
  const pixels = blank();
  draw(pixels, rows);
  sprites.set(name, pixels);
  return pixels;
}
tile('tile.floor.0', floor);
const floorVariant = tile('tile.floor.1', mirror(floor));
draw(floorVariant, grid(`mmm`), 6, 1);
tile('tile.wall', wall);
tile('tile.door', door);
tile('tile.machine.0', tint(machine, 'k'));
tile('tile.machine.1', tint(machine, 'c'));
tile('tile.desk', desk);
tile('tile.crate', crate);
tile('bay.free', pad);
const staged = tile('bay.staged', pad);
draw(staged, Array(5).fill('yyyyyyyyyy'), 3, 8);
const ready = tile('bay.ready', pad);
draw(ready, Array(10).fill('kkkkkkkkkk'), 3, 3);

const frame = 16;
const width = 8 * frame;
const height = Math.ceil(sprites.size / 8) * frame;
const pixels = Buffer.alloc(width * height * 4);
const atlas = { frame, sheet: 'sprites.png', frames: {} };
let index = 0;
for (const [name, rows] of sprites) {
  const x = (index % 8) * frame;
  const y = Math.floor(index++ / 8) * frame;
  atlas.frames[name] = { x, y };
  rows.forEach((row, dy) => row.forEach((key, dx) => {
    rgba[key].copy(pixels, ((y + dy) * width + x + dx) * 4);
  }));
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
  }
  return (crc ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const out = Buffer.alloc(data.length + 12);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, 4, 'ascii');
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, -4)), out.length - 4);
  return out;
}
const signature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(width, 0);
ihdr.writeUInt32BE(height, 4);
ihdr[8] = 8; // Eight bits per channel.
ihdr[9] = 6; // RGBA, no interlace.
const stride = width * 4;
const scanlines = Buffer.alloc((stride + 1) * height);
for (let y = 0; y < height; y++) pixels.copy(scanlines, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
const png = Buffer.concat([
  signature, chunk('IHDR', ihdr),
  chunk('IDAT', deflateSync(scanlines, { level: 9 })), chunk('IEND', Buffer.alloc(0)),
]);
const dataUrl = `data:image/png;base64,${png.toString('base64')}`;
const atlasJson = JSON.stringify(atlas, null, 2) + '\n';
const preview = `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Dark Factory · Sprite atlas</title>
<style>
  * { box-sizing: border-box; }
  body { margin: 32px; background: #0b0b0b; color: #e5d5ad; font: 13px system-ui,sans-serif; }
  h1 { font-size: 24px; margin-bottom: 8px; }
  p { color: #8b95a5; }
  h2 { font-size: 15px; margin-top: 32px; }
  section { display: grid; grid-template-columns: repeat(auto-fit,minmax(210px,1fr)); gap: 12px; max-width: 1440px; }
  figure { margin: 0; padding: 16px 10px; border: 1px solid #303741; background: #101318; text-align: center; }
  canvas { display: block; width: 64px; height: 64px; margin: 0 auto 12px; image-rendering: pixelated; }
  figcaption { color: #8b95a5; font: 11px ui-monospace,monospace; overflow-wrap: anywhere; }
</style>
<h1>Dark Factory / Sprite atlas</h1>
<p>${sprites.size} frames · 16 × 16 pixels · ${width} × ${height} sheet · 4× nearest-neighbour preview</p>
<main id="frames"></main>
<script>
const atlas = ${JSON.stringify(atlas)};
const image = new Image();
image.onload = () => {
  const groups = new Map();
  for (const [name, {x,y}] of Object.entries(atlas.frames)) {
    const parts = name.split('.');
    const group = parts[0] === 'tile' || parts[0] === 'bay' ? parts[0] : parts.slice(0,2).join(' / ');
    if (!groups.has(group)) {
      const title = document.createElement('h2');
      title.textContent = group;
      const section = document.createElement('section');
      document.getElementById('frames').append(title, section);
      groups.set(group, section);
    }
    const figure = document.createElement('figure');
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 64;
    canvas.setAttribute('role', 'img');
    canvas.setAttribute('aria-label', name);
    const context = canvas.getContext('2d');
    context.imageSmoothingEnabled = false;
    context.drawImage(image, x, y, 16, 16, 0, 0, 64, 64);
    const label = document.createElement('figcaption');
    label.textContent = name;
    figure.append(canvas, label);
    groups.get(group).append(figure);
  }
};
image.src = ${JSON.stringify(dataUrl)};
</script>
</html>
`;
const root = new URL('./', import.meta.url);
for (const [name, data] of Object.entries({
  'sprites.png': png,
  'atlas.json': atlasJson,
  'sprites.generated.ts': `export const spriteSheet = ${JSON.stringify(dataUrl)};\nexport const spriteAtlas = ${atlasJson.trim()} as const;\n`,
  'preview.html': preview,
})) writeFileSync(new URL(name, root), data);

// Decode the actual file: verify chunk boundaries, CRCs, format, and every RGBA byte.
const decodedFile = readFileSync(new URL('sprites.png', root));
assert.deepEqual(decodedFile.subarray(0, 8), signature);
const idats = [];
const chunkTypes = [];
let offset = 8;
while (offset < decodedFile.length) {
  const length = decodedFile.readUInt32BE(offset);
  const end = offset + length + 12;
  assert(end <= decodedFile.length, 'Truncated PNG chunk');
  const type = decodedFile.toString('ascii', offset + 4, offset + 8);
  const data = decodedFile.subarray(offset + 8, end - 4);
  assert.equal(decodedFile.readUInt32BE(end - 4), crc32(decodedFile.subarray(offset + 4, end - 4)));
  chunkTypes.push(type);
  if (type === 'IHDR') assert.deepEqual(data, ihdr);
  if (type === 'IDAT') idats.push(data);
  offset = end;
}
assert.equal(offset, decodedFile.length);
assert.deepEqual(chunkTypes, ['IHDR', 'IDAT', 'IEND']);
const inflated = inflateSync(Buffer.concat(idats));
assert.equal(inflated.length, (stride + 1) * height);
for (let y = 0; y < height; y++) {
  assert.equal(inflated[y * (stride + 1)], 0, 'Expected PNG filter zero');
  assert.deepEqual(inflated.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1)), pixels.subarray(y * stride, (y + 1) * stride));
}
const expectedNames = [];
for (const role of ['worker', 'overseer']) {
  for (const provider of ['claude_code', 'codex', 'shell']) {
    for (const suffix of ['busy.0', 'busy.1', 'waiting.0', 'needs-you.0', 'idle.0', 'idle.1']) {
      expectedNames.push(`${role}.${provider}.${suffix}`);
    }
  }
}
expectedNames.push('tile.floor.0', 'tile.floor.1', 'tile.wall', 'tile.door', 'tile.machine.0', 'tile.machine.1', 'tile.desk', 'tile.crate', 'bay.free', 'bay.staged', 'bay.ready');
const savedAtlas = JSON.parse(readFileSync(new URL('atlas.json', root), 'utf8'));
assert.deepEqual(savedAtlas, atlas);
assert.deepEqual(Object.keys(savedAtlas.frames).sort(), expectedNames.sort());
const occupied = new Set();
for (const {x, y} of Object.values(savedAtlas.frames)) {
  assert(Number.isInteger(x) && Number.isInteger(y) && x >= 0 && y >= 0);
  assert(x % 16 === 0 && y % 16 === 0 && x + 16 <= width && y + 16 <= height);
  assert(!occupied.has(`${x},${y}`), 'Overlapping atlas entries');
  occupied.add(`${x},${y}`);
}
assert.equal(Object.keys(palette).length, 16);
for (const role of ['worker', 'overseer']) {
  for (const provider of Object.keys(providers)) {
    for (const activity of ['busy', 'idle']) {
      assert.notDeepEqual(sprites.get(`${role}.${provider}.${activity}.0`), sprites.get(`${role}.${provider}.${activity}.1`));
    }
    assert.equal(sprites.get(`${role}.${provider}.needs-you.0`).flat().filter(key => key === 'r').length, 3);
  }
}
console.log(`Verified PNG decode, 16-colour palette, exact atlas names, bounds, and animation frames.`);
console.log(`Sheet: ${width} × ${height} RGBA; frames: ${sprites.size}`);
for (const name of ['gen-sprites.mjs', 'sprites.png', 'atlas.json', 'sprites.generated.ts', 'preview.html']) {
  const path = fileURLToPath(new URL(name, root));
  console.log(`${name}: ${statSync(path).size} bytes; sha256 ${createHash('sha256').update(readFileSync(path)).digest('hex')}`);
}
