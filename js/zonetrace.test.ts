// The JavaScript port against its own promises and against the Rust
// crate's report formats: the fixture under fixtures/ is what the Rust
// test tests/format.rs writes for the same script of zones, with the time
// columns masked.

import { describe, expect, test } from "bun:test";
import { heapStats } from "bun:jsc";
import { readFileSync } from "node:fs";
import { Report, copied, countHeap, enter, exit, overhead, read, take, zone, zones } from "./zonetrace.js";

// Bun's heap grows a block at a time, so a call that allocates a lot
// shows it and a call that allocates nothing shows nothing.
const heap = () => heapStats().heapCapacity;

const OUTER = zone(1, "outer");
const INNER = zone(2, "inner");
const OTHER = zone(3, "other");

const spin = (micros: number) => {
  const t0 = Bun.nanoseconds();
  while (Bun.nanoseconds() - t0 < micros * 1000);
};

const fresh = () => {
  zones.on = true;
  zones.heap = null;
  take();
};

describe("zones", () => {
  test("off, enter returns null and exit of null is nothing", () => {
    zones.on = false;
    const z = enter(OUTER);
    expect(z).toBeNull();
    exit(z);
    read(10);
    expect(take().rows).toEqual([]);
  });

  test("nest by path and sum their calls", () => {
    fresh();
    for (let i = 0; i < 3; i++) {
      const o = enter(OUTER);
      const a = enter(INNER);
      spin(20);
      exit(a);
      exit(o);
    }
    const b = enter(INNER);
    exit(b);
    const r = take();
    expect(r.rows.map((x) => [x.name, x.parent, x.calls])).toEqual([
      ["outer", null, 3],
      ["inner", 0, 3],
      ["inner", null, 1],
    ]);
    expect(r.roots).toEqual([0, 2]);
    expect(r.rows[0].children).toEqual([1]);
    expect(r.rows[0].nanos).toBeGreaterThanOrEqual(r.rows[1].nanos);
    expect(r.rows[1].nanos).toBeGreaterThanOrEqual(60_000);
  });

  test("a second visit allocates no node", () => {
    fresh();
    exit(enter(OUTER));
    const n = zones.nodes.length;
    for (let i = 0; i < 100; i++) exit(enter(OUTER));
    expect(zones.nodes.length).toBe(n);
    expect(take().rows[0].calls).toBe(101);
  });

  test("read and copied land on the open zone and nowhere outside one", () => {
    fresh();
    read(5);
    copied(5);
    const o = enter(OUTER);
    read(1000);
    const a = enter(INNER);
    copied(300);
    exit(a);
    copied(200);
    exit(o);
    const r = take();
    expect([r.rows[0].read, r.rows[0].copied, r.rows[1].read, r.rows[1].copied]).toEqual([1000, 200, 0, 300]);
  });

  test("a zone open across take is counted in neither tree", () => {
    fresh();
    const o = enter(OUTER);
    const before = take();
    exit(o);
    expect(before.rows[0].calls).toBe(0);
    expect(take().rows).toEqual([]);
    expect(zones.cur.id).toBe(-1);
  });

  test("overhead is a few nanoseconds and leaves the tree alone", () => {
    fresh();
    exit(enter(OTHER));
    const ns = overhead(10_000);
    expect(ns).toBeGreaterThan(0);
    expect(ns).toBeLessThan(2000);
    expect(zones.on).toBe(true);
    const r = take();
    expect(r.rows.map((x) => x.name)).toEqual(["other"]);
  });

  test("net times take the tracer out beneath a row", () => {
    const r = new Report(
      [
        { id: 0, parent: null, zone: 1, name: "a", calls: 1, nanos: 1000, read: 0, copied: 0, heap: null, children: [1] },
        { id: 1, parent: 0, zone: 2, name: "b", calls: 100, nanos: 600, read: 0, copied: 0, heap: null, children: [] },
      ],
      [0],
    );
    expect(r.netTimes(4)).toEqual([
      [600, 0],
      [600, 600],
    ]);
  });

  test("merge sums rows on the same path and adds the rest", () => {
    fresh();
    const o = enter(OUTER);
    read(10);
    exit(o);
    const a = take();
    const o2 = enter(OUTER);
    exit(enter(INNER));
    exit(o2);
    exit(enter(OTHER));
    const b = take();
    a.merge(b);
    expect(a.rows.map((x) => [x.name, x.parent, x.calls, x.read])).toEqual([
      ["outer", null, 2, 10],
      ["inner", 0, 1, 0],
      ["other", null, 1, 0],
    ]);
    expect(a.roots).toEqual([0, 2]);
  });
});

