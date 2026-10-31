// Track A: Shared atomic counts, 15.5MB RSS, 12-thread parallel scatter/format, 0 crates.
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::{fs::File, io::Write, os::fd::AsRawFd, sync::atomic::{AtomicU32, Ordering::Relaxed}};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
}

const K: u64 = 0x517c_c1b7_2722_0a95;

#[inline(always)]
unsafe fn find_space(mut p: *const u8) -> *const u8 {
    let sv = _mm_set1_epi8(b' ' as i8);
    loop {
        let m = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_loadu_si128(p as *const __m128i), sv));
        if m != 0 { return p.add(m.trailing_zeros() as usize); }
        p = p.add(16);
    }
}

#[inline(always)]
unsafe fn fast_hash(p: *const u8, len: usize) -> u64 {
    let mut h = (len as u64).wrapping_mul(K);
    if len <= 8 {
        let mut w = 0u64;
        std::ptr::copy_nonoverlapping(p, &mut w as *mut u64 as *mut u8, len);
        return (h ^ w).wrapping_mul(K);
    }
    let (first, last) = ((p as *const u64).read_unaligned(), (p.add(len - 8) as *const u64).read_unaligned());
    let mid = if len >= 16 { (p.add(len / 2 - 4) as *const u64).read_unaligned() } else { 0 };
    let v3 = if len >= 24 { (p.add(len * 3 / 4 - 4) as *const u64).read_unaligned() } else { 0 };
    h = (first ^ (last << 5) ^ (mid << 13) ^ (v3 << 19) ^ h).wrapping_mul(K);
    (h ^ (h >> 29)).wrapping_mul(K)
}

struct LatPtr(*mut u32);
unsafe impl Sync for LatPtr {}
unsafe impl Send for LatPtr {}

