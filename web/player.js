/* flq web player — vanilla JS + the raw extern-C wasm decoder. */
'use strict';

const CHUNK_SEC = 2;       // playback decode chunk per track
const AHEAD_SEC = 4;       // schedule this far ahead of the play head
const SCAN_SEC = 1;        // waveform scan slice length
const PEAK_COLS = 1200;    // max waveform columns kept per track
const ORIGIN = ['WAV', 'AIFF', 'FLAC', 'ALAC'];
const ICO_VOL = '<svg class="ic" viewBox="0 0 24 24" aria-hidden="true"><path class="solid" d="M11 5 6 9H2v6h4l5 4V5Z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/></svg>';
const PALETTE = ['#86efac', '#93c5fd', '#fdba74', '#d8b4fe', '#67e8f9', '#f9a8d4', '#fde68a', '#bef264'];

const $ = (id) => document.getElementById(id);
let wasm = null;
let mem = null;

function refreshMem() { mem = new Uint8Array(wasm.memory.buffer); }
function outBytes() { refreshMem(); return mem.slice(wasm.flq_out_ptr(), wasm.flq_out_ptr() + wasm.flq_out_len()); }
function errText() {
  refreshMem();
  const n = wasm.flq_err_len();
  return n ? new TextDecoder().decode(mem.slice(wasm.flq_err_ptr(), wasm.flq_err_ptr() + n)) : '';
}
function fail(prefix) {
  const m = `${prefix}: ${errText() || 'unknown error'}`;
  const e = $('err');
  e.textContent = m; e.hidden = false;
  throw new Error(m);
}
const clamp = (v, a, b) => Math.min(Math.max(v, a), b);
const fmtTime = (s) => {
  s = Math.max(0, s | 0);
  const h = s / 3600 | 0, m = (s % 3600) / 60 | 0, r = s % 60;
  return (h ? h + ':' + String(m).padStart(2, '0') : m) + ':' + String(r).padStart(2, '0');
};
const fmtBytes = (n) => n > 1e6 ? (n / 1e6).toFixed(2) + ' MB' : n > 1e3 ? (n / 1e3).toFixed(1) + ' KB' : n + ' B';
const toDb = (v) => v > 1e-7 ? 20 * Math.log10(v) : -Infinity;
const fmtDb = (db) => db === -Infinity ? '−∞ dB' : db.toFixed(1) + ' dB';

// ------------------------------------------------------------------- state

const P = {
  h: 0, info: null, fileName: '', fileBytes: 0,
  ctx: null, master: null,
  ch: [],               // per track {gain, ana, vol, enabled, solo, level, peak}
  sources: new Set(),
  playing: false, loop: false, posSec: 0, schedSec: 0, startAt: 0, timer: 0,
  focus: 0, scanGen: 0, scanning: false,
  decSec: 0, decWall: 0, // decode-speed accumulators
  peaks: [],            // per track {a: Float32Array, filled: int, cols: int}
};

function trackDur(t) { return t.frames / t.rate; }
function fileDur() { return P.info ? Math.max(...P.info.tracks.map(trackDur), 0) : 0; }
function color(i) { return PALETTE[i % PALETTE.length]; }
function pcmBytes(t) { return t.frames * t.channels * (t.bits / 8); }
function fmtSpec(t) {
  return `PCM · ${(t.rate / 1000).toFixed(1).replace(/\.0$/, '')} kHz · ${t.sample_fmt ? 'f' : 'i'}${t.bits} · ` +
    ({ 1: 'Mono', 2: 'Stereo' }[t.channels] || t.channels + ' ch');
}

// ---------------------------------------------------------------- playback

function stopScheduled() {
  for (const s of P.sources) { try { s.stop(); } catch (_) {} s.disconnect(); }
  P.sources.clear();
  if (P.timer) { clearInterval(P.timer); P.timer = 0; }
}

/* Decode `dur` seconds of track `ti` at `posSec` → AudioBuffer, and record
 * the chunk's peak level + decode wall time. */
