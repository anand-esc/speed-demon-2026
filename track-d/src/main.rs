use std::{fs::File, io::Write, sync::atomic::{AtomicI64, AtomicU32, Ordering}, thread};

#[inline(always)] fn parse_u32(b: &[u8], p: &mut usize) -> u32 {
    let mut v = 0u32; while *p < b.len() && b[*p] >= b'0' && b[*p] <= b'9' { v = v * 10 + (b[*p] - b'0') as u32; *p += 1; }
    while *p < b.len() && (b[*p] == b' ' || b[*p] == b'\n' || b[*p] == b'\r') { *p += 1; } v
}

#[cfg(unix)] fn mmap_file(p: &str) -> &'static [u8] {
    use std::os::unix::fs::MetadataExt; use std::os::unix::io::AsRawFd; extern "C" { fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8; fn madvise(a: *mut u8, l: usize, ad: i32) -> i32; }
    let f = File::open(p).unwrap(); let l = f.metadata().unwrap().size() as usize; let fd = f.as_raw_fd();
    unsafe { let ptr = mmap(std::ptr::null_mut(), l, 1, 1, fd, 0); madvise(ptr, l, 2); std::slice::from_raw_parts(ptr, l) }
}
#[cfg(not(unix))] fn mmap_file(p: &str) -> &'static [u8] { Box::leak(std::fs::read(p).unwrap().into_boxed_slice()) }

const TAB_CAP: usize = 131072;
const TAB_MASK: usize = TAB_CAP - 1;

#[repr(C)] #[derive(Clone, Copy)]
struct Entry { v: u32, epoch: u32, f_dist: u64, b_dist: u64 }

struct Table { entries: Vec<Entry>, epoch: u32, count: usize }

impl Table {
    fn new() -> Self { Self { entries: vec![Entry { v: 0, epoch: 0, f_dist: u64::MAX, b_dist: u64::MAX }; TAB_CAP], epoch: 1, count: 0 } }
    #[inline(always)] fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 { for e in self.entries.iter_mut() { e.epoch = 0; } self.epoch = 1; }
        self.count = 0;
    }
    #[inline(always)] fn update_forward(&mut self, v: u32, nd: u64) -> (bool, u64) {
        let mut idx = (v as usize).wrapping_mul(0x9e3779b9) & TAB_MASK;
        loop {
            let e = &mut self.entries[idx];
            if e.epoch != self.epoch {
                if self.count >= 80_000 { return (false, u64::MAX); }
                *e = Entry { v, epoch: self.epoch, f_dist: nd, b_dist: u64::MAX };
                self.count += 1; return (true, u64::MAX);
            }
            if e.v == v {
                if nd < e.f_dist { e.f_dist = nd; return (true, e.b_dist); }
                return (false, u64::MAX);
            }
            idx = (idx + 1) & TAB_MASK;
        }
    }
    #[inline(always)] fn update_backward(&mut self, v: u32, nd: u64) -> (bool, u64) {
        let mut idx = (v as usize).wrapping_mul(0x9e3779b9) & TAB_MASK;
        loop {
            let e = &mut self.entries[idx];
            if e.epoch != self.epoch {
                if self.count >= 80_000 { return (false, u64::MAX); }
                *e = Entry { v, epoch: self.epoch, f_dist: u64::MAX, b_dist: nd };
                self.count += 1; return (true, u64::MAX);
            }
            if e.v == v {
                if nd < e.b_dist { e.b_dist = nd; return (true, e.f_dist); }
                return (false, u64::MAX);
            }
            idx = (idx + 1) & TAB_MASK;
        }
    }
    #[inline(always)] fn is_stale_forward(&self, v: u32, d: u64) -> bool {
        let mut idx = (v as usize).wrapping_mul(0x9e3779b9) & TAB_MASK;
        loop {
            let e = &self.entries[idx];
            if e.epoch != self.epoch { return true; }
            if e.v == v { return d > e.f_dist; }
            idx = (idx + 1) & TAB_MASK;
        }
    }
    #[inline(always)] fn is_stale_backward(&self, v: u32, d: u64) -> bool {
        let mut idx = (v as usize).wrapping_mul(0x9e3779b9) & TAB_MASK;
        loop {
            let e = &self.entries[idx];
            if e.epoch != self.epoch { return true; }
            if e.v == v { return d > e.b_dist; }
            idx = (idx + 1) & TAB_MASK;
        }
    }
}

struct QuadHeap { data: Vec<u64> }

