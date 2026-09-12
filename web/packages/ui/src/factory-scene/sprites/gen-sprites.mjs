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
const keep = (rows, keys) => rows.map(row => [...row].map(key => keys.includes(key) ? key : '.').join(''));

// Art lives here: shared face, chosen hair/clothes/shoes, then pose/equipment.
// a/b are skin/light-shadow; u is the sleeve colour. All parts face forward.
const head = grid(`
  .oooo.
  oaaaao
  oaooao
  oaaaao
  .obbo.
  ..ba..
`);
const hair = [
  grid(`
    ..oooo..
    .ouuuuo.
    .ouuuuo.
    ..u.....
  `),
  grid(`
    .oooooo.
    .ouumuo.
    .ouu.uo.
    ..u...u.
  `),
  grid(`
    ..oo.oo.
    .ouuouuo
    ouuuuuuo
    .u....u.
  `),
  grid(`
    ..oooo..
    .ouuuuo.
    .ouuuuo.
    ..u...uo
    ......uo
    ......uo
  `),
];
const outfits = [
  grid(`
    ........
    ..o..o..
    .ouppuo.
    ouoppouo
    ouuppuuo
    ouuppuuo
    ouo..ouo
  `),
  grid(`
    ........
    ........
    .oummuo.
    opuppupo
    opuuuupo
    ouuuuuuo
    .ouuuuo.
  `),
  grid(`
    ........
    ........
    ..ouuo..
    .ouoouo.
    .ouuuuo.
    .ouuuuo.
    ..oooo..
  `),
  grid(`
    .ou..uo.
    ouu..uuo
    ouummuuo
    .ouuuuo.
    oupuupuo
    ouummuuo
    .oooooo.
  `),
];
// Stable slots: revise a part in place rather than reorder identities.
const hairStyles = ['Cropped', 'Side part', 'Curls', 'Tied'];
const hairColours = [
  { name: 'black', label: 'Black', colour: 'd' },
  { name: 'brown', label: 'Brown', colour: 'h' },
  { name: 'auburn', label: 'Auburn', colour: 'w' },
  { name: 'blonde', label: 'Blonde', colour: 'y' },
];
const outfitStyles = ['Jacket', 'Overalls', 'Shirt', 'Hoodie'];
const clothesColours = [
  { name: 'copper', label: 'Copper', colour: 'c' },
  { name: 'slate', label: 'Slate', colour: 's' },
  { name: 'linen', label: 'Linen', colour: 'p' },
  { name: 'steel', label: 'Steel', colour: 'l' },
];
const legs = grid(`
  .omoomo.
  .omoomo.
  .oo..oo.
`);
const shoes = [
  { name: 'coal', label: 'Coal', colour: 'm' },
  { name: 'slate', label: 'Slate', colour: 's' },
  { name: 'brown', label: 'Brown', colour: 'w' },
  { name: 'cream', label: 'Cream', colour: 'p' },
];
const shoe = grid(`
  ........
  .uu..uu.
  ouo..ouo
`);
const relaxedArm = grid(`
  ou
  ou
  oa
  oo
`);
const foldedArms = grid(`
  ouabbaao
  .oooooo.
`);
const raisedArm = grid(`
  .oo
  oaa
  oao
  ouo
  ouo
  .ou
  ..o
`);
const typingArms = grid(`
  ou....uo
  oa....ao
  ..aaaa..
`);
const keyboard = grid(`
  .oooooo.
  olslsllo
  .oooooo.
`);
const clipboard = grid(`
  .ss.
  oppo
  osso
  opmo
  oooo
`);
const hat = grid(`
  ..oooo..
  .oyyyyo.
  oyyyyyyo
`);
const alert = grid(`
  oro
  oro
  .o.
  oro
`);
const faceFeatures = [
  { name: 'none', label: 'None', part: [] },
  { name: 'glasses', label: 'Glasses', part: grid(`sosos`), x: 5, y: 4 },
  { name: 'beard', label: 'Beard', part: grid(`hhhh\n.hh.`), y: 6 },
  { name: 'moustache', label: 'Moustache', part: grid(`hhh`), y: 5 },
];
const tools = [
  { name: 'none', label: 'None', part: [], x: 0, y: 0 },
  { name: 'clipboard', label: 'Clipboard', part: clipboard, x: 11, y: 9 },
  { name: 'wrench', label: 'Wrench', part: grid(`s.s\n.s.\n.s.\n.s.\noso`), x: 12, y: 8 },
  { name: 'mug', label: 'Mug', part: grid(`pppoo\nppp.o\npppoo\nooo..`), x: 11, y: 10 },
  { name: 'tablet', label: 'Tablet', part: grid(`oooo\notto\nokto\nooso`), x: 11, y: 10 },
];
const headwear = [
  { name: 'none', label: 'None', part: [], x: 0, y: 0 },
  { name: 'hard-hat', label: 'Hard hat', part: hat, x: 4, y: 0 },
  { name: 'cap', label: 'Cap', part: grid(`..oooo..\n.otttto.\notttttoo\n.oottttt`), x: 4, y: 0 },
  { name: 'headset', label: 'Headset', part: grid(`..ssssss..\n.s......s.\n.s......s.\nss......ss\nst......ts\n........so\n......sss.`), x: 3, y: 0 },
];
const skinTones = [
  { name: 'light', label: 'Light', skin: 'p', shadow: 'a' },
  { name: 'warm', label: 'Warm', skin: 'a', shadow: 'b' },
  { name: 'brown', label: 'Brown', skin: 'b', shadow: 'w' },
  { name: 'deep', label: 'Deep', skin: 'w', shadow: 'h' },
];
const optionGroups = {
  skin: skinTones.map(({ name, label }) => ({ name, label })),
  hair: hairStyles.map((label, index) => ({ name: `style-${index}`, label })),
  hair_colour: hairColours.map(({ name, label }) => ({ name, label })),
  face: faceFeatures.map(({ name, label }) => ({ name, label })),
  outfit: outfitStyles.map((label, index) => ({ name: `outfit-${index}`, label })),
  clothes_colour: clothesColours.map(({ name, label }) => ({ name, label })),
  shoes: shoes.map(({ name, label }) => ({ name, label })),
  tool: tools.map(({ name, label }) => ({ name, label })),
  headwear: headwear.map(({ name, label }) => ({ name, label })),
};
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
const activities = ['busy', 'waiting', 'needs-you', 'idle'];
const add = (name, build) => { const pixels = blank(); build(pixels); sprites.set(name, pixels); };
const arms = activity => activity === 'waiting' ? [foldedArms] : activity === 'busy' ? [typingArms] : activity === 'needs-you' ? [relaxedArm, mirror(relaxedArm), raisedArm] : [relaxedArm, mirror(relaxedArm)];
const armPosition = (activity, index) => index === 0 ? [activity === 'waiting' || activity === 'busy' ? 4 : 3, activity === 'waiting' ? 11 : 9] : index === 1 ? [11, 9] : [2, 4];
for (const [skinIndex, tone] of skinTones.entries()) for (const activity of activities) add(`person.skin.${skinIndex}.${activity}`, pixels => {
  const colour = rows => rows.map(row => row.replace(/[ab]/g, key => key === 'a' ? tone.skin : tone.shadow));
  draw(pixels, colour(head), 5, 2);
  arms(activity).forEach((part, index) => draw(pixels, keep(colour(part).map(row => row.replaceAll('u', tone.skin)), [tone.skin, tone.shadow]), ...armPosition(activity, index)));
});
for (const [outfitIndex, outfit] of outfits.entries()) for (const [colourIndex, colour] of clothesColours.entries()) for (const activity of activities) add(`person.outfit.${outfitIndex}.${colourIndex}.${activity}`, pixels => {
  draw(pixels, tint(outfit, colour.colour), 4, 6);
  draw(pixels, outfitIndex === 1 ? legs.map(row => row.replaceAll('m', colour.colour)) : legs, 4, outfitIndex === 1 ? 12 : 13);
  if (activity === 'busy') draw(pixels, keyboard, 2, 12);
  arms(activity).forEach((part, index) => {
    const [x, y] = armPosition(activity, index);
    if (outfitIndex === 2) part = part.map((row, dy) => y + dy === 9 ? row : row.replaceAll('u', 'a'));
    // Hands and short sleeves reveal the skin layer beneath the clothes.
    part.forEach((row, dy) => [...row].forEach((key, dx) => {
      if (key === 'a' || key === 'b') pixels[y + dy][x + dx] = '.';
    }));
    draw(pixels, keep(tint(part, colour.colour), ['o', colour.colour]), x, y);
  });
});
for (const [hairIndex, style] of hair.entries()) for (const [colourIndex, colour] of hairColours.entries()) for (const activity of activities) add(`person.hair.${hairIndex}.${colourIndex}.${activity}`, pixels => draw(pixels, tint(style, colour.colour), 4, 1));
for (const [featureIndex, feature] of faceFeatures.entries()) for (const activity of activities) add(`person.face.${featureIndex}.${activity}`, pixels => draw(pixels, feature.part, feature.x ?? 6, feature.y ?? 4));
for (const [shoeIndex, colour] of shoes.entries()) for (const activity of activities) add(`person.shoes.${shoeIndex}.${activity}`, pixels => draw(pixels, tint(shoe, colour.colour), 4, 13));
for (const [toolIndex, tool] of tools.entries()) for (const activity of activities) add(`person.tool.${toolIndex}.${activity}`, pixels => {
  draw(pixels, tool.part, tool.x, tool.y);
});
for (const [hatIndex, item] of headwear.entries()) for (const activity of activities) add(`person.headwear.${hatIndex}.${activity}`, pixels => draw(pixels, item.part, item.x, item.y));
for (const role of ['worker', 'overseer']) for (const [provider, colour] of Object.entries(providers)) for (const activity of activities) add(`person.system.${role}.${provider}.${activity}`, pixels => {
  draw(pixels, [colour], 9, 8);
  if (role === 'overseer') draw(pixels, ['yy'], 7, 8);
  if (activity === 'needs-you') draw(pixels, alert, 13, 0);
});
// Compare finished portraits: transparent layer differences can disappear in composition.
const portrait = (activity, appearance = {}) => {
  const v = { skin: 1, hair: 0, hair_colour: 1, face: 0, outfit: 0, clothes_colour: 0, shoes: 0, tool: 0, headwear: 0, ...appearance };
  const pixels = blank();
  for (const name of [`skin.${v.skin}`, `outfit.${v.outfit}.${v.clothes_colour}`, `hair.${v.hair}.${v.hair_colour}`, `face.${v.face}`, `shoes.${v.shoes}`, `tool.${v.tool}`, `headwear.${v.headwear}`, 'system.worker.codex']) {
    draw(pixels, sprites.get(`person.${name}.${activity}`).map(row => row.join('')));
  }
  return pixels;
};
for (const activity of activities) {
  for (let colour = 0; colour < clothesColours.length; colour++) {
    const dressed = outfits.map((_, outfit) => portrait(activity, { outfit, clothes_colour: colour }).flat());
    for (let first = 0; first < dressed.length; first++) for (let second = first + 1; second < dressed.length; second++) {
      const difference = dressed[first].filter((pixel, index) => pixel !== dressed[second][index]).length;
      assert(difference >= 8, `Clothing needs more than trim differences: ${activity}/${colour}/${first}/${second}: ${difference} pixels`);
    }
  }
  for (const [group, options] of Object.entries(optionGroups)) {
    assert.equal(new Set(options.map((_, index) => JSON.stringify(portrait(activity, { [group]: index })))).size, options.length, `Indistinguishable ${group}: ${activity}`);
  }
  for (const [skin, tone] of skinTones.entries()) {
    const pixels = portrait(activity, { skin });
    if (activity === 'idle') {
      assert.equal(portrait(activity, { skin, outfit: 2 })[10][4], tone.skin, 'Shirt must expose forearms');
      assert.equal(pixels[10][4], clothesColours[0].colour, 'Jacket must retain long sleeves');
    }
    arms(activity).forEach((part, index) => {
      const [x, y] = armPosition(activity, index);
      part.forEach((row, dy) => [...row].forEach((key, dx) => {
        if (key === 'a' || key === 'b') assert.equal(pixels[y + dy][x + dx], key === 'a' ? tone.skin : tone.shadow, `Hidden hand: ${activity}/${skin}`);
      }));
    });
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
const atlasJson = JSON.stringify(atlas);
const preview = `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Dark Factory · Sprite atlas</title>
<style>
  * { box-sizing: border-box; }
  body { margin: 24px; background: #08131d; color: #e5d5ad; font: 13px system-ui,sans-serif; }
  h1 { font-size: 24px; margin-bottom: 8px; }
  p, figcaption { color: #8b95a5; }
  h2 { font-size: 15px; margin-top: 24px; }
  section { display: grid; grid-template-columns: repeat(auto-fit,minmax(240px,1fr)); gap: 12px; max-width: 1100px; }
  figure { margin: 0; padding: 12px; border: 1px solid #303741; background: #101f2b; }
  .scales { display: flex; align-items: end; justify-content: space-between; margin: 12px 0; }
  canvas { display: block; image-rendering: pixelated; }
  small { display: block; margin-top: 6px; text-align: center; color: #8b95a5; }
  label { display: inline-block; margin: 0 16px 12px 0; }
  select { font: inherit; color: inherit; background: #191d24; padding: 6px; }
</style>
<h1>Dark Factory / Sprite workbench</h1>
<p>${sprites.size} reusable layers · shared 16-colour palette · 16 × 16 footprint.</p>
<label>Role <select id="role"><option value="worker">Worker</option><option value="overseer">Overseer</option></select></label>
<label>Provider badge <select id="provider">${Object.keys(providers).map(provider => `<option>${provider}</option>`).join('')}</select></label>
<p>Each pose: native 1×, normal display 3×, enlarged 8×, over the factory floor tile.</p>
${hairStyles.map((style, index) => `<h2>${index} · ${style}</h2><section>${activities.map(activity => `<figure><figcaption>${activity}</figcaption><div class="scales">${[1, 3, 8].map(scale => `<div><canvas width="16" height="16" style="width:${16 * scale}px;height:${16 * scale}px" data-identity="${index}" data-activity="${activity}" role="img" aria-label="${style}, ${activity}, ${scale}×"></canvas><small>${scale}×</small></div>`).join('')}</div></figure>`).join('')}</section>`).join('')}
<h2>Floor / equipment bays</h2>
<section>${[...sprites.keys()].filter(name => /^(tile|bay)\./.test(name)).map(name => `<figure><figcaption>${name}</figcaption><canvas width="16" height="16" style="width:48px;height:48px" data-tile="${name}" role="img" aria-label="${name}"></canvas></figure>`).join('')}</section>
<p>Edit hair, outfits, identities or equipment in gen-sprites.mjs, run <code>node web/packages/ui/src/factory-scene/sprites/gen-sprites.mjs</code>, reload this page, then inspect the console fixture floor.</p>
<script>
const atlas = ${JSON.stringify(atlas)};
const image = new Image();
const role = document.getElementById('role');
const provider = document.getElementById('provider');
function paint() {
  for (const canvas of document.querySelectorAll('canvas')) {
    const context = canvas.getContext('2d');
    context.clearRect(0, 0, 16, 16);
    const floor = atlas.frames['tile.floor.0'];
    context.drawImage(image, floor.x, floor.y, 16, 16, 0, 0, 16, 16);
    const identity = Number(canvas.dataset.identity);
    const activity = canvas.dataset.activity;
    const names = canvas.dataset.tile ? [canvas.dataset.tile] : [
      'person.skin.' + (identity < 2 ? 1 : 2) + '.' + activity,
      'person.outfit.' + identity + '.' + identity + '.' + activity,
      'person.hair.' + identity + '.' + identity + '.' + activity,
      'person.face.0.' + activity,
      'person.shoes.' + identity + '.' + activity,
      'person.tool.' + (role.value === 'overseer' ? 1 : 0) + '.' + activity,
      'person.headwear.' + (role.value === 'overseer' ? 1 : 0) + '.' + activity,
      'person.system.' + role.value + '.' + provider.value + '.' + activity,
    ];
    for (const name of names) { const {x,y} = atlas.frames[name]; context.drawImage(image, x, y, 16, 16, 0, 0, 16, 16); }
  }
}
image.onload = () => {
  paint();
  for (const select of [role, provider]) select.onchange = paint;
};
image.src = ${JSON.stringify(dataUrl)};
</script>
</html>
`;
const root = new URL('./', import.meta.url);
for (const [name, data] of Object.entries({
  'sprites.png': png,
  'sprites.generated.ts': `export const spriteSheet = ${JSON.stringify(dataUrl)};\nexport const spriteSheetSize = ${JSON.stringify({ width, height })} as const;\nexport const spriteAtlas = ${atlasJson} as const;\nexport const spriteOptions = ${JSON.stringify(optionGroups)} as const;\n`,
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
const expectedNames = [...sprites.keys()];
const generated = readFileSync(new URL('sprites.generated.ts', root), 'utf8');
const savedAtlas = JSON.parse(generated.slice(generated.indexOf('spriteAtlas = ') + 14, generated.indexOf(' as const;\nexport const spriteOptions')));
assert.deepEqual(savedAtlas, atlas);
assert.match(generated, new RegExp(`spriteSheetSize = \\{"width":${width},"height":${height}\\}`));
assert.deepEqual(Object.keys(savedAtlas.frames).sort(), expectedNames.sort());
const occupied = new Set();
for (const {x, y} of Object.values(savedAtlas.frames)) {
  assert(Number.isInteger(x) && Number.isInteger(y) && x >= 0 && y >= 0);
  assert(x % 16 === 0 && y % 16 === 0 && x + 16 <= width && y + 16 <= height);
  assert(!occupied.has(`${x},${y}`), 'Overlapping atlas entries');
  occupied.add(`${x},${y}`);
}
assert.equal(Object.keys(palette).length, 16);
for (const provider of Object.keys(providers)) assert.equal(sprites.get(`person.system.worker.${provider}.needs-you`).flat().filter(key => key === 'r').length, 3);
console.log(`Verified PNG decode, 16-colour palette, exact atlas names, bounds, and sprite layers.`);
console.log(`Sheet: ${width} × ${height} RGBA; frames: ${sprites.size}`);
for (const name of ['gen-sprites.mjs', 'sprites.png', 'sprites.generated.ts', 'preview.html']) {
  const path = fileURLToPath(new URL(name, root));
  console.log(`${name}: ${statSync(path).size} bytes; sha256 ${createHash('sha256').update(readFileSync(path)).digest('hex')}`);
}
