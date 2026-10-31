// Track A (Ingest): std-only, zero crates, builds with `cargo build --release --offline`.
// mmap (libc symbol already linked by std) + one worker per hardware thread.
// 1. Workers parse line-aligned slices into flat (group, latency) records.
// 2. Records are counted per (endpoint, hour) group, then scattered into one
//    contiguous latency array, so there is no per-group allocation.
// 3. Each group is sorted in place for nearest-rank percentiles.
// 4. Hours are formatted in parallel, a few at a time, and streamed to disk.
use std::collections::HashMap;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;

const HOURS: usize = 72;
const K: u64 = 0x517c_c1b7_2722_0a95;

#[derive(Default)]
struct Fx(u64);
impl Hasher for Fx {
    fn write(&mut self, b: &[u8]) {
        let mut h = self.0;
        let mut c = b.chunks_exact(8);
        for w in &mut c {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(K);
        }
        let r = c.remainder();
        if !r.is_empty() {
            let mut t = [0u8; 8];
            t[..r.len()].copy_from_slice(r);
            h = (h.rotate_left(5) ^ u64::from_le_bytes(t)).wrapping_mul(K);
        }
        self.0 = h;
    }
    fn finish(&self) -> u64 {
        (self.0 ^ (self.0 >> 29)).wrapping_mul(K)
    }
}
type Map<'a> = HashMap<&'a [u8], u32, BuildHasherDefault<Fx>>;

// Raw pointer into the shared latency array; each worker writes disjoint slots.
struct Ptr(*mut u32);
unsafe impl Sync for Ptr {}
impl Ptr {
    unsafe fn put(&self, i: usize, v: u32) {
        *self.0.add(i) = v;
    }
}

struct Part<'a> {
    eps: Vec<&'a [u8]>,
    recs: Vec<(u32, u32)>, // (local_endpoint << 8 | hour << 1 | is_5xx, latency)
}

fn parse(chunk: &[u8]) -> Part<'_> {
    let n = chunk.len();
    let mut eps = Vec::new();
    let mut map = Map::default();
    let mut recs = Vec::with_capacity(n / 40 + 1);
    let mut i = 0;
    while i < n {
        // "2026-MM-DDTHH:MM:SS.ffffffZ" is exactly 27 bytes.
        let ts = &chunk[i..i + 27];
        let d = ((ts[8] - b'0') * 10 + (ts[9] - b'0')) as usize;
        let day = if ts[6] == b'0' { d } else { 31 + d }; // October or November
        let hour = ((ts[11] - b'0') * 10 + (ts[12] - b'0')) as usize;
        let h = (day - 30) * 24 + hour - 18; // 0..71, window starts Oct 30 18:00
        let mut p = i + 28;
        while chunk[p] != b' ' {
            p += 1;
        }
        p += 1;
        let es = p;
        while chunk[p] != b' ' {
            p += 1;
        }
        let ep = &chunk[es..p];
        let err = (chunk[p + 1] == b'5') as u32;
        p += 5; // " 200 " -> first latency digit
        let mut lat = 0u64;
        while p < n && chunk[p] != b'\n' {
            lat = lat * 10 + (chunk[p] - b'0') as u64;
            p += 1;
        }
        i = p + 1;
        let id = *map.entry(ep).or_insert_with(|| {
            eps.push(ep);
            eps.len() as u32 - 1
        });
        recs.push((id << 8 | (h as u32) << 1 | err, lat as u32));
    }
    Part { eps, recs }
}

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    let mut b = [0u8; 20];
    let mut i = 20;
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&b[i..]);
}

