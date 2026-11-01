// Zones: named stretches of code, timed into a call tree. The JavaScript
// port of the Rust crate beside it, with the same model and the same two
// report formats, so a report from either diffs against the other.
//
// A zone is declared once, where the code it times lives; entering it
// returns its node, and `exit` of that node ends the call:
//
//   import { zone, enter, exit, read, take, overhead } from "zonetrace";
//   const LOAD = zone(1, "chunk.load");
//   function load(buf) {
//     const z = enter(LOAD);
//     // ... the work the zone times ...
//     read(buf.byteLength);
//     exit(z);
//   }
//
// One node per path of zones (a zone entered from two different zones is
// two nodes); a node sums its calls, its time, and the bytes the code in
// it said it read and copied. Nothing is recorded per call and nothing is
// allocated after a node's first visit, so a zone in a loop costs a few
// property reads and two clock reads. With `zones.on` false, `enter`
// returns null and `exit(null)` returns at once: a page can ship its zones
// and pay one branch each.
//
// The clock is `Bun.nanoseconds()` under Bun and `performance.now()` in a
// browser, where its resolution is 5 µs to 1 ms by browser and headers;
// there, read a zone's time as a mean over many calls. With `countHeap`
// given a function that returns the heap's size in bytes, each call also
// sums what the heap grew by while it ran, and a call the collector ran
// through, in which the heap shrank, is counted under `gc` and left out
// of that sum. In Chrome `performance.memory.usedJSHeapSize` moves with
// each allocation; in Bun `heapStats().heapCapacity` from "bun:jsc" moves
// a block (16 KB) at a time, so read it over a round of calls, not one.

const ROOT = { id: -1, children: [] };

export const zones = {
  on: false,
  // the heap's size in bytes, when a run counts what calls allocate
  heap: null,
  // what one read of it costs in bytes, taken off every call
  heapCost: 0,
  cur: ROOT,
  epoch: 0,
  nodes: [],
};

const clock = globalThis.Bun ? () => Bun.nanoseconds() : () => performance.now() * 1e6;

// A zone with a number unique among the zones one report shows, and a
// dotted name, e.g. `chunk.load`.
export const zone = (id, name) => ({ id, name });

// Opens `z` inside whatever zone is open: its node, or null when zones are off.
export function enter(z) {
  if (!zones.on) return null;
  const at = zones.cur;
  const kids = at.children;
  let n = null;
  for (let i = 0; i < kids.length; i++) {
    if (kids[i].zone === z) {
      n = kids[i];
      break;
    }
  }
  if (n === null) n = make(at, z);
  zones.cur = n;
  n.epoch = zones.epoch;
  if (zones.heap !== null) n.h0 = zones.heap();
  // the clock is read after the bookkeeping on entry and before it on
  // exit, so the zone's own time leaves the tracer out; its parent's does
  // not, and the report takes that out
  n.t0 = clock();
  return n;
}

// Ends the call of `n`, the node `enter` returned.
export function exit(n) {
  if (n === null) return;
  const t = clock() - n.t0;
  if (n.epoch !== zones.epoch) return;
  n.calls++;
  n.nanos += t;
  if (zones.heap !== null) {
    const d = zones.heap() - n.h0 - zones.heapCost;
    if (d < 0) n.gc++;
    else n.bytes += d;
  }
  zones.cur = n.parent;
}

function make(parent, z) {
  const n = {
    id: zones.nodes.length,
    parent,
    zone: z,
    children: [],
    calls: 0,
    nanos: 0,
    read: 0,
    copied: 0,
    bytes: 0,
    gc: 0,
    epoch: 0,
    t0: 0,
    h0: 0,
  };
  zones.nodes.push(n);
  parent.children.push(n);
  return n;
}

// `n` bytes read from the source (a request, a file) in the open zone.
export function read(n) {
  if (zones.cur !== ROOT) zones.cur.read += n;
}

// `n` bytes copied from one buffer to another in the open zone.
export function copied(n) {
  if (zones.cur !== ROOT) zones.cur.copied += n;
}

const CALIBRATION = zone(65535, "zones.calibration");

// What one zone costs the zone around it, in nanoseconds: an empty zone
// entered and left `k` times, the median of five runs, on a tree of its
// own so the report's tree is untouched. With a heap probe set, its two
// reads are in the cost, as they are in every zone.
export function overhead(k = 100000) {
  const saved = [zones.on, zones.cur, zones.heap, zones.nodes];
  zones.on = true;
  const runs = [];
  for (let r = 0; r < 5; r++) {
    zones.cur = { id: -1, children: [] };
    zones.nodes = [];
    const t0 = clock();
    for (let i = 0; i < k; i++) exit(enter(CALIBRATION));
    runs.push((clock() - t0) / k);
  }
  [zones.on, zones.cur, zones.heap, zones.nodes] = saved;
  runs.sort((a, b) => a - b);
  return runs[2];
}

// Sets `zones.heap` to a function that returns the heap's size in bytes
// and measures what one read of it costs.
export function countHeap(heap) {
  zones.heap = heap;
  const a = heap();
  const b = heap();
  zones.heapCost = Math.max(0, b - a);
}