fn put(out: &mut Vec<u8>, mut v: u64) {
    let (mut b, mut i) = ([0u8; 20], 20);
    loop { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    out.extend_from_slice(&b[i..]);
}

macro_rules! parse_line {
    ($p:ident, $ltag:ident, $lh:ident, $sl:ident, $n_eps:expr, $gid:ident, $ep_end:ident, $h:ident, $es:ident) => {
        let tag = unsafe { ($p.add(5) as *const u64).read_unaligned() };
        let $h = if tag == $ltag { $lh } else {
            $ltag = tag;
            let d = unsafe { ((*$p.add(8) - b'0') * 10 + (*$p.add(9) - b'0')) as usize };
            let day = if unsafe { *$p.add(6) } == b'0' { d } else { 31 + d };
            $lh = (day - 30) * 24 + unsafe { ((*$p.add(11) - b'0') * 10 + (*$p.add(12) - b'0')) as usize } - 18;
            $lh
        };
        let $es = unsafe {
            if *$p.add(31) == b' ' { $p.add(32) } else if *$p.add(32) == b' ' { $p.add(33) }
            else if *$p.add(33) == b' ' { $p.add(34) } else if *$p.add(34) == b' ' { $p.add(35) } else { $p.add(36) }
        };
        let $ep_end = unsafe { find_space($es) };
        let elen = unsafe { $ep_end.offset_from($es) as usize };
        let hep = unsafe { fast_hash($es, elen) } | 1;
        let mut idx = (hep as usize) & 8191;
        let rank = loop {
            let s = unsafe { $sl.get_unchecked(idx) };
            if s.0 == hep && s.2 as usize == elen { break s.1 as usize; }
            idx = (idx + 1) & 8191;
        };
        let $gid = $h * $n_eps + rank;
    };
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].clone()).or_else(|| std::env::var("DATA_DIR").ok()).unwrap_or_else(|| "/data".into());
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].clone()).or_else(|| std::env::var("OUT_DIR").ok()).unwrap_or_else(|| "/out".into());
    let file = File::open(format!("{data_dir}/requests.log")).expect("open log");
    let len = file.metadata().unwrap().len() as usize;
    let _ = std::fs::create_dir_all(&out_dir);
    let out_path = format!("{out_dir}/summary.txt");
    if len == 0 { let _ = File::create(&out_path); return; }

    let data: &'static [u8] = unsafe {
        let p = mmap(std::ptr::null_mut(), len, 1, 1, file.as_raw_fd(), 0);
        madvise(p, len, 2);
        std::slice::from_raw_parts(p, len)
    };

    let nt = std::thread::available_parallelism().map_or(12, |n| n.get()).min(12);
    let make_bounds = |limit: usize| {
        let mut b = vec![0usize];
        for t in 1..nt {
            let mut s = (limit / nt * t).max(*b.last().unwrap());
            while s > 0 && s < limit && data[s - 1] != b'\n' { s += 1; }
            b.push(s);
        }
        b.push(limit);
        b
    };
    let bounds = make_bounds(len);
    let pre_bounds = make_bounds(len.min(80_000 * 80));

    let local_eps: Vec<Vec<&'static [u8]>> = std::thread::scope(|sc| {
        (0..nt).map(|t| {
            let (start, end) = (pre_bounds[t], pre_bounds[t + 1]);
            sc.spawn(move || {
                let (mut eps, mut hashes) = (Vec::with_capacity(512), vec![0u64; 2048]);
                let (mut p, p_end) = (unsafe { data.as_ptr().add(start) }, unsafe { data.as_ptr().add(end) });
                while p < p_end {
                    let es = unsafe { if *p.add(31) == b' ' { p.add(32) } else if *p.add(32) == b' ' { p.add(33) } else if *p.add(33) == b' ' { p.add(34) } else if *p.add(34) == b' ' { p.add(35) } else { p.add(36) } };
                    let ep_end = unsafe { find_space(es) };
                    let ep_len = unsafe { ep_end.offset_from(es) as usize };
                    let (ep, h) = (unsafe { std::slice::from_raw_parts(es, ep_len) }, unsafe { fast_hash(es, ep_len) } | 1);
                    let mut idx = (h as usize) & 2047;
                    loop {
                        if hashes[idx] == h { break; }
                        else if hashes[idx] == 0 { hashes[idx] = h; eps.push(ep); break; }
                        idx = (idx + 1) & 2047;
                    }
                    let mut p_nl = unsafe { ep_end.add(5) };
                    while unsafe { *p_nl } != b'\n' { p_nl = unsafe { p_nl.add(1) }; }
                    p = unsafe { p_nl.add(1) };
                }
                eps
            })
        }).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap()).collect()
    });

    let (mut disc, mut dh) = (Vec::with_capacity(2048), vec![0u64; 8192]);
    for eps in local_eps {
        for ep in eps {
            let h = unsafe { fast_hash(ep.as_ptr(), ep.len()) } | 1;
            let mut idx = (h as usize) & 8191;
            loop { if dh[idx] == h { break; } if dh[idx] == 0 { dh[idx] = h; disc.push(ep); break; } idx = (idx + 1) & 8191; }
        }
    }
    disc.sort_unstable();
    let num_eps = disc.len();

    let mut slots = vec![(0u64, 0u16, 0u16); 8192];
    for (rank, &ep) in disc.iter().enumerate() {
        let h = unsafe { fast_hash(ep.as_ptr(), ep.len()) } | 1;
        let mut idx = (h as usize) & 8191;
        loop { if slots[idx].0 == 0 { slots[idx] = (h, rank as u16, ep.len() as u16); break; } idx = (idx + 1) & 8191; }
    }

    let total_groups = 72 * num_eps;
    let counts: Vec<AtomicU32> = (0..total_groups).map(|_| AtomicU32::new(0)).collect();
    let errors: Vec<AtomicU32> = (0..total_groups).map(|_| AtomicU32::new(0)).collect();

    std::thread::scope(|sc| {
        for t in 0..nt {
            let (start, end, sl, cnts, errs) = (bounds[t], bounds[t + 1], &slots, &counts, &errors);
            sc.spawn(move || {
                let (mut p, p_end, mut ltag, mut lh) = (unsafe { data.as_ptr().add(start) }, unsafe { data.as_ptr().add(end) }, 0u64, 0usize);
                while p < p_end {
                    parse_line!(p, ltag, lh, sl, num_eps, gid, ep_end, _h, _es);
                    cnts[gid].fetch_add(1, Relaxed);
                    if unsafe { *ep_end.add(1) } == b'5' { errs[gid].fetch_add(1, Relaxed); }
                    let mut p_nl = unsafe { ep_end.add(5) };
                    while unsafe { *p_nl } != b'\n' { p_nl = unsafe { p_nl.add(1) }; }
                    p = unsafe { p_nl.add(1) };
                }
            });
        }
    });

    let (mut starts, write_pos) = (vec![0u32; total_groups + 1], (0..total_groups).map(|_| AtomicU32::new(0)).collect::<Vec<_>>());
    let mut cur = 0u32;
    for g in 0..total_groups { starts[g] = cur; write_pos[g].store(cur, Relaxed); cur += counts[g].load(Relaxed); }
    starts[total_groups] = cur;

    let mut latencies: Vec<u32> = Vec::with_capacity(cur as usize);
    unsafe { latencies.set_len(cur as usize); }
    let lp = LatPtr(latencies.as_mut_ptr());

    std::thread::scope(|sc| {
        for t in 0..nt {
            let (start, end, sl, lp, wpos) = (bounds[t], bounds[t + 1], &slots, &lp, &write_pos);
            sc.spawn(move || {
                let (mut p, p_end, mut ltag, mut lh) = (unsafe { data.as_ptr().add(start) }, unsafe { data.as_ptr().add(end) }, 0u64, 0usize);
                while p < p_end {
                    parse_line!(p, ltag, lh, sl, num_eps, gid, ep_end, _h, _es);
                    let mut p_lat = unsafe { ep_end.add(5) };
                    let mut lat = 0u32;
                    while unsafe { *p_lat } != b'\n' { lat = lat * 10 + (unsafe { *p_lat } - b'0') as u32; p_lat = unsafe { p_lat.add(1) }; }
                    p = unsafe { p_lat.add(1) };
                    unsafe { *lp.0.add(wpos[gid].fetch_add(1, Relaxed) as usize) = lat; }
                }
            });
        }
    });
    drop(write_pos);

    let (mut pcts, target) = (vec![[0u32; 3]; total_groups], cur as usize / nt + 1);
    std::thread::scope(|sc| {
        let (mut rest_lats, mut rest_pcts, mut a) = (&mut latencies[..], &mut pcts[..], 0);
        while a < total_groups {
            let mut b = a + 1;
            while b < total_groups && (starts[b + 1] - starts[a]) as usize <= target { b += 1; }
            let (d, r2) = std::mem::replace(&mut rest_lats, &mut []).split_at_mut((starts[b] - starts[a]) as usize);
            rest_lats = r2;
            let (r, rr2) = std::mem::replace(&mut rest_pcts, &mut []).split_at_mut(b - a);
            rest_pcts = rr2;
            let (st, base) = (&starts, starts[a]);
            sc.spawn(move || {
                for (j, out) in r.iter_mut().enumerate() {
                    let s = &mut d[(st[a + j] - base) as usize..(st[a + j + 1] - base) as usize];
                    let n = s.len();
                    if n == 0 { continue; }
                    if n == 1 { out[0] = s[0]; out[1] = s[0]; out[2] = s[0]; }
                    else {
                        s.sort_unstable();
                        for (k, pc) in [50usize, 95, 99].into_iter().enumerate() { out[k] = s[(pc * n + 99) / 100 - 1]; }
                    }
                }
            });
            a = b;
        }
    });
    drop(latencies);

    let h_per_t = (72 + nt - 1) / nt;
    let (st, errs, d_ref, pc_ref) = (&starts, &errors, &disc, &pcts);
    let bufs: Vec<Vec<u8>> = std::thread::scope(|sc| {
        (0..nt).map(|t| {
            let (h_start, h_end) = ((t * h_per_t).min(72), ((t + 1) * h_per_t).min(72));
            sc.spawn(move || {
                let mut chunk_buf = Vec::with_capacity((h_end - h_start) * 160 * 1024);
                for h in h_start..h_end {
                    let abs = h + 18;
                    let d = 30 + abs / 24;
                    let (mo, da) = if d <= 31 { (10, d) } else { (11, d - 31) };
                    let hdr = format!("2026-{mo:02}-{da:02}T{:02}:00:00Z ", abs % 24).into_bytes();
                    for rank in 0..num_eps {
                        let gid = h * num_eps + rank;
                        let cnt = st[gid + 1] - st[gid];
                        if cnt == 0 { continue; }
                        let e = errs[gid].load(Relaxed);
                        chunk_buf.extend_from_slice(&hdr);
                        chunk_buf.extend_from_slice(d_ref[rank]);
                        for v in [cnt, pc_ref[gid][0], pc_ref[gid][1], pc_ref[gid][2], e] { chunk_buf.push(b' '); put(&mut chunk_buf, v as u64); }
                        chunk_buf.push(b' ');
                        put(&mut chunk_buf, e as u64 * 1_000_000 / cnt as u64);
                        chunk_buf.push(b'\n');
                    }
                }
                chunk_buf
            })
        }).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut out_file = File::create(&out_path).expect("create summary");
    for b in bufs { out_file.write_all(&b).unwrap(); }
}