fn put2(out: &mut Vec<u8>, v: usize) {
    out.push(b'0' + (v / 10) as u8);
    out.push(b'0' + (v % 10) as u8);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args
        .windows(2)
        .find(|w| w[0] == "--data")
        .map(|w| w[1].clone())
        .or_else(|| std::env::var("DATA_DIR").ok())
        .unwrap_or_else(|| "/data".into());
    let out_dir = args
        .windows(2)
        .find(|w| w[0] == "--out")
        .map(|w| w[1].clone())
        .or_else(|| std::env::var("OUT_DIR").ok())
        .unwrap_or_else(|| "/out".into());
    let in_path = format!("{data_dir}/requests.log");
    let file = File::open(&in_path).expect("open requests.log");
    let len = file.metadata().unwrap().len() as usize;
    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = File::create(format!("{out_dir}/summary.txt")).expect("create summary.txt");
    if len == 0 {
        return;
    }
    let data: &'static [u8] = unsafe {
        let p = mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0); // PROT_READ, MAP_PRIVATE
        if p as isize == -1 {
            // Fallback if the sandbox forbids mmap: read the whole file into memory.
            Box::leak(std::fs::read(&in_path).expect("read requests.log").into_boxed_slice())
        } else {
            std::slice::from_raw_parts(p, len)
        }
    };

    // Line-aligned slices, one per hardware thread.
    let nt = thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(12);
    let mut bounds = vec![0usize];
    for t in 1..nt {
        let mut s = (len / nt * t).max(*bounds.last().unwrap());
        while s > 0 && s < len && data[s - 1] != b'\n' {
            s += 1;
        }
        bounds.push(s);
    }
    bounds.push(len);
    let mut parts: Vec<Part> = thread::scope(|sc| {
        let hs: Vec<_> = bounds
            .windows(2)
            .map(|w| {
                let c = &data[w[0]..w[1]];
                sc.spawn(move || parse(c))
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Global endpoint ids; a group is (endpoint, hour) = gid * HOURS + hour.
    let mut gmap = Map::default();
    let mut geps: Vec<&[u8]> = Vec::new();
    let remaps: Vec<Vec<u32>> = parts
        .iter()
        .map(|p| {
            p.eps
                .iter()
                .map(|&e| {
                    *gmap.entry(e).or_insert_with(|| {
                        geps.push(e);
                        geps.len() as u32 - 1
                    })
                })
                .collect()
        })
        .collect();
    let g = geps.len() * HOURS;

    // Count records per group (and 5xx per group); rewrite keys to group ids.
    let errs: Vec<AtomicU32> = (0..g).map(|_| AtomicU32::new(0)).collect();
    let cnts: Vec<Vec<u32>> = thread::scope(|sc| {
        let hs: Vec<_> = parts
            .iter_mut()
            .zip(&remaps)
            .map(|(p, rm)| {
                let errs = &errs;
                sc.spawn(move || {
                    let mut c = vec![0u32; g];
                    for r in p.recs.iter_mut() {
                        let gs =
                            rm[(r.0 >> 8) as usize] as usize * HOURS + ((r.0 >> 1) & 127) as usize;
                        c[gs] += 1;
                        if r.0 & 1 != 0 {
                            errs[gs].fetch_add(1, Relaxed);
                        }
                        r.0 = gs as u32;
                    }
                    c
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // starts[gs]..starts[gs + 1] is the group's slice; each worker gets its own sub-range.
    let mut cnts = cnts;
    let mut starts = vec![0u32; g + 1];
    let mut o = 0u32;
    for gs in 0..g {
        starts[gs] = o;
        for c in cnts.iter_mut() {
            let k = c[gs];
            c[gs] = o;
            o += k;
        }
    }
    starts[g] = o;

    // Scatter every latency into its group's slice.
    let mut lats = vec![0u32; o as usize];
    let dp = Ptr(lats.as_mut_ptr());
    thread::scope(|sc| {
        for (p, mut pos) in parts.iter_mut().zip(cnts) {
            let dp = &dp;
            sc.spawn(move || {
                for &(gs, lat) in &p.recs {
                    let q = &mut pos[gs as usize];
                    unsafe { dp.put(*q as usize, lat) };
                    *q += 1;
                }
                p.recs = Vec::new();
            });
        }
    });

    // Sort each group and read nearest-rank percentiles, in balanced slot ranges.
    let mut pcts = vec![[0u32; 3]; g];
    let target = o as usize / nt + 1;
    thread::scope(|sc| {
        let (mut rest, mut rres) = (&mut lats[..], &mut pcts[..]);
        let mut a = 0;
        while a < g {
            let mut b = a + 1;
            while b < g && (starts[b + 1] - starts[a]) as usize <= target {
                b += 1;
            }
            let (d, r2) = std::mem::take(&mut rest).split_at_mut((starts[b] - starts[a]) as usize);
            rest = r2;
            let (r, rr2) = std::mem::take(&mut rres).split_at_mut(b - a);
            rres = rr2;
            let (st, base) = (&starts, starts[a]);
            sc.spawn(move || {
                for (j, out) in r.iter_mut().enumerate() {
                    let s = &mut d[(st[a + j] - base) as usize..(st[a + j + 1] - base) as usize];
                    let n = s.len();
                    if n == 0 {
                        continue;
                    }
                    s.sort_unstable();
                    for (k, pc) in [50usize, 95, 99].into_iter().enumerate() {
                        out[k] = s[(pc * n + 99) / 100 - 1];
                    }
                }
            });
            a = b;
        }
    });
    drop(lats);

    // Output: hour-major, endpoints in byte order. Format a wave of hours in parallel, then stream it.
    let mut order: Vec<usize> = (0..geps.len()).collect();
    order.sort_unstable_by(|&a, &b| geps[a].cmp(geps[b]));
    let fmt = |h: usize| -> Vec<u8> {
        let mut o = Vec::with_capacity(160 << 10);
        let abs = h + 18;
        let d = 30 + abs / 24;
        let (mo, da) = if d <= 31 { (10, d) } else { (11, d - 31) };
        let mut hdr = b"2026-".to_vec();
        put2(&mut hdr, mo);
        hdr.push(b'-');
        put2(&mut hdr, da);
        hdr.push(b'T');
        put2(&mut hdr, abs % 24);
        hdr.extend_from_slice(b":00:00Z ");
        for &gid in &order {
            let gs = gid * HOURS + h;
            let cnt = starts[gs + 1] - starts[gs];
            if cnt == 0 {
                continue;
            }
            let e = errs[gs].load(Relaxed);
            o.extend_from_slice(&hdr);
            o.extend_from_slice(geps[gid]);
            for v in [cnt, pcts[gs][0], pcts[gs][1], pcts[gs][2], e] {
                o.push(b' ');
                put(&mut o, v as u64);
            }
            o.push(b' ');
            put(&mut o, e as u64 * 1_000_000 / cnt as u64);
            o.push(b'\n');
        }
        o
    };
    let fmt = &fmt;
    for w in (0..HOURS).step_by(nt) {
        let bufs: Vec<Vec<u8>> = thread::scope(|sc| {
            let hs: Vec<_> = (w..(w + nt).min(HOURS))
                .map(|h| sc.spawn(move || fmt(h)))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for b in bufs {
            out_file.write_all(&b).expect("write summary.txt");
        }
    }
}