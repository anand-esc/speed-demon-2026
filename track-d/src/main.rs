use std::{cmp::Reverse, collections::{BinaryHeap, HashMap}, fs::File, io::Write, sync::{atomic::{AtomicU32, AtomicU64, Ordering}, Arc, Mutex}, thread};
#[inline(always)] fn parse_u32(b: &[u8], p: &mut usize) -> u32 {
    let mut v = 0u32; while *p < b.len() && b[*p] >= b'0' && b[*p] <= b'9' { v = v * 10 + (b[*p] - b'0') as u32; *p += 1; }
    while *p < b.len() && (b[*p] == b' ' || b[*p] == b'\n' || b[*p] == b'\r') { *p += 1; } v }
#[cfg(unix)] fn mmap_file(p: &str) -> &'static [u8] {
    use std::os::unix::fs::MetadataExt; use std::os::unix::io::AsRawFd; extern "C" { fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8; fn madvise(a: *mut u8, l: usize, ad: i32) -> i32; }
    let f = File::open(p).unwrap(); let l = f.metadata().unwrap().size() as usize; let fd = f.as_raw_fd();
    unsafe { let ptr = mmap(std::ptr::null_mut(), l, 1, 1, fd, 0); madvise(ptr, l, 2); std::slice::from_raw_parts(ptr, l) }
}
#[cfg(not(unix))] fn mmap_file(p: &str) -> &'static [u8] { Box::leak(std::fs::read(p).unwrap().into_boxed_slice()) } fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect(); let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].as_str()).unwrap_or("/data");
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].as_str()).unwrap_or("/out");
    let (g_bytes, mut gp) = (mmap_file(&format!("{}/graph.txt", data_dir)), 0); let (n, m) = (parse_u32(g_bytes, &mut gp) as usize, parse_u32(g_bytes, &mut gp) as usize);
    let (n_th, chunk, mut bounds) = (12, (g_bytes.len() - gp) / 12, vec![gp]);
    for t in 1..n_th { let mut cur = gp + t * chunk; while cur < g_bytes.len() && g_bytes[cur] != b'\n' { cur += 1; } bounds.push(if cur < g_bytes.len() { cur + 1 } else { cur }); } bounds.push(g_bytes.len());
    let (head_atomic, cur_atomic, has_in) = ((0..=n).map(|_| AtomicU32::new(0)).collect::<Vec<_>>(), (0..=n).map(|_| AtomicU32::new(0)).collect::<Vec<_>>(), (0..((n + 63) / 64)).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    thread::scope(|s| { for t in 0..n_th {
        let (b, h, hi) = (&g_bytes[bounds[t]..bounds[t + 1]], &head_atomic, &has_in);
        s.spawn(move || {
            let (mut p, len) = (0, b.len()); while p < len {
                let (u, v, _) = (parse_u32(b, &mut p) as usize, parse_u32(b, &mut p) as usize, parse_u32(b, &mut p));
                h[u + 1].fetch_add(1, Ordering::Relaxed); hi[v >> 6].fetch_or(1 << (v & 63), Ordering::Relaxed);
            }
        });
    }});
    let mut head = vec![0u32; n + 1]; for i in 0..n { head[i + 1] = head[i] + head_atomic[i + 1].load(Ordering::Relaxed); cur_atomic[i].store(head[i], Ordering::Relaxed); }
    let (edges, overflow) = ((0..m).map(|_| AtomicU32::new(0)).collect::<Vec<_>>(), Arc::new(Mutex::new(HashMap::new())));
    thread::scope(|s| { for t in 0..n_th {
        let (b, ca, e, of) = (&g_bytes[bounds[t]..bounds[t + 1]], &cur_atomic, &edges, overflow.clone());
        s.spawn(move || {
            let (mut p, len, mut loc_of) = (0, b.len(), Vec::new()); while p < len {
                let (u, v, w) = (parse_u32(b, &mut p) as usize, parse_u32(b, &mut p), parse_u32(b, &mut p));
                let pos = ca[u].fetch_add(1, Ordering::Relaxed) as usize; e[pos].store((v & 0x1FFFFF) | (w.min(2047) << 21), Ordering::Relaxed);
                if w >= 2047 { loc_of.push((pos as u32, w)); }
            }
            if !loc_of.is_empty() { of.lock().unwrap().extend(loc_of); }
        });
    }});
    let (overflow, raw_edges) = (Arc::try_unwrap(overflow).unwrap().into_inner().unwrap(), unsafe { std::mem::transmute::<Vec<AtomicU32>, Vec<u32>>(edges) });
    let (q_bytes, mut qp, mut queries) = (mmap_file(&format!("{}/queries.txt", data_dir)), 0, Vec::with_capacity(2048)); while qp < q_bytes.len() { queries.push((parse_u32(q_bytes, &mut qp), parse_u32(q_bytes, &mut qp))); }
    let (mut answers, q_chunk) = (vec![-1i64; queries.len()], (queries.len() + n_th - 1) / n_th); let ans_atomic = (0..answers.len()).map(|_| std::sync::atomic::AtomicI64::new(-1)).collect::<Vec<_>>();
    let (q_ref, h_ref, e_ref, hi_ref, of_ref, a_ref) = (&queries, &head, &raw_edges, &has_in, &overflow, &ans_atomic);
    thread::scope(|s| { for t in 0..n_th {
        let (st, en) = (t * q_chunk, ((t + 1) * q_chunk).min(q_ref.len())); if st >= en { continue; }
        s.spawn(move || {
            let (mut dist, mut vis, mut heap) = (vec![u64::MAX; n], Vec::with_capacity(65536), BinaryHeap::with_capacity(65536));
            for qi in st..en {
                let (src, tgt) = q_ref[qi]; if src == tgt { a_ref[qi].store(0, Ordering::Relaxed); continue; }
                let (si, ti) = (src as usize, tgt as usize); if (hi_ref[ti >> 6].load(Ordering::Relaxed) & (1 << (ti & 63))) == 0 || h_ref[si] == h_ref[si + 1] { continue; }
                dist[si] = 0; vis.push(src); heap.push(Reverse((0u64, src))); let mut found = -1i64;
                while let Some(Reverse((d, u))) = heap.pop() {
                    if d > dist[u as usize] { continue; } if u == tgt { found = d as i64; break; }
                    for i in (h_ref[u as usize] as usize)..(h_ref[u as usize + 1] as usize) {
                        let e = e_ref[i]; let (v, mut w) = (e & 0x1FFFFF, (e >> 21) as u64);
                        if w == 2047 { if let Some(&ow) = of_ref.get(&(i as u32)) { w = ow as u64; } }
                        let nd = d + w; if nd < dist[v as usize] { dist[v as usize] = nd; vis.push(v); heap.push(Reverse((nd, v))); }
                    }
                }
                a_ref[qi].store(found, Ordering::Relaxed); for &v in &vis { dist[v as usize] = u64::MAX; } vis.clear(); heap.clear();
            }
        });
    }});
    for (i, a) in answers.iter_mut().enumerate() { *a = ans_atomic[i].load(Ordering::Relaxed); }
    let (mut out, mut buf) = (File::create(format!("{}/distances.txt", out_dir))?, Vec::with_capacity(answers.len() * 12)); for a in answers { buf.extend_from_slice(a.to_string().as_bytes()); buf.push(b'\n'); }
    out.write_all(&buf)
}