function decodeChunk(ti, posSec, dur) {
  const t = P.info.tracks[ti];
  const start = Math.min(Math.floor(posSec * t.rate), t.frames);
  const len = Math.min(Math.ceil(dur * t.rate), t.frames - start);
  if (len <= 0) return null;
  const t0 = performance.now();
  if (!wasm.flq_decode_f32(P.h, ti, start, len)) fail(`decode ${t.name}`);
  const wall = performance.now() - t0;
  const f32 = new Float32Array(outBytes().buffer); // outBytes() already copies
  P.decSec += len / t.rate; P.decWall += wall / 1000;
  let pk = 0;
  for (let i = 0; i < f32.length; i++) { const a = Math.abs(f32[i]); if (a > pk) pk = a; }
  P.ch[ti].peak = Math.max(P.ch[ti].peak * 0.7, pk);
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
  const now = P.posSec + (P.ctx.currentTime - P.startAt);
  const horizon = now + AHEAD_SEC;
  while (P.schedSec < horizon) {
    let any = false;
    for (let ti = 0; ti < P.info.tracks.length; ti++) {
      const buf = decodeChunk(ti, P.schedSec, CHUNK_SEC);
      if (!buf) continue;
      const src = P.ctx.createBufferSource();
      src.buffer = buf;
      src.connect(P.ch[ti].gain);
      const when = P.startAt + (P.schedSec - P.posSec);
      src.start(Math.max(when, P.ctx.currentTime + 0.01));
      src.onended = () => P.sources.delete(src);
      P.sources.add(src);
      any = true;
    }
    if (!any) break;
    P.schedSec += CHUNK_SEC;
  }
  if (now >= fileDur() && P.sources.size === 0) {
    if (P.loop) { P.posSec = 0; setPlaying(true); }
    else setPlaying(false);
  }
}

function setPlaying(on) {
  if (!P.h || !P.ctx) return;
  if (on) {
    if (P.ctx.state === 'suspended') P.ctx.resume();
    if (P.posSec >= fileDur()) P.posSec = 0;
    stopScheduled();
    P.playing = true;
    P.schedSec = P.posSec;
    P.startAt = P.ctx.currentTime + 0.05;
    P.timer = setInterval(pump, 250);
    pump();
    setPlayIcon(true);
  } else {
    P.posSec += P.ctx ? P.ctx.currentTime - P.startAt : 0;
    P.posSec = Math.min(P.posSec, fileDur());
    P.playing = false;
    stopScheduled();
    if (P.ctx) P.ctx.suspend();
    setPlayIcon(false);
  }
  statusLeft();
}

function setPlayIcon(playing) {
  $('play').querySelector('.i-play').hidden = playing;
  $('play').querySelector('.i-pause').hidden = !playing;
}

function playPos() {
  return P.playing ? P.posSec + (P.ctx.currentTime - P.startAt) : P.posSec;
}

function seekTo(frac) {
  const was = P.playing;
  if (was) setPlaying(false);
  P.posSec = clamp(frac, 0, 1) * fileDur();
  if (was) setPlaying(true);
  else { drawTransport(); drawWave(); }
}

function skip(ds) { seekTo((playPos() + ds) / Math.max(fileDur(), 1e-9)); }

// ------------------------------------------------------------------- mix

function applyMix() {
  const anySolo = P.ch.some((c) => c.solo);
  P.ch.forEach((c) => {
    const audible = c.enabled && (!anySolo || c.solo);
    if (c.gain) c.gain.gain.value = audible ? c.vol : 0;
  });
}

// ------------------------------------------------------------- load file

