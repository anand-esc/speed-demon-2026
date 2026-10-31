use std::{cmp::Reverse, collections::BinaryHeap, fs::File, io::Write, thread};
#[derive(Clone, Copy)] struct Ptr<T>(*mut T); unsafe impl<T> Send for Ptr<T> {} unsafe impl<T> Sync for Ptr<T> {}
impl<T: Copy> Ptr<T> { #[inline(always)] unsafe fn set(self, i: usize, v: T) { *self.0.add(i) = v; } }
#[inline(always)] fn parse_u32(b: &[u8], p: &mut usize) -> u32 {
    let mut v = 0u32; while *p < b.len() && b[*p] >= b'0' && b[*p] <= b'9' { v = v * 10 + (b[*p] - b'0') as u32; *p += 1; }
    while *p < b.len() && (b[*p] == b' ' || b[*p] == b'\n' || b[*p] == b'\r') { *p += 1; }
    v
}
fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect(); let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].as_str()).unwrap_or("/data");
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].as_str()).unwrap_or("/out");
    let (g_bytes, mut gp) = (std::fs::read(format!("{}/graph.txt", data_dir))?, 0); let (n, m) = (parse_u32(&g_bytes, &mut gp) as usize, parse_u32(&g_bytes, &mut gp) as usize);
    let (n_th, chunk, mut bounds) = (12, (g_bytes.len() - gp) / 12, vec![gp]);
    for t in 1..n_th {
        let mut cur = gp + t * chunk; while cur < g_bytes.len() && g_bytes[cur] != b'\n' { cur += 1; }
        bounds.push(if cur < g_bytes.len() { cur + 1 } else { cur });
    }
    bounds.push(g_bytes.len());
    let (mut parts, mut has_in) = (vec![Vec::new(); n_th], vec![false; n]);
    let in_ptr = Ptr(has_in.as_mut_ptr());
    thread::scope(|s| {
        let ths: Vec<_> = (0..n_th).map(|t| {
            let b = &g_bytes[bounds[t]..bounds[t + 1]];
            s.spawn(move || {
                let (mut p, len, mut loc) = (0, b.len(), Vec::with_capacity(b.len() / 18 + 128));
                while p < len { let (u, v, w) = (parse_u32(b, &mut p), parse_u32(b, &mut p), parse_u32(b, &mut p)); loc.push((u, v, w)); unsafe { in_ptr.set(v as usize, true); } }
                loc
            })
        }).collect();
        for (i, h) in ths.into_iter().enumerate() { parts[i] = h.join().unwrap(); }
    });
    let (mut head, mut edges) = (vec![0u32; n + 1], vec![(0u32, 0u32); m]);
    for p in &parts { for &(u, _, _) in p { head[u as usize + 1] += 1; } }
    for i in 0..n { head[i + 1] += head[i]; }
    let mut cur = head.clone(); for p in parts { for (u, v, w) in p { let pos = cur[u as usize] as usize; cur[u as usize] += 1; edges[pos] = (v, w); } }
    let (q_bytes, mut qp, mut queries) = (std::fs::read(format!("{}/queries.txt", data_dir))?, 0, Vec::with_capacity(2048)); while qp < q_bytes.len() { queries.push((parse_u32(&q_bytes, &mut qp), parse_u32(&q_bytes, &mut qp))); }
    let (mut answers, q_chunk) = (vec![-1i64; queries.len()], (queries.len() + n_th - 1) / n_th);
    let (a_ptr, q_ref, h_ref, e_ref, in_ref) = (Ptr(answers.as_mut_ptr()), &queries, &head, &edges, &has_in);
    thread::scope(|s| {
        for t in 0..n_th {
            let (st, en) = (t * q_chunk, ((t + 1) * q_chunk).min(q_ref.len())); if st >= en { continue; }
            s.spawn(move || {
                let (mut dist, mut vis, mut heap) = (vec![u64::MAX; n], Vec::with_capacity(65536), BinaryHeap::with_capacity(65536));
                for qi in st..en {
                    let (src, tgt) = q_ref[qi];
                    if src == tgt { unsafe { a_ptr.set(qi, 0); } continue; }
                    if !in_ref[tgt as usize] || h_ref[src as usize] == h_ref[src as usize + 1] { continue; }
                    dist[src as usize] = 0; vis.push(src); heap.push(Reverse((0u64, src)));
                    let mut found = -1i64;
                    while let Some(Reverse((d, u))) = heap.pop() {
                        if d > dist[u as usize] { continue; }
                        if u == tgt { found = d as i64; break; }
                        for i in (h_ref[u as usize] as usize)..(h_ref[u as usize + 1] as usize) {
                            let (v, w) = e_ref[i]; let nd = d + w as u64;
                            if nd < dist[v as usize] { dist[v as usize] = nd; vis.push(v); heap.push(Reverse((nd, v))); }
                        }
                    }
                    unsafe { a_ptr.set(qi, found); }
                    for &v in &vis { dist[v as usize] = u64::MAX; } vis.clear(); heap.clear();
                }
            });
        }
    });
    let (mut out, mut buf) = (File::create(format!("{}/distances.txt", out_dir))?, Vec::with_capacity(answers.len() * 12)); for a in answers { buf.extend_from_slice(a.to_string().as_bytes()); buf.push(b'\n'); }
    out.write_all(&buf)
}