// The tree as a report; a new tree starts. A zone open while it is taken
// is counted in neither.
export function take() {
  const rows = zones.nodes.map((n) => ({
    id: n.id,
    parent: n.parent === ROOT ? null : n.parent.id,
    zone: n.zone.id,
    name: n.zone.name,
    calls: n.calls,
    nanos: Math.round(n.nanos),
    read: n.read,
    copied: n.copied,
    heap: zones.heap === null ? null : { bytes: n.bytes, gc: n.gc },
    children: n.children.map((c) => c.id),
  }));
  const roots = ROOT.children.map((c) => c.id);
  ROOT.children = [];
  zones.nodes = [];
  zones.cur = ROOT;
  zones.epoch++;
  return new Report(rows, roots);
}

// A call tree taken by `take`; reports of several runs `merge` into one.
export class Report {
  constructor(rows = [], roots = []) {
    this.rows = rows;
    this.roots = roots;
  }

  // Adds `other` in: rows on the same path of zone numbers are summed, the
  // others are added.
  merge(other) {
    for (const r of other.roots) this.#mergeRow(other, r, null);
  }

  #mergeRow(other, at, parent) {
    const o = other.rows[at];
    const siblings = parent === null ? this.roots : this.rows[parent].children;
    let id = siblings.find((c) => this.rows[c].zone === o.zone);
    if (id === undefined) {
      id = this.rows.length;
      this.rows.push({ ...o, id, parent, heap: o.heap && { ...o.heap }, children: [] });
      siblings.push(id);
    } else {
      const r = this.rows[id];
      r.calls += o.calls;
      r.nanos += o.nanos;
      r.read += o.read;
      r.copied += o.copied;
      if (o.heap) {
        r.heap ??= { bytes: 0, gc: 0 };
        r.heap.bytes += o.heap.bytes;
        r.heap.gc += o.heap.gc;
      }
    }
    for (const c of o.children) this.#mergeRow(other, c, id);
  }

  // Each row's time with the tracer taken out, and the part of it not
  // spent in its children, in nanoseconds, by row number.
  netTimes(overhead) {
    const beneath = new Float64Array(this.rows.length);
    for (let i = this.rows.length - 1; i >= 0; i--) {
      const r = this.rows[i];
      if (r.parent !== null) beneath[r.parent] += r.calls + beneath[r.id];
    }
    const net = this.rows.map((r, i) => Math.max(0, r.nanos - beneath[i] * overhead));
    return this.rows.map((r, i) => {
      let inner = 0;
      for (const c of r.children) inner += net[c];
      return [net[i], Math.max(0, net[i] - inner)];
    });
  }

  // The tree as text: one line per row, indented under its parent, with
  // calls, net time and its share of the top rows, net self time, MB read
  // and copied, and copies per byte read; when a row counted the heap, the
  // MB its calls grew the heap by and the calls the collector ran in.
  text(overhead) {
    const net = this.netTimes(overhead);
    let whole = 0;
    for (const r of this.roots) whole += net[r][0];
    const counted = this.rows.some((r) => r.heap !== null);
    const col = (s, w) => String(s).padStart(w);
    let out =
      "zone".padEnd(40) +
      ` ${col("calls", 10)} ${col("total ms", 11)} ${col("%", 6)} ${col("self ms", 11)} ${col("read MB", 10)} ${col("copy MB", 10)} ${col("copy/r", 6)}`;
    if (counted) out += ` ${col("alloc MB", 10)} ${col("gc", 6)}`;
    out += "\n";
    const stack = this.roots.map((r) => [r, 0]).reverse();
    while (stack.length) {
      const [id, depth] = stack.pop();
      const r = this.rows[id];
      const [total, own] = net[id];
      const ratio = r.read === 0 ? "-" : (r.copied / r.read).toFixed(2);
      const share = whole > 0 ? (total / whole) * 100 : 0;
      out +=
        (" ".repeat(depth * 2) + r.name).padEnd(40) +
        ` ${col(r.calls, 10)} ${col((total / 1e6).toFixed(3), 11)} ${col(share.toFixed(1), 6)} ${col((own / 1e6).toFixed(3), 11)} ${col((r.read / 1e6).toFixed(1), 10)} ${col((r.copied / 1e6).toFixed(1), 10)} ${col(ratio, 6)}`;
      if (counted)
        out += r.heap ? ` ${col((r.heap.bytes / 1e6).toFixed(1), 10)} ${col(r.heap.gc, 6)}` : ` ${col("-", 10)} ${col("-", 6)}`;
      out += "\n";
      for (let i = r.children.length - 1; i >= 0; i--) stack.push([r.children[i], depth + 1]);
    }
    return out;
  }

  // The tree as NDJSON, one row a line, for diffing runs: the Rust
  // report's fields in its order, then `heap` when the row counted the heap.
  ndjson(overhead) {
    const net = this.netTimes(overhead);
    let out = "";
    this.rows.forEach((r, i) => {
      const [total, own] = net[i];
      out += `{"id":${r.id},"parent":${r.parent === null ? "null" : r.parent},"zone":${r.zone},"name":${JSON.stringify(r.name)},"calls":${r.calls},"nanos":${r.nanos},"net":${total.toFixed(0)},"self":${own.toFixed(0)},"read":${r.read},"copied":${r.copied}`;
      if (r.heap) out += `,"heap":{"bytes":${r.heap.bytes},"gc":${r.heap.gc}}`;
      out += "}\n";
    });
    return out;
  }
}