function openFile(bytes, label) {
  setPlaying(false);
  P.scanGen++; P.scanning = false;
  for (const c of P.ch) c.gain.disconnect();
  P.ch = [];
  if (P.h) wasm.flq_close(P.h);
  Object.assign(P, {
    h: 0, posSec: 0, schedSec: 0, decSec: 0, decWall: 0,
    sources: new Set(), fileName: label, fileBytes: bytes.length, focus: 0,
  });
  $('err').hidden = true;
  const p = wasm.flq_alloc(bytes.length);
  refreshMem();
  mem.set(bytes, p);
  P.h = wasm.flq_open(p, bytes.length);
  wasm.flq_free(p, bytes.length);
  if (!P.h) return fail(`open ${label}`);
  if (!wasm.flq_info_json(P.h)) return fail('info');
  P.info = JSON.parse(new TextDecoder().decode(outBytes()));
  if (!P.ctx) {
    P.ctx = new AudioContext();
    P.master = P.ctx.createGain();
    P.master.connect(P.ctx.destination);
  }
  P.ch = P.info.tracks.map((t, i) => {
    const gain = P.ctx.createGain();
    const ana = P.ctx.createAnalyser();
    ana.fftSize = 2048;
    gain.connect(ana).connect(P.master);
    return { gain, ana, vol: 1, enabled: true, solo: false, peak: 0 };
  });
  P.master.gain.value = $('volume').value / 100;
  P.peaks = P.info.tracks.map((t) => ({
    a: new Float32Array(Math.min(PEAK_COLS, Math.max(200, t.frames / (t.rate * 0.1) | 0))),
    filled: 0,
  }));
  renderAll();
  applyMix();
  startScan().catch(() => {}); // fail() already surfaced the message
}

/* Background waveform scan: decode each track in SCAN_SEC slices, fold to
 * peak columns, draw progressively. Token-cancellable on reload. */
async function startScan() {
  const gen = P.scanGen;
  P.scanning = true;
  statusLeft();
  for (let ti = 0; ti < P.info.tracks.length; ti++) {
    const t = P.info.tracks[ti], pk = P.peaks[ti];
    const cols = pk.a.length, per = t.frames / cols;
    for (let s = 0; s < t.frames;) {
      if (gen !== P.scanGen) return;
      const len = Math.min(Math.ceil(SCAN_SEC * t.rate), t.frames - s);
      if (len <= 0) break;
      const t0 = performance.now();
      if (!wasm.flq_decode_f32(P.h, ti, s, len)) { fail(`scan ${t.name}`); return; }
      P.decSec += len / t.rate; P.decWall += (performance.now() - t0) / 1000;
      const f32 = new Float32Array(outBytes().buffer);
      const c0 = Math.floor(s / per), c1 = Math.min(cols, Math.ceil((s + len) / per));
      for (let c = c0; c < c1; c++) {
        const i0 = Math.max(0, Math.floor(c * per) - s), i1 = Math.min(len, Math.ceil((c + 1) * per) - s);
        let m = pk.a[c];
        for (let i = i0; i < i1; i++) {
          for (let ch = 0; ch < t.channels; ch++) {
            const a = Math.abs(f32[i * t.channels + ch]);
            if (a > m) m = a;
          }
        }
        pk.a[c] = m;
      }
      pk.filled = c1;
      s += len;
      drawLayerWave(ti);
      if (ti === P.focus) drawWave();
      statusLeft();
      await new Promise((r) => setTimeout(r, 0)); // let playback/timers interleave
    }
  }
  if (gen === P.scanGen) { P.scanning = false; statusLeft(); }
}

// ---------------------------------------------------------------- render

function renderAll() {
  const d = fileDur();
  $('empty').hidden = true;
  $('now').hidden = false;
  $('now-name').textContent = P.fileName;
  $('now-sub').textContent = `${P.info.tracks.length} track${P.info.tracks.length > 1 ? 's' : ''} · ${fmtTime(d)} · ${headerFmt()}`;
  $('t-dur').textContent = fmtTime(d);
  for (const id of ['transport', 'chips', 'tabs']) $(id).hidden = false;
  drawThumb();
  drawTransport();
  renderLayers();
  renderChips();
  renderInfo();
  updateFocus();
  statusLeft();
}

function headerFmt() {
  const ts = P.info.tracks;
  const same = (f) => ts.every((t) => f(t) === f(ts[0]));
  const rate = same((t) => t.rate) ? (ts[0].rate / 1000).toFixed(1).replace(/\.0$/, '') + ' kHz' : 'mixed rate';
  const bits = same((t) => t.bits + ':' + t.sample_fmt)
    ? (ts[0].sample_fmt ? 'f' : 'i') + ts[0].bits : 'mixed fmt';
  return `${rate} · ${bits}`;
}

