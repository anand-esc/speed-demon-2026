// Track A (Ingest): std-only, zero crates, two-pass zero-allocation architecture.
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

const K: u64 = 0x517c_c1b7_2722_0a95;
const OFF: [usize; 16] = [0, 0, 0, 0, 7, 0, 0, 4, 5, 0, 0, 0, 0, 0, 0, 8];

#[inline(always)]
unsafe fn fast_hash(p: *const u8, len: usize) -> u64 {
    let mut h = (len as u64).wrapping_mul(K);
    if len <= 8 {
        let mut w = 0u64;
        std::ptr::copy_nonoverlapping(p, &mut w as *mut u64 as *mut u8, len);
        return (h ^ w).wrapping_mul(K);
    }
    let first = (p as *const u64).read_unaligned();
    let last = (p.add(len - 8) as *const u64).read_unaligned();
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
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 { break; }
    }
    out.extend_from_slice(&b[i..]);
}

macro_rules! parse_req {
    ($p:ident, $sl:ident, $num_eps:expr, $gid:ident, $err:ident, $lat:ident, $next_p:ident) => {
        let d = unsafe { ((*$p.add(8) - b'0') * 10 + (*$p.add(9) - b'0')) as usize };
        let day = if unsafe { *$p.add(6) } == b'0' { d } else { 31 + d };
        let h = (day - 30) * 24 + unsafe { ((*$p.add(11) - b'0') * 10 + (*$p.add(12) - b'0')) as usize } - 18;
        let m0 = unsafe { *$p.add(28) };
        let off = if m0 == b'P' { 4 + (unsafe { *$p.add(29) } == b'O') as usize + (unsafe { *$p.add(29) } == b'A') as usize * 2 } else { OFF[(m0 & 15) as usize] };
        let es = unsafe { $p.add(28 + off) };
        let mut ep_scan = es;
        while unsafe { *ep_scan } != b' ' { ep_scan = unsafe { ep_scan.add(1) }; }
        let ep_len = unsafe { ep_scan.offset_from(es) as usize };
        let h_ep = unsafe { fast_hash(es, ep_len) } | 1;
        let mut idx = (h_ep as usize) & 8191;
        let rank = loop {
            let s = unsafe { $sl.get_unchecked(idx) };
            if s.0 == h_ep && s.2 as usize == ep_len { break s.1 as usize; }
            idx = (idx + 1) & 8191;
        };
        let $gid = h * $num_eps + rank;
        let $err = unsafe { *ep_scan.add(1) } == b'5';
        let mut p_lat = unsafe { ep_scan.add(5) };
        let mut $lat = 0u32;
        while unsafe { *p_lat } != b'\n' { $lat = $lat * 10 + (unsafe { *p_lat } - b'0') as u32; p_lat = unsafe { p_lat.add(1) }; }
        let $next_p = unsafe { p_lat.add(1) };
    };
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].clone()).or_else(|| std::env::var("DATA_DIR").ok()).unwrap_or_else(|| "/data".into());
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].clone()).or_else(|| std::env::var("OUT_DIR").ok()).unwrap_or_else(|| "/out".into());
    let in_path = format!("{data_dir}/requests.log");
    let file = File::open(&in_path).expect("open requests.log");
    let len = file.metadata().unwrap().len() as usize;
    let _ = std::fs::create_dir_all(&out_dir);
    if len == 0 { return; }

    let data: &'static [u8] = unsafe {
        let p = mmap(std::ptr::null_mut(), len, 1, 1, file.as_raw_fd(), 0);
        madvise(p, len, 2);
        std::slice::from_raw_parts(p, len)
    };
    let base = data.as_ptr();

    let nt = std::thread::available_parallelism().map_or(12, |n| n.get()).min(12);
    let mut bounds = vec![0usize];
    for t in 1..nt {
        let mut s = (len / nt * t).max(*bounds.last().unwrap());
        while s > 0 && s < len && data[s - 1] != b'\n' { s += 1; }
        bounds.push(s);
    }
    bounds.push(len);

    let (mut disc, mut dh, mut p, p_end) = (Vec::with_capacity(2048), vec![0u64; 8192], base, unsafe { base.add(len.min(6_400_000)) });
    while p < p_end {
        let m0 = unsafe { *p.add(28) };
        let off = if m0 == b'P' { 4 + (unsafe { *p.add(29) } == b'O') as usize + (unsafe { *p.add(29) } == b'A') as usize * 2 } else { OFF[(m0 & 15) as usize] };
        let es = unsafe { p.add(28 + off) };
        let mut s = es;
        while unsafe { *s } != b' ' { s = unsafe { s.add(1) }; }
        let ep = unsafe { std::slice::from_raw_parts(es, s.offset_from(es) as usize) };
        let h = unsafe { fast_hash(es, ep.len()) } | 1;
        let mut idx = (h as usize) & 8191;
        loop {
            if dh[idx] == h { break; }
            if dh[idx] == 0 { dh[idx] = h; disc.push(ep); break; }
            idx = (idx + 1) & 8191;
        }
        s = unsafe { s.add(5) };
        while unsafe { *s } != b'\n' { s = unsafe { s.add(1) }; }
        p = unsafe { s.add(1) };
    }
    disc.sort_unstable();
    let num_eps = disc.len();

    let mut slots = vec![(0u64, 0u16, 0u16); 8192];
    for (rank, &ep) in disc.iter().enumerate() {
        let h = unsafe { fast_hash(ep.as_ptr(), ep.len()) } | 1;
        let mut idx = (h as usize) & 8191;
        loop {
            if slots[idx].0 == 0 { slots[idx] = (h, rank as u16, ep.len() as u16); break; }
            idx = (idx + 1) & 8191;
        }
    }

    let total_groups = 72 * num_eps;
    let (mut all_counts, all_errors) = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt).map(|t| {
            let (start, end) = (bounds[t], bounds[t + 1]);
            let sl = &slots;
            sc.spawn(move || {
                let (mut counts, mut errors) = (vec![0u32; total_groups], vec![0u32; total_groups]);
                let (mut p, p_end) = (unsafe { data.as_ptr().add(start) }, unsafe { data.as_ptr().add(end) });
                while p < p_end {
                    parse_req!(p, sl, num_eps, gid, err, _lat, next_p);
                    unsafe { *counts.get_unchecked_mut(gid) += 1; }
                    if err { unsafe { *errors.get_unchecked_mut(gid) += 1; } }
                    p = next_p;
                }
                (counts, errors)
            })
        }).collect();
        let (mut cs, mut es) = (Vec::with_capacity(nt), Vec::with_capacity(nt));
        for h in hs { let (c, e) = h.join().unwrap(); cs.push(c); es.push(e); }
        (cs, es)
    });

    let (mut starts, mut group_errors, mut cur) = (vec![0u32; total_groups + 1], vec![0u32; total_groups], 0u32);
    for g in 0..total_groups {
        starts[g] = cur;
        let mut sum_e = 0u32;
        for t in 0..nt {
            let c = all_counts[t][g];
            all_counts[t][g] = cur;
            cur += c;
            sum_e += all_errors[t][g];
        }
        group_errors[g] = sum_e;
    }
    starts[total_groups] = cur;
    drop(all_errors);

    let mut latencies: Vec<u32> = Vec::with_capacity(cur as usize);
    unsafe { latencies.set_len(cur as usize); }
    let lp = LatPtr(latencies.as_mut_ptr());

    std::thread::scope(|sc| {
        for (t, mut pos) in all_counts.into_iter().enumerate() {
            let (start, end) = (bounds[t], bounds[t + 1]);
            let (sl, lp) = (&slots, &lp);
            sc.spawn(move || {
                let (mut p, p_end) = (unsafe { data.as_ptr().add(start) }, unsafe { data.as_ptr().add(end) });
                while p < p_end {
                    parse_req!(p, sl, num_eps, gid, _err, lat, next_p);
                    let dst = unsafe { pos.get_unchecked_mut(gid) };
                    unsafe { *lp.0.add(*dst as usize) = lat; }
                    *dst += 1;
                    p = next_p;
                }
            });
        }
    });

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

    let fmt_hour = |h: usize| -> Vec<u8> {
        let mut o = Vec::with_capacity(160 << 10);
        let (abs, d) = (h + 18, 30 + (h + 18) / 24);
        let (mo, da) = if d <= 31 { (10, d) } else { (11, d - 31) };
        let hdr = format!("2026-{mo:02}-{da:02}T{:02}:00:00Z ", abs % 24).into_bytes();
        for rank in 0..num_eps {
            let gid = h * num_eps + rank;
            let cnt = starts[gid + 1] - starts[gid];
            if cnt == 0 { continue; }
            let e = group_errors[gid];
            o.extend_from_slice(&hdr);
            o.extend_from_slice(disc[rank]);
            for v in [cnt, pcts[gid][0], pcts[gid][1], pcts[gid][2], e] {
                o.push(b' ');
                put(&mut o, v as u64);
            }
            o.push(b' ');
            put(&mut o, e as u64 * 1_000_000 / cnt as u64);
            o.push(b'\n');
        }
        o
    };

    let mut out_file = File::create(format!("{out_dir}/summary.txt")).expect("create summary.txt");
    for w in (0..72).step_by(nt) {
        let bufs: Vec<Vec<u8>> = std::thread::scope(|sc| {
            let hs: Vec<_> = (w..(w + nt).min(72)).map(|h| sc.spawn(move || fmt_hour(h))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for b in bufs { out_file.write_all(&b).unwrap(); }
    }
}
