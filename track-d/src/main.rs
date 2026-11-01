use std::{fs::File, io::Write, sync::atomic::{AtomicI64, AtomicU32, Ordering}, thread};

#[inline(always)]
unsafe fn parse_u32_fast(ptr: &mut *const u8) -> u32 {
    let mut v = (**ptr - b'0') as u32;
    *ptr = ptr.add(1);
    while **ptr >= b'0' {
        v = v * 10 + (**ptr - b'0') as u32;
        *ptr = ptr.add(1);
    }
    *ptr = ptr.add(1);
    v
}

#[cfg(unix)]
fn mmap_file(p: &str) -> &'static [u8] {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
        fn madvise(a: *mut u8, l: usize, ad: i32) -> i32;
    }
    let f = File::open(p).unwrap();
    let l = f.metadata().unwrap().size() as usize;
    let fd = f.as_raw_fd();
    unsafe {
        let ptr = mmap(std::ptr::null_mut(), l, 1, 1, fd, 0);
        madvise(ptr, l, 2);
        std::slice::from_raw_parts(ptr, l)
    }
}

#[cfg(not(unix))]
fn mmap_file(p: &str) -> &'static [u8] {
    Box::leak(std::fs::read(p).unwrap().into_boxed_slice())
}

const TAB_CAP: usize = 131072;
const TAB_MASK: usize = TAB_CAP - 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    v: u32,
    epoch: u32,
    f_dist: u64,
    b_dist: u64,
}

struct Table {
    entries: Vec<Entry>,
    epoch: u32,
    count: usize,
}

impl Table {
    fn new() -> Self {
        Self {
            entries: unsafe { std::mem::transmute(vec![0u8; TAB_CAP * std::mem::size_of::<Entry>()]) },
            epoch: 1,
            count: 0,
        }
    }
    #[inline(always)]
    fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            for e in self.entries.iter_mut() { e.epoch = 0; }
            self.epoch = 1;
        }
        self.count = 0;
    }
    #[inline(always)]
    fn update_forward(&mut self, v: u32, nd: u64) -> (bool, u64) {
        let mut idx = ((v as u64).wrapping_mul(0x517cc1b727220a95) >> 47) as usize;
        loop {
            let e = unsafe { self.entries.get_unchecked_mut(idx) };
            if e.epoch != self.epoch {
                if self.count >= 80_000 { return (false, u64::MAX); }
                *e = Entry { v, epoch: self.epoch, f_dist: nd, b_dist: u64::MAX };
                self.count += 1;
                return (true, u64::MAX);
            }
            if e.v == v {
                if nd < e.f_dist {
                    e.f_dist = nd;
                    return (true, e.b_dist);
                }
                return (false, u64::MAX);
            }
            idx = (idx + 1) & TAB_MASK;
        }
    }
    #[inline(always)]
    fn update_backward(&mut self, v: u32, nd: u64) -> (bool, u64) {
        let mut idx = ((v as u64).wrapping_mul(0x517cc1b727220a95) >> 47) as usize;
        loop {
            let e = unsafe { self.entries.get_unchecked_mut(idx) };
            if e.epoch != self.epoch {
                if self.count >= 80_000 { return (false, u64::MAX); }
                *e = Entry { v, epoch: self.epoch, f_dist: u64::MAX, b_dist: nd };
                self.count += 1;
                return (true, u64::MAX);
            }
            if e.v == v {
                if nd < e.b_dist {
                    e.b_dist = nd;
                    return (true, e.f_dist);
                }
                return (false, u64::MAX);
            }
            idx = (idx + 1) & TAB_MASK;
        }
    }
    #[inline(always)]
    fn is_stale_forward(&self, v: u32, d: u64) -> bool {
        let mut idx = ((v as u64).wrapping_mul(0x517cc1b727220a95) >> 47) as usize;
        loop {
            let e = unsafe { self.entries.get_unchecked(idx) };
            if e.epoch != self.epoch { return true; }
            if e.v == v { return d > e.f_dist; }
            idx = (idx + 1) & TAB_MASK;
        }
    }
    #[inline(always)]
    fn is_stale_backward(&self, v: u32, d: u64) -> bool {
        let mut idx = ((v as u64).wrapping_mul(0x517cc1b727220a95) >> 47) as usize;
        loop {
            let e = unsafe { self.entries.get_unchecked(idx) };
            if e.epoch != self.epoch { return true; }
            if e.v == v { return d > e.b_dist; }
            idx = (idx + 1) & TAB_MASK;
        }
    }
}