function drawThumb() {
  const cv = $('thumb'), c = cv.getContext('2d'), s = cv.width;
  let h = 0;
  for (const ch of P.fileName) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  const g = c.createLinearGradient(0, 0, s, s);
  g.addColorStop(0, `hsl(${h % 360} 45% 62%)`);
  g.addColorStop(1, `hsl(${(h / 360 | 0) % 360} 45% 42%)`);
  c.fillStyle = g; c.fillRect(0, 0, s, s);
  c.fillStyle = '#ffffff22';
  for (let i = 0; i < 14; i++) {
    const x = 5 + i * 3.3, hh = 6 + ((h >> (i % 16)) & 31);
    c.fillRect(x, s / 2 - hh / 2, 2, hh);
  }
}

function fitCanvas(cv) {
  const dpr = devicePixelRatio || 1, w = cv.clientWidth, h = cv.clientHeight;
  if (!w || !h) return null;
  if (cv.width !== (w * dpr | 0) || cv.height !== (h * dpr | 0)) {
    cv.width = w * dpr | 0; cv.height = h * dpr | 0;
  }
  const ctx = cv.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  return { ctx, w, h };
}

function drawPeaks(cv, pk, col, opts = {}) {
  const f = fitCanvas(cv);
  if (!f) return;
  const { ctx, w, h } = f, mid = h / 2;
  ctx.clearRect(0, 0, w, h);
  const n = pk.a.length, step = w / n;
  ctx.fillStyle = col;
  for (let i = 0; i < pk.filled; i++) {
    const y = pk.a[i] * mid * 0.92;
    ctx.fillRect(i * step, mid - y, Math.max(1, step - 0.5), Math.max(1, y * 2));
  }
  if (pk.filled < n) { // unscanned tail
    ctx.fillStyle = '#3a4a66';
    ctx.fillRect(pk.filled * step, mid - 1, w - pk.filled * step, 2);
  }
  if (opts.playhead != null) {
    ctx.fillStyle = '#ffffffcc';
    ctx.fillRect(clamp(opts.playhead, 0, 1) * w - 0.5, 0, 1.5, h);
  }
}

function renderLayers() {
  const box = $('layers');
  box.innerHTML = '';
  P.info.tracks.forEach((t, i) => {
    const el = document.createElement('div');
    el.className = 'layer';
    el.innerHTML =
      `<input type="checkbox" class="en" checked title="enable in mix">` +
      `<span class="dot" style="background:${color(i)}" aria-hidden="true"></span>` +
      `<span class="lname"></span>` +
      `<span class="lv dim">${ICO_VOL}<input type="range" class="vol" min="0" max="100" value="100"></span>` +
      `<span class="sz"></span>` +
      `<span class="act"><button class="mini solo" title="solo" aria-pressed="false">S</button>` +
      `<button class="mini wav" title="download WAV">wav</button></span>` +
      `<canvas class="lw" height="44" role="img"></canvas>` +
      `<span class="spec" style="grid-column:1/4">${fmtSpec(t)} · ${fmtTime(trackDur(t))}</span>` +
      `<span class="sz" style="grid-column:4/7"></span>`;
    el.querySelector('.en').setAttribute('aria-label', `include ${t.name} in the mix`);
    el.querySelector('.vol').setAttribute('aria-label', `${t.name} volume`);
    el.querySelector('.solo').setAttribute('aria-label', `solo ${t.name}`);
    el.querySelector('.wav').setAttribute('aria-label', `download ${t.name} as WAV`);
    el.querySelector('.lw').setAttribute('aria-label', `waveform of ${t.name}`);
    const nm = document.createElement('span');
    nm.textContent = t.name;
    el.querySelector('.lname').append(nm, chip(`Track ${i + 1}`), chip(ORIGIN[t.origin] || '?'));
    const pcm = pcmBytes(t);
    el.querySelectorAll('.sz')[1].textContent = `${fmtBytes(t.coded)} · ${(100 * t.coded / pcm).toFixed(1)}%`;
    el.querySelectorAll('.sz')[0].textContent = '';
    el.querySelector('.en').onchange = (e) => { P.ch[i].enabled = e.target.checked; applyMix(); };
    el.querySelector('.vol').oninput = (e) => { P.ch[i].vol = e.target.value / 100; applyMix(); };
    const solo = el.querySelector('.solo');
    solo.onclick = () => {
      P.ch[i].solo = !P.ch[i].solo;
      solo.classList.toggle('on', P.ch[i].solo);
      solo.setAttribute('aria-pressed', P.ch[i].solo);
      applyMix();
    };
    el.querySelector('.wav').onclick = () => {
      if (!wasm.flq_wav(P.h, i)) return fail(`wav ${t.name}`);
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([outBytes()], { type: 'audio/wav' }));
      a.download = t.name.replace(/\.[^.]*$/, '') + '.wav';
      a.click();
    };
    el.onclick = (e) => { if (!e.target.closest('button,input')) setFocus(i); };
    el.tabIndex = 0;
    el.onkeydown = (e) => {
      if (!e.target.closest('button,input') && (e.key === 'Enter' || e.key === ' ')) {
        e.preventDefault(); setFocus(i);
      }
    };
    box.appendChild(el);
  });
}