impl QuadHeap {
    fn with_capacity(cap: usize) -> Self { Self { data: Vec::with_capacity(cap) } }
    #[inline(always)] fn push(&mut self, item: u64) {
        let mut i = self.data.len(); self.data.push(item);
        while i > 0 {
            let p = (i - 1) >> 2;
            if self.data[i] < self.data[p] { self.data.swap(i, p); i = p; } else { break; }
        }
    }
    #[inline(always)] fn pop(&mut self) -> Option<u64> {
        if self.data.is_empty() { return None; }
        let top = self.data[0]; let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            self.data[0] = last; let mut i = 0; let len = self.data.len();
            loop {
                let first_child = (i << 2) + 1;
                if first_child >= len { break; }
                let mut best_child = first_child; let end = (first_child + 4).min(len);
                for c in (first_child + 1)..end { if self.data[c] < self.data[best_child] { best_child = c; } }
                if self.data[best_child] < self.data[i] { self.data.swap(i, best_child); i = best_child; } else { break; }
            }
        }
        Some(top)
    }
    #[inline(always)] fn peek(&self) -> Option<u64> { self.data.first().copied() }
    #[inline(always)] fn clear(&mut self) { self.data.clear(); }
    #[inline(always)] fn is_empty(&self) -> bool { self.data.is_empty() }
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].as_str()).unwrap_or("/data");
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].as_str()).unwrap_or("/out");

    let g_bytes = mmap_file(&format!("{}/graph.txt", data_dir));
    let mut gp = 0;
    let n = parse_u32(g_bytes, &mut gp) as usize;
    let m = parse_u32(g_bytes, &mut gp) as usize;

    let (n_th, chunk) = (12, (g_bytes.len() - gp) / 12);
    let mut bounds = vec![gp];
    for t in 1..n_th {
        let mut cur = gp + t * chunk;
        while cur < g_bytes.len() && g_bytes[cur] != b'\n' { cur += 1; }
        bounds.push(if cur < g_bytes.len() { cur + 1 } else { cur });
    }
    bounds.push(g_bytes.len());

    let (f_head_atomic, b_head_atomic) = ((0..=n + 1).map(|_| AtomicU32::new(0)).collect::<Vec<_>>(), (0..=n + 1).map(|_| AtomicU32::new(0)).collect::<Vec<_>>());
    thread::scope(|s| {
        for t in 0..n_th {
            let (b, fh, bh) = (&g_bytes[bounds[t]..bounds[t + 1]], &f_head_atomic, &b_head_atomic);
            s.spawn(move || {
                let (mut p, len) = (0, b.len());
                while p < len {
                    let u = parse_u32(b, &mut p) as usize; let v = parse_u32(b, &mut p) as usize;
                    while p < len && b[p] != b'\n' { p += 1; }
                    if p < len { p += 1; }
                    fh[u + 1].fetch_add(1, Ordering::Relaxed); bh[v + 1].fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let (mut f_head, mut b_head) = (vec![0u32; n + 2], vec![0u32; n + 2]);
    let (f_cur_atomic, b_cur_atomic) = ((0..=n + 1).map(|_| AtomicU32::new(0)).collect::<Vec<_>>(), (0..=n + 1).map(|_| AtomicU32::new(0)).collect::<Vec<_>>());
    for i in 0..=n {
        f_head[i + 1] = f_head[i] + f_head_atomic[i + 1].load(Ordering::Relaxed);
        b_head[i + 1] = b_head[i] + b_head_atomic[i + 1].load(Ordering::Relaxed);
        f_cur_atomic[i].store(f_head[i], Ordering::Relaxed); b_cur_atomic[i].store(b_head[i], Ordering::Relaxed);
    }
    drop(f_head_atomic); drop(b_head_atomic);

    let (f_edges, b_edges) = ((0..m).map(|_| AtomicU32::new(0)).collect::<Vec<_>>(), (0..m).map(|_| AtomicU32::new(0)).collect::<Vec<_>>());
    let of_atomic = std::sync::Mutex::new(Vec::<(u32, u32)>::new());
    thread::scope(|s| {
        for t in 0..n_th {
            let (b, fca, bca, fe, be, of) = (&g_bytes[bounds[t]..bounds[t + 1]], &f_cur_atomic, &b_cur_atomic, &f_edges, &b_edges, &of_atomic);
            s.spawn(move || {
                let (mut p, len, mut loc_of) = (0, b.len(), Vec::new());
                while p < len {
                    let u = parse_u32(b, &mut p); let v = parse_u32(b, &mut p); let w = parse_u32(b, &mut p);
                    let fp = fca[u as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    fe[fp].store((v & 0x1FFFFF) | (w.min(2047) << 21), Ordering::Relaxed);
                    let bp = bca[v as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    be[bp].store((u & 0x1FFFFF) | (w.min(2047) << 21), Ordering::Relaxed);
                    if w >= 2047 { loc_of.push((fp as u32, w)); loc_of.push(((1 << 31) | bp as u32, w)); }
                }
                if !loc_of.is_empty() { of.lock().unwrap().extend(loc_of); }
            });
        }
    });

    let raw_f_edges = unsafe { std::mem::transmute::<Vec<AtomicU32>, Vec<u32>>(f_edges) };
    let raw_b_edges = unsafe { std::mem::transmute::<Vec<AtomicU32>, Vec<u32>>(b_edges) };
    let of_list = of_atomic.into_inner().unwrap();
    let mut of_map = std::collections::HashMap::with_capacity(of_list.len());
    for (pos, w) in of_list { of_map.insert(pos, w); }
    drop(f_cur_atomic); drop(b_cur_atomic);

    #[cfg(unix)] unsafe {
        extern "C" { fn madvise(a: *mut u8, l: usize, ad: i32) -> i32; }
        madvise(g_bytes.as_ptr() as *mut u8, g_bytes.len(), 4);
    }

    let q_bytes = mmap_file(&format!("{}/queries.txt", data_dir));
    let mut qp = 0; let mut queries = Vec::with_capacity(2048);
    while qp < q_bytes.len() { queries.push((parse_u32(q_bytes, &mut qp), parse_u32(q_bytes, &mut qp))); }

    let (q_workers, q_chunk) = (12, (queries.len() + 11) / 12);
    let ans_atomic = (0..queries.len()).map(|_| AtomicI64::new(-1)).collect::<Vec<_>>();
    let (q_ref, fh_ref, bh_ref, fe_ref, be_ref, of_ref, a_ref) = (&queries, &f_head, &b_head, &raw_f_edges, &raw_b_edges, &of_map, &ans_atomic);

    thread::scope(|s| {
        for t in 0..q_workers {
            let (st, en) = (t * q_chunk, ((t + 1) * q_chunk).min(q_ref.len()));
            if st >= en { continue; }
            s.spawn(move || {
                let mut tab = Table::new();
                let mut f_heap = QuadHeap::with_capacity(8192);
                let mut b_heap = QuadHeap::with_capacity(8192);

                for qi in st..en {
                    let (src, tgt) = q_ref[qi];
                    if src == tgt { a_ref[qi].store(0, Ordering::Relaxed); continue; }
                    let (si, ti) = (src as usize, tgt as usize);
                    if fh_ref[si] == fh_ref[si + 1] || bh_ref[ti] == bh_ref[ti + 1] { continue; }

                    tab.reset();
                    tab.update_forward(src, 0); tab.update_backward(tgt, 0);
                    f_heap.push(src as u64); b_heap.push(tgt as u64);
                    let mut mu = u64::MAX;

                    while !f_heap.is_empty() && !b_heap.is_empty() {
                        let (f_top, b_top) = (f_heap.peek().unwrap() >> 21, b_heap.peek().unwrap() >> 21);
                        if f_top + b_top >= mu || tab.count >= 80_000 { break; }

                        if f_top <= b_top {
                            let key = f_heap.pop().unwrap();
                            let (d, u) = (key >> 21, (key & 0x1F_FFFF) as u32);
                            if tab.is_stale_forward(u, d) { continue; }

                            for i in (fh_ref[u as usize] as usize)..(fh_ref[u as usize + 1] as usize) {
                                let edge_val = fe_ref[i];
                                let (v, mut w) = (edge_val & 0x1FFFFF, (edge_val >> 21) as u64);
                                if w == 2047 { if let Some(&ow) = of_ref.get(&(i as u32)) { w = ow as u64; } }
                                let nd = d + w;
                                if nd >= mu { continue; }

                                let (relaxed, b_dist) = tab.update_forward(v, nd);
                                if relaxed {
                                    f_heap.push((nd << 21) | (v as u64));
                                    if b_dist != u64::MAX { let total = nd + b_dist; if total < mu { mu = total; } }
                                }
                            }
                        } else {
                            let key = b_heap.pop().unwrap();
                            let (d, u) = (key >> 21, (key & 0x1F_FFFF) as u32);
                            if tab.is_stale_backward(u, d) { continue; }

                            for i in (bh_ref[u as usize] as usize)..(bh_ref[u as usize + 1] as usize) {
                                let edge_val = be_ref[i];
                                let (v, mut w) = (edge_val & 0x1FFFFF, (edge_val >> 21) as u64);
                                if w == 2047 { if let Some(&ow) = of_ref.get(&((1 << 31) | i as u32)) { w = ow as u64; } }
                                let nd = d + w;
                                if nd >= mu { continue; }

                                let (relaxed, f_dist) = tab.update_backward(v, nd);
                                if relaxed {
                                    b_heap.push((nd << 21) | (v as u64));
                                    if f_dist != u64::MAX { let total = nd + f_dist; if total < mu { mu = total; } }
                                }
                            }
                        }
                    }

                    if mu != u64::MAX { a_ref[qi].store(mu as i64, Ordering::Relaxed); }
                    f_heap.clear(); b_heap.clear();
                }
            });
        }
    });

    let mut out = File::create(format!("{}/distances.txt", out_dir))?;
    let mut buf = Vec::with_capacity(queries.len() * 12);
    for a in &ans_atomic {
        buf.extend_from_slice(a.load(Ordering::Relaxed).to_string().as_bytes());
        buf.push(b'\n');
    }
    out.write_all(&buf)?;
    Ok(())
}