struct QuadHeap {
    data: Vec<u64>,
}

impl QuadHeap {
    fn with_capacity(cap: usize) -> Self { Self { data: Vec::with_capacity(cap) } }
    #[inline(always)]
    fn push(&mut self, item: u64) {
        let mut i = self.data.len();
        self.data.push(item);
        while i > 0 {
            let p = (i - 1) >> 2;
            let parent_val = unsafe { *self.data.get_unchecked(p) };
            if item < parent_val {
                unsafe { *self.data.get_unchecked_mut(i) = parent_val; }
                i = p;
            } else {
                break;
            }
        }
        unsafe { *self.data.get_unchecked_mut(i) = item; }
    }
    #[inline(always)]
    fn pop(&mut self) -> Option<u64> {
        if self.data.is_empty() { return None; }
        let top = unsafe { *self.data.get_unchecked(0) };
        let last = self.data.pop().unwrap();
        let len = self.data.len();
        if len > 0 {
            let mut i = 0;
            loop {
                let first_child = (i << 2) + 1;
                if first_child >= len { break; }
                let mut best_child = first_child;
                let end = (first_child + 4).min(len);
                for c in (first_child + 1)..end {
                    if unsafe { *self.data.get_unchecked(c) < *self.data.get_unchecked(best_child) } {
                        best_child = c;
                    }
                }
                let best_val = unsafe { *self.data.get_unchecked(best_child) };
                if best_val < last {
                    unsafe { *self.data.get_unchecked_mut(i) = best_val; }
                    i = best_child;
                } else {
                    break;
                }
            }
            unsafe { *self.data.get_unchecked_mut(i) = last; }
        }
        Some(top)
    }
    #[inline(always)]
    fn peek(&self) -> Option<u64> { self.data.first().copied() }
    #[inline(always)]
    fn clear(&mut self) { self.data.clear(); }
    #[inline(always)]
    fn is_empty(&self) -> bool { self.data.is_empty() }
}

