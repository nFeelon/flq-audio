/* flq web player — vanilla JS + the raw extern-C wasm decoder. */
'use strict';

const CHUNK_SEC = 2;      // decode chunk length per track
const AHEAD_SEC = 4;      // schedule this far ahead of the play head

const $ = (id) => document.getElementById(id);
const errBox = $('err');
let wasm = null;          // instance.exports
let mem = null;           // Uint8Array view of wasm memory (re-fetch after calls)

function refreshMem() { mem = new Uint8Array(wasm.memory.buffer); }
function outBytes() { refreshMem(); return mem.slice(wasm.flq_out_ptr(), wasm.flq_out_ptr() + wasm.flq_out_len()); }
function errText() {
  refreshMem();
  const n = wasm.flq_err_len();
  return n ? new TextDecoder().decode(mem.slice(wasm.flq_err_ptr(), wasm.flq_err_ptr() + n)) : '';
}
function fail(prefix) {
  const m = `${prefix}: ${errText() || 'unknown error'}`;
  errBox.textContent = m; errBox.hidden = false;
  throw new Error(m);
}
const fmtTime = (s) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
const fmtBytes = (n) => n > 1e6 ? (n / 1e6).toFixed(2) + ' MB' : n > 1e3 ? (n / 1e3).toFixed(1) + ' KB' : n + ' B';

// ------------------------------------------------------------------ playback

const P = {
  h: 0, info: null, ctx: null, gains: [], sources: new Set(),
  playing: false, posSec: 0, startAt: 0, timer: 0,
};

function stopScheduled() {
  for (const s of P.sources) { try { s.stop(); } catch (_) {} s.disconnect(); }
  P.sources.clear();
  if (P.timer) { clearInterval(P.timer); P.timer = 0; }
}

function trackDur(t) { return t.frames / t.rate; }
function fileDur() { return Math.max(...P.info.tracks.map(trackDur), 0); }

/* Decode `dur` seconds of `track` at `posSec` into interleaved f32. */
function decodeChunk(ti, posSec, dur) {
  const t = P.info.tracks[ti];
  const start = Math.min(Math.floor(posSec * t.rate), t.frames);
  const len = Math.min(Math.ceil(dur * t.rate), t.frames - start);
  if (len <= 0) return null;
  if (!wasm.flq_decode_f32(P.h, ti, start, len)) fail(`decode ${t.name}`);
  const f32 = new Float32Array(outBytes().buffer.slice(0));
  const buf = P.ctx.createBuffer(t.channels, len, t.rate);
  for (let c = 0; c < t.channels; c++) {
    const cd = buf.getChannelData(c);
    for (let i = 0; i < len; i++) cd[i] = f32[i * t.channels + c];
  }
  return buf;
}

/* Keep ~AHEAD_SEC of audio scheduled, chunk by chunk, across all tracks. */
function pump() {
  if (!P.playing) return;
  const horizon = P.posSec + (P.ctx.currentTime - P.startAt) + AHEAD_SEC;
  while (P.schedSec < horizon) {
    let any = false;
    for (let ti = 0; ti < P.info.tracks.length; ti++) {
      const buf = decodeChunk(ti, P.schedSec, CHUNK_SEC);
      if (!buf) continue;
      const src = P.ctx.createBufferSource();
      src.buffer = buf;
      src.connect(P.gains[ti]);
      const when = P.startAt + (P.schedSec - P.posSec);
      src.start(Math.max(when, P.ctx.currentTime + 0.01));
      src.onended = () => P.sources.delete(src);
      P.sources.add(src);
      any = true;
    }
    if (!any) break;
    P.schedSec += CHUNK_SEC;
  }
  // progress + end detection
  const t = P.posSec + (P.ctx.currentTime - P.startAt);
  const dur = fileDur();
  $('seek').value = Math.round(1000 * Math.min(t / dur, 1));
  $('time').textContent = `${fmtTime(t)} / ${fmtTime(dur)}`;
  if (t >= dur && P.sources.size === 0) setPlaying(false);
}

function setPlaying(on) {
  if (!P.h) return;
  if (on) {
    if (!P.ctx) P.ctx = new AudioContext();
    if (P.ctx.state === 'suspended') P.ctx.resume();
    if (P.posSec >= fileDur()) P.posSec = 0;
    stopScheduled();
    P.playing = true;
    P.schedSec = P.posSec;
    P.startAt = P.ctx.currentTime + 0.05;
    P.timer = setInterval(pump, 250);
    pump();
    $('play').textContent = 'pause';
  } else {
    P.posSec += P.ctx ? P.ctx.currentTime - P.startAt : 0;
    P.playing = false;
    stopScheduled();
    if (P.ctx) P.ctx.suspend();
    $('play').textContent = 'play';
  }
}

