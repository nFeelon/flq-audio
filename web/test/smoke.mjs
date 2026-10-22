// Node smoke test for flq_wasm.wasm — run: node web/test/smoke.mjs
import { readFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../../', import.meta.url));
const wasmBytes = readFileSync(join(root, 'web/flq_wasm.wasm'));
const { instance } = await WebAssembly.instantiate(wasmBytes);
const W = instance.exports;
const mem = () => new Uint8Array(W.memory.buffer);
const out = () => mem().slice(W.flq_out_ptr(), W.flq_out_ptr() + W.flq_out_len());
const err = () => new TextDecoder().decode(mem().slice(W.flq_err_ptr(), W.flq_err_ptr() + W.flq_err_len()));

function open(path) {
  const b = readFileSync(path);
  const p = W.flq_alloc(b.length);
  mem().set(b, p);
  const h = W.flq_open(p, b.length);
  W.flq_free(p, b.length);
  if (!h) throw new Error(`open ${path}: ${err()}`);
  return h;
}
function info(h) {
  if (!W.flq_info_json(h)) throw new Error(`info: ${err()}`);
  return JSON.parse(new TextDecoder().decode(out()));
}
function decF32(h, t, start, len) {
  if (!W.flq_decode_f32(h, t, start, len)) throw new Error(`decode_f32: ${err()}`);
  return new Float32Array(out().buffer.slice(0));
}
function wav(h, t) {
  if (!W.flq_wav(h, t)) throw new Error(`wav: ${err()}`);
  return out();
}

// CLI-decoded reference WAV for the same file/track
function cliWav(flqPath, outDir) {
  execFileSync(join(root, 'target/release/flq.exe'), ['decode', flqPath, '-o', outDir], { stdio: 'pipe' });
  return outDir;
}
function wavPcm(b) { // extract data chunk bytes
  for (let off = 12; off + 8 <= b.length;) {
    const tag = b.toString('ascii', off, off + 4);
    const n = b.readUInt32LE(off + 4);
    if (tag === 'data') return b.slice(off + 8, off + 8 + n);
    off += 8 + n + (n & 1);
  }
  throw new Error('no data chunk');
}

let pass = 0;
const vecf = join(root, 'testdata/v1/v1_partial_frame.flq');
const demof = join(root, 'web/samples/demo.flq');

// 1. info JSON on both files
{
  const h = open(vecf);
  const j = info(h);
  if (j.frame_len !== 16384 || j.tracks.length !== 1 || j.tracks[0].frames !== 16507)
    throw new Error(`bad info: ${JSON.stringify(j)}`);
  const t0 = j.tracks[0];
  if (j.version !== 1 || !j.has_index || t0.nframes !== 2 || !(t0.coded > 0) || typeof t0.tags_len !== 'number')
    throw new Error(`bad info fields: ${JSON.stringify(j)}`);
  W.flq_close(h);
  const d = open(demof);
  const dj = info(d);
  if (dj.tracks.length !== 2 || dj.tracks[0].name !== 'drums.wav')
    throw new Error(`bad demo info: ${JSON.stringify(dj)}`);
  if (dj.tracks.some((t) => !(t.coded > 0)))
    throw new Error(`demo coded sizes missing: ${JSON.stringify(dj)}`);
  W.flq_close(d);
  console.log('info ok: vec 1x16507f, demo 2 tracks'); pass++;
}

// 2. decode_f32 across a frame boundary (frame 0 ends at 16384) vs CLI WAV
{
  const h = open(vecf);
  const dir = mkdtempSync(join(tmpdir(), 'flqsmoke'));
  cliWav(vecf, dir);
  const ref = readFileSync(join(dir, 'p.wav'));
  const pcm = wavPcm(ref);
  const ch = 2;
  const expect = (i) => pcm.readInt16LE(i * ch * 2) / 32768; // ch 0
  const start = 16000, len = Math.min(1000, 16507 - start); // spans 16384 boundary, clamps at EOF
  const got = decF32(h, 0, start, len);
  if (got.length !== len * ch) throw new Error(`len ${got.length}, want ${len * ch}`);
  for (let i = 0; i < len; i++) {
    for (let c = 0; c < ch; c++) {
      const e = pcm.readInt16LE((start + i) * ch * 2 + c * 2) / 32768;
      if (got[i * ch + c] !== e)
        throw new Error(`sample ${start + i} ch${c}: got ${got[i * ch + c]}, want ${e}`);
    }
  }
  // 3. flq_wav bytes == CLI output bytes
  const w = wav(h, 0);
  if (Buffer.compare(Buffer.from(w), ref) !== 0)
    throw new Error(`flq_wav ${w.length}B != cli ${ref.length}B`);
  W.flq_close(h);
  console.log(`decode_f32 frame-boundary range exact; flq_wav == CLI (${ref.length} B)`); pass++;
}

// 4. demo end-to-end: decode a chunk of both tracks
{
  const h = open(demof);
  const j = info(h);
  for (let t = 0; t < j.tracks.length; t++) {
    const g = decF32(h, t, 0, j.tracks[t].rate); // first second
    if (g.length !== j.tracks[t].rate * j.tracks[t].channels)
      throw new Error(`demo track ${t} chunk len`);
    const w = wav(h, t);
    if (w.length < 44 || Buffer.from(w).toString('ascii', 0, 4) !== 'RIFF')
      throw new Error(`demo track ${t} wav`);
  }
  W.flq_close(h);
  console.log('demo: 2 tracks decode+wav ok'); pass++;
}

// 5. stems bundle: 8 tracks incl. one f32, decode a second of each
{
  const h = open(join(root, 'web/samples/stems.flq'));
  const j = info(h);
  if (j.tracks.length !== 8) throw new Error(`stems tracks: ${j.tracks.length}`);
  if (!j.tracks.some((t) => t.sample_fmt === 1)) throw new Error('stems: no f32 track');
  for (let t = 0; t < j.tracks.length; t++) {
    const g = decF32(h, t, 0, j.tracks[t].rate);
    if (g.length !== j.tracks[t].rate * j.tracks[t].channels)
      throw new Error(`stems track ${t} chunk len`);
  }
  W.flq_close(h);
  console.log('stems: 8 tracks (incl. f32) decode ok'); pass++;
}

console.log(`smoke: ${pass}/4 checks passed`);