#[inline(always)]
fn write_i64(mut n: i64, buf: &mut Vec<u8>) {
    if n == -1 {
        buf.extend_from_slice(b"-1\n");
        return;
    }
    if n == 0 {
        buf.extend_from_slice(b"0\n");
        return;
    }
    let mut tmp = [0u8; 24];
    let mut i = 24;
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&tmp[i..]);
    buf.push(b'\n');
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].as_str()).unwrap_or("/data");
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].as_str()).unwrap_or("/out");

    let g_bytes = mmap_file(&format!("{}/graph.txt", data_dir));
    let mut gp_ptr = g_bytes.as_ptr();
    let n = unsafe { parse_u32_fast(&mut gp_ptr) as usize };
    let m = unsafe { parse_u32_fast(&mut gp_ptr) as usize };
    let gp = unsafe { gp_ptr.offset_from(g_bytes.as_ptr()) as usize };

    let n_th = 12;
    let chunk = (g_bytes.len() - gp) / n_th;
    let mut bounds = Vec::with_capacity(n_th + 1);
    bounds.push(gp);
    for t in 1..n_th {
        let mut cur = gp + t * chunk;
        while cur < g_bytes.len() && g_bytes[cur] != b'\n' { cur += 1; }
        bounds.push(if cur < g_bytes.len() { cur + 1 } else { cur });
    }
    bounds.push(g_bytes.len());

    let f_head_atomic: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; n + 2]) };
    let b_head_atomic: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; n + 2]) };

    thread::scope(|s| {
        for t in 0..n_th {
            let (b, fh, bh) = (&g_bytes[bounds[t]..bounds[t + 1]], &f_head_atomic, &b_head_atomic);
            s.spawn(move || {
                unsafe {
                    let mut ptr = b.as_ptr();
                    let end = ptr.add(b.len());
                    while ptr < end {
                        let u = parse_u32_fast(&mut ptr) as usize;
                        let v = parse_u32_fast(&mut ptr) as usize;
                        while *ptr >= b'0' { ptr = ptr.add(1); }
                        ptr = ptr.add(1);
                        fh.get_unchecked(u + 1).fetch_add(1, Ordering::Relaxed);
                        bh.get_unchecked(v + 1).fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    let mut f_sum = 0u32;
    let mut b_sum = 0u32;
    for i in 0..n {
        let f_deg = f_head_atomic[i + 1].load(Ordering::Relaxed);
        let b_deg = b_head_atomic[i + 1].load(Ordering::Relaxed);
        f_head_atomic[i + 1].store(f_sum, Ordering::Relaxed);
        b_head_atomic[i + 1].store(b_sum, Ordering::Relaxed);
        f_sum += f_deg;
        b_sum += b_deg;
    }
    f_head_atomic[n + 1].store(f_sum, Ordering::Relaxed);
    b_head_atomic[n + 1].store(b_sum, Ordering::Relaxed);

    let f_edges: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; m]) };
    let b_edges: Vec<AtomicU32> = unsafe { std::mem::transmute(vec![0u32; m]) };
    let of_atomic = std::sync::Mutex::new(Vec::<(u32, u32)>::new());

    thread::scope(|s| {
        for t in 0..n_th {
            let (b, fca, bca, fe, be, of) = (&g_bytes[bounds[t]..bounds[t + 1]], &f_head_atomic, &b_head_atomic, &f_edges, &b_edges, &of_atomic);
            s.spawn(move || {
                let mut loc_of = Vec::new();
                unsafe {
                    let mut ptr = b.as_ptr();
                    let end = ptr.add(b.len());
                    while ptr < end {
                        let u = parse_u32_fast(&mut ptr);
                        let v = parse_u32_fast(&mut ptr);
                        let w = parse_u32_fast(&mut ptr);
                        let fp = fca.get_unchecked(u as usize + 1).fetch_add(1, Ordering::Relaxed) as usize;
                        let bp = bca.get_unchecked(v as usize + 1).fetch_add(1, Ordering::Relaxed) as usize;
                        if w < 2047 {
                            let packed_w = w << 21;
                            fe.get_unchecked(fp).store(v | packed_w, Ordering::Relaxed);
                            be.get_unchecked(bp).store(u | packed_w, Ordering::Relaxed);
                        } else {
                            let packed_w = 2047 << 21;
                            fe.get_unchecked(fp).store(v | packed_w, Ordering::Relaxed);
                            be.get_unchecked(bp).store(u | packed_w, Ordering::Relaxed);
                            loc_of.push((fp as u32, w));
                            loc_of.push(((1 << 31) | bp as u32, w));
                        }
                    }
                }
                if !loc_of.is_empty() { of.lock().unwrap().extend(loc_of); }
            });
        }
    });

    let raw_f_head = unsafe { std::mem::transmute::<Vec<AtomicU32>, Vec<u32>>(f_head_atomic) };
    let raw_b_head = unsafe { std::mem::transmute::<Vec<AtomicU32>, Vec<u32>>(b_head_atomic) };
    let raw_f_edges = unsafe { std::mem::transmute::<Vec<AtomicU32>, Vec<u32>>(f_edges) };
    let raw_b_edges = unsafe { std::mem::transmute::<Vec<AtomicU32>, Vec<u32>>(b_edges) };
    let of_list = of_atomic.into_inner().unwrap();
    let mut of_map = std::collections::HashMap::with_capacity(of_list.len());
    for (pos, w) in of_list { of_map.insert(pos, w); }

    #[cfg(unix)] unsafe {
        extern "C" { fn madvise(a: *mut u8, l: usize, ad: i32) -> i32; }
        madvise(g_bytes.as_ptr() as *mut u8, g_bytes.len(), 4);
    }

    let q_bytes = mmap_file(&format!("{}/queries.txt", data_dir));
    let mut q_ptr = q_bytes.as_ptr();
    let q_end = unsafe { q_ptr.add(q_bytes.len()) };
    let mut queries = Vec::with_capacity(2048);
    while q_ptr < q_end {
        unsafe { queries.push((parse_u32_fast(&mut q_ptr), parse_u32_fast(&mut q_ptr))); }
    }

    let q_workers = 12;
    let q_chunk = (queries.len() + q_workers - 1) / q_workers;
    let ans_atomic = (0..queries.len()).map(|_| AtomicI64::new(-1)).collect::<Vec<_>>();
    let (q_ref, fh_ref, bh_ref, fe_ref, be_ref, of_ref, a_ref) = (&queries, &raw_f_head, &raw_b_head, &raw_f_edges, &raw_b_edges, &of_map, &ans_atomic);

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
                    tab.update_forward(src, 0);
                    tab.update_backward(tgt, 0);
                    f_heap.push(src as u64);
                    b_heap.push(tgt as u64);
                    let mut mu = u64::MAX;

                    while !f_heap.is_empty() && !b_heap.is_empty() {
                        let (f_top, b_top) = (f_heap.peek().unwrap() >> 21, b_heap.peek().unwrap() >> 21);
                        if f_top + b_top >= mu || tab.count >= 80_000 { break; }

                        if f_top <= b_top {
                            let key = f_heap.pop().unwrap();
                            let (d, u) = (key >> 21, (key & 0x1F_FFFF) as u32);
                            if tab.is_stale_forward(u, d) { continue; }

                            let start_idx = unsafe { *fh_ref.get_unchecked(u as usize) as usize };
                            let end_idx = unsafe { *fh_ref.get_unchecked(u as usize + 1) as usize };
                            for i in start_idx..end_idx {
                                let edge_val = unsafe { *fe_ref.get_unchecked(i) };
                                let (v, mut w) = (edge_val & 0x1FFFFF, (edge_val >> 21) as u64);
                                if w == 2047 {
                                    if let Some(&ow) = of_ref.get(&(i as u32)) { w = ow as u64; }
                                }
                                let nd = d + w;
                                if nd >= mu { continue; }

                                let (relaxed, b_dist) = tab.update_forward(v, nd);
                                if relaxed {
                                    f_heap.push((nd << 21) | (v as u64));
                                    if b_dist != u64::MAX {
                                        let total = nd + b_dist;
                                        if total < mu { mu = total; }
                                    }
                                }
                            }
                        } else {
                            let key = b_heap.pop().unwrap();
                            let (d, u) = (key >> 21, (key & 0x1F_FFFF) as u32);
                            if tab.is_stale_backward(u, d) { continue; }

                            let start_idx = unsafe { *bh_ref.get_unchecked(u as usize) as usize };
                            let end_idx = unsafe { *bh_ref.get_unchecked(u as usize + 1) as usize };
                            for i in start_idx..end_idx {
                                let edge_val = unsafe { *be_ref.get_unchecked(i) };
                                let (v, mut w) = (edge_val & 0x1FFFFF, (edge_val >> 21) as u64);
                                if w == 2047 {
                                    if let Some(&ow) = of_ref.get(&((1 << 31) | i as u32)) { w = ow as u64; }
                                }
                                let nd = d + w;
                                if nd >= mu { continue; }

                                let (relaxed, f_dist) = tab.update_backward(v, nd);
                                if relaxed {
                                    b_heap.push((nd << 21) | (v as u64));
                                    if f_dist != u64::MAX {
                                        let total = nd + f_dist;
                                        if total < mu { mu = total; }
                                    }
                                }
                            }
                        }
                    }

                    if mu != u64::MAX { a_ref[qi].store(mu as i64, Ordering::Relaxed); }
                    f_heap.clear();
                    b_heap.clear();
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(out_dir);
    let mut out = File::create(format!("{}/distances.txt", out_dir))?;
    let mut buf = Vec::with_capacity(queries.len() * 12);
    for a in &ans_atomic {
        write_i64(a.load(Ordering::Relaxed), &mut buf);
    }
    out.write_all(&buf)?;
    std::process::exit(0);
}