function chip(txt) {
  const s = document.createElement('span');
  s.className = 'chip'; s.textContent = txt;
  return s;
}

function drawLayerWave(i) {
  const cv = document.querySelectorAll('#layers .layer canvas.lw')[i];
  if (cv) drawPeaks(cv, P.peaks[i], color(i));
}
function drawAllLayers() { P.peaks.forEach((_, i) => drawLayerWave(i)); }

function renderChips() {
  const pcmTotal = P.info.tracks.reduce((s, t) => s + pcmBytes(t), 0);
  const ratio = (100 * P.fileBytes / Math.max(pcmTotal, 1)).toFixed(1);
  const coded = P.info.tracks.reduce((s, t) => s + t.coded, 0);
  $('chips').innerHTML =
    `<span class="chip"><b>${fmtBytes(P.fileBytes)}</b> .flq</span>` +
    `<span class="chip">vs PCM <b>${ratio}%</b></span>` +
    `<span class="chip">frames <b>${fmtBytes(coded)}</b> · overhead ${fmtBytes(P.fileBytes - coded)}</span>` +
    `<span class="chip">frame_len <b>${P.info.frame_len}</b></span>` +
    `<span class="chip">${P.info.has_index ? 'index ✓' : 'no index'}</span>` +
    `<span class="chip">decode <b id="ch-dec">—</b></span>` +
    `<span class="chip" id="ch-buf" hidden>buf <b id="ch-buf-v">—</b></span>`;
}

function renderInfo() {
  const g = $('info-grid');
  g.innerHTML = '';
  const file = iblock('file', [
    ['name', P.fileName], ['size', fmtBytes(P.fileBytes)],
    ['version', `FLQ v${P.info.version}`], ['frame_len', P.info.frame_len + ' samples'],
    ['index', P.info.has_index ? 'present' : 'absent (sequential scan)'],
    ['tracks', P.info.tracks.length], ['duration', fmtTime(fileDur())],
  ]);
  g.appendChild(file);
  P.info.tracks.forEach((t, i) => {
    const pcm = pcmBytes(t);
    g.appendChild(iblock(`track ${i + 1} — ${t.name}`, [
      ['origin', ORIGIN[t.origin] || t.origin],
      ['format', `${t.sample_fmt ? 'float' : 'int'} ${t.bits}-bit`],
      ['rate', t.rate + ' Hz'],
      ['channels', `${t.channels} (mask 0x${t.channel_mask.toString(16)})`],
      ['frames', `${t.frames.toLocaleString()} (${t.nframes} × ≤${P.info.frame_len})`],
      ['duration', fmtTime(trackDur(t))],
      ['coded', `${fmtBytes(t.coded)} (${(100 * t.coded / pcm).toFixed(1)}% of raw pcm)`],
      ['pcm_crc', '0x' + t.pcm_crc.toString(16).padStart(8, '0')],
      ['tags', t.tags_len ? fmtBytes(t.tags_len) : 'none'],
    ]));
  });
}