describe("heap", () => {
  test("a call's allocations are counted and a collection is set aside", () => {
    fresh();
    countHeap(heap);
    Bun.gc(true);
    const keep: unknown[] = [];
    const z = enter(OUTER);
    for (let i = 0; i < 100_000; i++) keep.push({ i });
    exit(z);
    const g = enter(INNER);
    keep.length = 0;
    Bun.gc(true);
    exit(g);
    const r = take();
    expect(r.rows[0].heap!.bytes).toBeGreaterThanOrEqual(1_000_000);
    expect(r.rows[0].heap!.gc).toBe(0);
    expect(r.rows[1].heap!.gc).toBe(1);
    expect(r.rows[1].heap!.bytes).toBe(0);
    zones.heap = null;
  });

  test("with a probe that allocates nothing, a zone that allocates nothing shows zero", () => {
    // Bun's own heap statistics allocate on every read, so this is a
    // counter the test moves: the arithmetic, the shrink as a collection
    // and the probe's cost taken off each call
    fresh();
    let size = 1000;
    countHeap(() => (size += 8));
    expect(zones.heapCost).toBe(8);
    const a = enter(OUTER);
    exit(a);
    const b = enter(OUTER);
    size += 4096;
    exit(b);
    const c = enter(OUTER);
    size -= 100_000;
    exit(c);
    const r = take();
    expect(r.rows[0].heap).toEqual({ bytes: 4096, gc: 1 });
    expect(r.rows[0].calls).toBe(3);
    zones.heap = null;
  });
});

// The script the Rust test runs too; the fixture is its masked output.
function script() {
  const o = enter(OUTER);
  read(1_234_567);
  const i = enter(INNER);
  copied(500_000);
  exit(i);
  const i2 = enter(INNER);
  copied(123_456);
  exit(i2);
  exit(o);
  const t = enter(OTHER);
  read(2_000_000);
  exit(t);
}

// Time columns differ run to run: total ms, %, self ms in the table and
// nanos, net, self in the rows become X, as the Rust test masks them.
const maskText = (s: string) =>
  s
    .split("\n")
    .map((line, k) =>
      k === 0 || !line
        ? line
        : line.replace(
            /^(.{40} {1,10}\d+) +[\d.]+ +[\d.]+ +[\d.]+ +(.*)$/,
            (_, head, tail) => `${head} X X X ${tail.trim().split(/\s+/).join(" ")}`,
          ),
    )
    .join("\n");
const maskRows = (s: string) => s.replace(/"nanos":\d+,"net":\d+,"self":\d+/g, '"nanos":X,"net":X,"self":X');

describe("formats", () => {
  test("the table and the rows match the Rust crate's", () => {
    fresh();
    script();
    const r = take();
    const dir = new URL("./fixtures/", import.meta.url);
    expect(maskText(r.text(4))).toBe(readFileSync(new URL("tree.txt", dir), "utf8"));
    expect(maskRows(r.ndjson(4))).toBe(readFileSync(new URL("tree.ndjson", dir), "utf8"));
  });

  test("rows parse with the Rust fields in order, then heap", () => {
    fresh();
    countHeap(heap);
    exit(enter(OUTER));
    const line = take().ndjson(0).trim();
    expect(Object.keys(JSON.parse(line))).toEqual([
      "id",
      "parent",
      "zone",
      "name",
      "calls",
      "nanos",
      "net",
      "self",
      "read",
      "copied",
      "heap",
    ]);
    zones.heap = null;
  });

  test("the table shows heap columns only when a row counted it", () => {
    fresh();
    exit(enter(OUTER));
    expect(take().text(0).split("\n")[0]).not.toContain("alloc MB");
    countHeap(heap);
    exit(enter(OUTER));
    const t = take().text(0);
    expect(t.split("\n")[0]).toContain("alloc MB");
    zones.heap = null;
  });
});