function seekTo(frac) {
  const was = P.playing;
  if (was) setPlaying(false);
  P.posSec = frac * fileDur();
  $('time').textContent = `${fmtTime(P.posSec)} / ${fmtTime(fileDur())}`;
  if (was) setPlaying(true);
}

// -------------------------------------------------------------------- load

function showTracks(fileBytes) {
  const { tracks } = P.info;
  const tb = document.querySelector('#tracks tbody');
  tb.innerHTML = '';
  let pcmTotal = 0;
  tracks.forEach((t, i) => {
    const pcm = t.frames * t.channels * (t.bits / 8);
    pcmTotal += pcm;
    const tr = document.createElement('tr');
    const fmt = `${t.sample_fmt ? 'f' : 'i'}${t.bits}`;
    tr.innerHTML =
      `<td>${i}</td><td></td><td>${fmt}</td><td>${t.rate}</td>` +
      `<td>${t.channels}</td><td>${fmtTime(trackDur(t))}</td><td>${fmtBytes(pcm)}</td>` +
      `<td><button class="wav">wav</button></td>` +
      `<td><input type="checkbox" class="mute"></td>` +
      `<td><input type="checkbox" class="solo"></td>`;
    tr.children[1].textContent = t.name;
    tr.querySelector('.wav').onclick = () => {
      if (!wasm.flq_wav(P.h, i)) return fail(`wav ${t.name}`);
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([outBytes()], { type: 'audio/wav' }));
      a.download = t.name.replace(/\.[^.]*$/, '') + '.wav';
      a.click();
    };
    tr.querySelector('.mute').onchange = (e) => applyMix();
    tr.querySelector('.solo').onchange = (e) => applyMix();
    tb.appendChild(tr);
  });
  $('meta').textContent = `${fmtBytes(fileBytes)} .flq — ${tracks.length} track${tracks.length > 1 ? 's' : ''}, frame_len ${P.info.frame_len}`;
  $('ratio').textContent = `pcm total ${fmtBytes(pcmTotal)} — archive is ${(100 * fileBytes / Math.max(pcmTotal, 1)).toFixed(1)}% of raw pcm`;
  $('player').hidden = false;
}

function applyMix() {
  const solos = [...document.querySelectorAll('.solo')].map((c) => c.checked);
  const anySolo = solos.some(Boolean);
  document.querySelectorAll('.mute').forEach((m, i) => {
    const g = P.gains[i];
    if (g) g.gain.value = m.checked || (anySolo && !solos[i]) ? 0 : 1;
  });
}

function openFile(bytes, label) {
  setPlaying(false);
  if (P.h) wasm.flq_close(P.h);
  P.h = 0; P.posSec = 0; P.schedSec = 0; P.sources = new Set();
  errBox.hidden = true;
  const p = wasm.flq_alloc(bytes.length);
  refreshMem();
  mem.set(bytes, p);
  P.h = wasm.flq_open(p, bytes.length);
  wasm.flq_free(p, bytes.length);
  if (!P.h) return fail(`open ${label}`);
  if (!wasm.flq_info_json(P.h)) return fail('info');
  P.info = JSON.parse(new TextDecoder().decode(outBytes()));
  if (!P.ctx) P.ctx = new AudioContext();
  P.gains = P.info.tracks.map(() => { const g = P.ctx.createGain(); g.connect(P.ctx.destination); return g; });
  showTracks(bytes.length);
  applyMix();
  $('time').textContent = `0:00 / ${fmtTime(fileDur())}`;
  $('seek').value = 0;
}

// --------------------------------------------------------------------- boot

async function boot() {
  const res = await WebAssembly.instantiateStreaming(fetch('flq_wasm.wasm'));
  wasm = res.instance.exports;
  refreshMem();
  $('file').onchange = async (e) => {
    const f = e.target.files[0];
    if (f) openFile(new Uint8Array(await f.arrayBuffer()), f.name);
  };
  $('sample').onclick = async () => {
    const r = await fetch('samples/demo.flq');
    if (!r.ok) return fail(`fetch demo (${r.status})`);
    openFile(new Uint8Array(await r.arrayBuffer()), 'demo.flq');
  };
  const dz = $('drop');
  dz.ondragover = (e) => { e.preventDefault(); dz.classList.add('over'); };
  dz.ondragleave = () => dz.classList.remove('over');
  dz.ondrop = async (e) => {
    e.preventDefault(); dz.classList.remove('over');
    const f = e.dataTransfer.files[0];
    if (f) openFile(new Uint8Array(await f.arrayBuffer()), f.name);
  };
  $('play').onclick = () => setPlaying(!P.playing);
  $('seek').oninput = (e) => seekTo(e.target.value / 1000);
}

boot().catch((e) => fail('wasm init (' + e.message + ')'));