function iblock(title, kvs) {
  const d = document.createElement('div');
  d.className = 'iblock';
  const h = document.createElement('h4');
  h.textContent = title;
  const tb = document.createElement('table');
  tb.className = 'kv';
  for (const [k, v] of kvs) {
    const tr = document.createElement('tr');
    tr.innerHTML = '<td></td><td></td>';
    tr.children[0].textContent = k;
    tr.children[1].textContent = v;
    tb.appendChild(tr);
  }
  d.append(h, tb);
  return d;
}

// ------------------------------------------------------------ focus/wave

function setFocus(i) {
  P.focus = i;
  updateFocus();
  drawWave();
}

function updateFocus() {
  document.querySelectorAll('#layers .layer').forEach((el, i) =>
    el.classList.toggle('focus', i === P.focus));
  const t = P.info ? P.info.tracks[P.focus] : null;
  $('wave-name').textContent = t ? t.name : '—';
}

function drawWave() {
  if (!P.info) return;
  const pk = P.peaks[P.focus];
  drawPeaks($('wave'), pk, color(P.focus), { playhead: playPos() / Math.max(fileDur(), 1e-9) });
}

function scanPct() {
  const tot = P.peaks.reduce((s, p) => s + p.a.length, 0);
  const fil = P.peaks.reduce((s, p) => s + p.filled, 0);
  return tot ? Math.round(100 * fil / tot) : 0;
}

// ----------------------------------------------------------- live visuals

function drawTransport() {
  const t = playPos(), d = fileDur();
  $('t-pos').textContent = fmtTime(t);
  $('t-dur').textContent = fmtTime(d);
  if (document.activeElement !== $('seek')) $('seek').value = Math.round(1000 * clamp(t / Math.max(d, 1e-9), 0, 1));
}

function statusLeft() {
  const parts = [];
  if (P.info) parts.push(`${P.info.tracks.length} track${P.info.tracks.length > 1 ? 's' : ''} · FLQ v${P.info.version}`);
  if (P.scanning) parts.push(`scanning ${scanPct()}%`);
  if (P.playing) parts.push('playing');
  $('sb-left').textContent = parts.length ? parts.join(' · ') : 'idle';
}

const scopeData = new Uint8Array(2048);
const level = { v: 0, d: -Infinity };

function raf() {
  requestAnimationFrame(raf);
  drawTransport();
  if (!P.info) return;
  const t = playPos();
  // live chips
  const dec = $('ch-dec');
  if (dec) dec.textContent = P.decWall > 0.001 ? '×' + (P.decSec / P.decWall).toFixed(1) : '—';
  const bf = $('ch-buf');
  if (bf) {
    bf.hidden = !P.playing;
    if (P.playing) $('ch-buf-v').textContent = Math.max(0, P.schedSec - t).toFixed(1) + ' s';
  }
  // peak level of focused track with release
  const pk = P.ch[P.focus] ? P.ch[P.focus].peak : 0;
  level.v = Math.max(pk, level.v * 0.92);
  level.d = toDb(level.v);
  $('rt-level').style.backgroundSize = `${clamp((level.d + 60) / 60, 0, 1) * 100}% 100%`;
  $('rt-db').textContent = fmtDb(level.d);
  // scope: focused track time-domain
  const cv = $('cl-scope'), f = fitCanvas(cv);
  if (f && P.ch[P.focus]) {
    const { ctx, w, h } = f;
    ctx.clearRect(0, 0, w, h);
    P.ch[P.focus].ana.getByteTimeDomainData(scopeData);
    ctx.strokeStyle = color(P.focus); ctx.lineWidth = 1.4;
    ctx.beginPath();
    const n = P.ch[P.focus].ana.fftSize;
    for (let i = 0; i < n; i++) {
      const x = i / (n - 1) * w, y = (scopeData[i] / 255) * h;
      i ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
    }
    ctx.stroke();
  }
  if ($('pane-wave').classList.contains('on') && (P.playing || P.scanning)) drawWave();
  $('sb-right').textContent = `seek ${fmtTime(t)}`;
}

// --------------------------------------------------------------------- boot

async function boot() {
  const res = await WebAssembly.instantiateStreaming(fetch('flq_wasm.wasm'));
  wasm = res.instance.exports;
  refreshMem();

  $('file').onchange = async (e) => {
    const f = e.target.files[0];
    if (f) openFile(new Uint8Array(await f.arrayBuffer()), f.name);
    e.target.value = '';
  };
  $('open').onclick = () => $('file').click();
  const loadSample = (path, name) => async () => {
    const r = await fetch(path);
    if (!r.ok) return fail(`fetch ${name} (${r.status})`);
    openFile(new Uint8Array(await r.arrayBuffer()), name);
  };
  $('sample').onclick = loadSample('samples/demo.flq', 'demo.flq');
  $('stems').onclick = loadSample('samples/stems.flq', 'stems.flq');
  $('empty-demo').onclick = loadSample('samples/demo.flq', 'demo.flq');
  $('empty-stems').onclick = loadSample('samples/stems.flq', 'stems.flq');
  $('empty-drop').onclick = () => $('file').click();
  $('empty-drop').onkeydown = (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); $('file').click(); } };

  // drag & drop anywhere
  let dragN = 0;
  addEventListener('dragenter', (e) => { e.preventDefault(); if (++dragN === 1) $('drop-overlay').hidden = false; });
  addEventListener('dragleave', (e) => { e.preventDefault(); if (--dragN === 0) $('drop-overlay').hidden = true; });
  addEventListener('dragover', (e) => e.preventDefault());
  addEventListener('drop', async (e) => {
    e.preventDefault();
    dragN = 0; $('drop-overlay').hidden = true;
    const f = e.dataTransfer.files[0];
    if (f) openFile(new Uint8Array(await f.arrayBuffer()), f.name);
  });

  // transport
  $('play').onclick = () => setPlaying(!P.playing);
  $('back').onclick = () => skip(-10);
  $('fwd').onclick = () => skip(10);
  $('loop').onclick = () => {
    P.loop = !P.loop;
    $('loop').classList.toggle('on', P.loop);
    $('loop').setAttribute('aria-pressed', P.loop);
  };
  $('volume').oninput = (e) => { if (P.master) P.master.gain.value = e.target.value / 100; };
  $('seek').oninput = (e) => seekTo(e.target.value / 1000);

  // tabs
  document.querySelectorAll('#tabs .tab').forEach((b) => {
    b.onclick = () => {
      document.querySelectorAll('#tabs .tab').forEach((x) => {
        x.classList.toggle('on', x === b);
        x.setAttribute('aria-selected', x === b);
      });
      document.querySelectorAll('.pane').forEach((p) => p.classList.remove('on'));
      $('pane-' + b.dataset.tab).classList.add('on');
      if (b.dataset.tab === 'wave') drawWave();
      if (b.dataset.tab === 'layers') drawAllLayers();
    };
  });

  // waveform seek (click + drag)
  const wave = $('wave');
  const wseek = (e) => {
    const r = wave.getBoundingClientRect();
    seekTo((e.clientX - r.left) / r.width);
  };
  wave.onmousedown = (e) => {
    e.preventDefault();
    wseek(e);
    const mv = (ev) => wseek(ev);
    const up = () => { removeEventListener('mousemove', mv); removeEventListener('mouseup', up); };
    addEventListener('mousemove', mv); addEventListener('mouseup', up);
  };

  addEventListener('keydown', (e) => {
    if (e.target.matches('input,textarea') || !P.h) return;
    if (e.code === 'Space') { e.preventDefault(); setPlaying(!P.playing); }
    else if (e.code === 'ArrowLeft') skip(-5);
    else if (e.code === 'ArrowRight') skip(5);
    else if (e.code === 'ArrowUp') { $('volume').value = clamp(+$('volume').value + 5, 0, 100); if (P.master) P.master.gain.value = $('volume').value / 100; }
    else if (e.code === 'ArrowDown') { $('volume').value = clamp(+$('volume').value - 5, 0, 100); if (P.master) P.master.gain.value = $('volume').value / 100; }
    else if (e.code === 'KeyL') $('loop').click();
  });

  addEventListener('resize', () => { drawAllLayers(); drawWave(); });
  requestAnimationFrame(raf);
}

boot().catch((e) => fail('wasm init (' + e.message + ')'));
