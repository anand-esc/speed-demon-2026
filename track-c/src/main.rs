use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, ad: i32) -> i32;
}

fn mmap_file(p: &str) -> &'static [u8] {
    let f = File::open(p).unwrap();
    let l = f.metadata().unwrap().size() as usize;
    let fd = f.as_raw_fd();
    unsafe {
        let ptr = mmap(std::ptr::null_mut(), l, 1, 1, fd, 0);
        madvise(ptr, l, 2);
        std::mem::forget(f);
        std::slice::from_raw_parts(ptr, l)
    }
}

#[derive(Clone, Copy)]
struct LocalAgg {
    count: u64, id_sum: i64, amount_sum: i64, amount_min: i64, amount_max: i64,
    name_bytes: u64, note_bytes: u64, names_distinct: u32,
}
impl LocalAgg {
    const EMPTY: Self = Self { count: 0, id_sum: 0, amount_sum: 0, amount_min: i64::MAX, amount_max: i64::MIN, name_bytes: 0, note_bytes: 0, names_distinct: 0 };
    #[inline(always)]
    fn merge(&mut self, o: &Self) {
        self.count += o.count; self.id_sum += o.id_sum; self.amount_sum += o.amount_sum;
        self.amount_min = self.amount_min.min(o.amount_min); self.amount_max = self.amount_max.max(o.amount_max);
        self.name_bytes += o.name_bytes; self.note_bytes += o.note_bytes;
    }
}

#[derive(Clone, Copy)]
struct NameCacheEntry {
    hash: u64,
    nid: u32,
}

#[inline(always)]
unsafe fn parse_fast(data: &[u8], p: &mut usize, len: usize) -> (u64, u64, u32, u32, usize, i64) {
    if *p + 2 < len && *data.get_unchecked(*p) == 0xEF && *data.get_unchecked(*p + 1) == 0xBB && *data.get_unchecked(*p + 2) == 0xBF { *p += 3; }

    let mut id = 0u64;
    loop {
        let b = *data.get_unchecked(*p); *p += 1;
        if b == b',' { break; }
        id = id * 10 + (b - b'0') as u64;
    }

    let (nh, name_len) = if *data.get_unchecked(*p) == b'"' {
        *p += 1;
        let (mut h, mut nl) = (0xcbf29ce484222325u64, 0u32);
        loop {
            let b = *data.get_unchecked(*p); *p += 1;
            if b == b'"' {
                if *p < len && *data.get_unchecked(*p) == b'"' {
                    h = (h ^ (b'"' as u64)).wrapping_mul(0x100000001b3); nl += 1; *p += 1;
                } else { break; }
            } else { h = (h ^ (b as u64)).wrapping_mul(0x100000001b3); nl += 1; }
        }
        *p += 1;
        (h, nl)
    } else {
        let (mut h, mut nl) = (0xcbf29ce484222325u64, 0u32);
        loop {
            let b = *data.get_unchecked(*p); *p += 1;
            if b == b',' { break; }
            h = (h ^ (b as u64)).wrapping_mul(0x100000001b3); nl += 1;
        }
        (h, nl)
    };

    let note_len = if *data.get_unchecked(*p) == b'"' {
        *p += 1;
        let mut nl = 0u32;
        loop {
            let b = *data.get_unchecked(*p); *p += 1;
            if b == b'"' {
                if *p < len && *data.get_unchecked(*p) == b'"' { nl += 1; *p += 1; } else { break; }
            } else { nl += 1; }
        }
        *p += 1;
        nl
    } else {
        let start = *p;
        while *data.get_unchecked(*p) != b',' { *p += 1; }
        let nl = (*p - start) as u32;
        *p += 1;
        nl
    };

    let mut rid = 0usize;
    loop {
        let b = *data.get_unchecked(*p); *p += 1;
        if b == b',' { break; }
        rid = rid * 10 + (b - b'0') as usize;
    }

    let is_neg = if *data.get_unchecked(*p) == b'-' { *p += 1; true } else { false };
    let mut cents = 0i64;
    loop {
        let b = *data.get_unchecked(*p); *p += 1;
        if b == b'.' { break; }
        cents = cents * 10 + (b - b'0') as i64;
    }
    cents = cents * 100 + ((*data.get_unchecked(*p) - b'0') as i64) * 10 + (*data.get_unchecked(*p + 1) - b'0') as i64;
    *p += 2;
    if is_neg { cents = -cents; }
    if *p < len && *data.get_unchecked(*p) == b'\r' { *p += 1; }
    if *p < len && *data.get_unchecked(*p) == b'\n' { *p += 1; }

    (id, nh, name_len, note_len, rid, cents)
}

fn write_money(c: i64, out: &mut String) {
    if c < 0 { out.push('-'); }
    let a = c.unsigned_abs();
    use std::fmt::Write;
    let _ = write!(out, "{}.{:02}", a / 100, a % 100);
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].as_str()).unwrap_or("/data");
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].as_str()).unwrap_or("/out");

    let rdata = mmap_file(&format!("{}/regions.csv", data_dir));
    let (rlen, mut rp) = (rdata.len(), rdata.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1));
    let (mut country_codes, mut code_map, mut region_to_country) = (Vec::<[u8; 3]>::with_capacity(256), [0xFFu8; 17576], vec![255u8; 1_000_000]);

    while rp < rlen {
        let mut rid = 0u32;
        while rp < rlen && unsafe { *rdata.get_unchecked(rp) } != b',' {
            rid = rid * 10 + (unsafe { *rdata.get_unchecked(rp) } - b'0') as u32;
            rp += 1;
        }
        rp += 1;
        if rp + 3 <= rlen {
            let h = ((unsafe { *rdata.get_unchecked(rp) } - b'A') as usize) * 676
                + ((unsafe { *rdata.get_unchecked(rp + 1) } - b'A') as usize) * 26
                + ((unsafe { *rdata.get_unchecked(rp + 2) } - b'A') as usize);
            let mut cid = code_map[h];
            if cid == 0xFF {
                cid = country_codes.len() as u8;
                country_codes.push([rdata[rp], rdata[rp + 1], rdata[rp + 2]]);
                code_map[h] = cid;
            }
            if (rid as usize) < 1_000_000 { unsafe { *region_to_country.get_unchecked_mut(rid as usize) = cid; } }
            rp += 3;
        }
        while rp < rlen && unsafe { *rdata.get_unchecked(rp) } <= b' ' { rp += 1; }
    }

    let (unmatched_cid, num_countries) = (country_codes.len() as u8, country_codes.len() + 1);

    let data = mmap_file(&format!("{}/records.csv", data_dir));
    let len = data.len();
    let body_start = if data.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
    let body_start = data[body_start..].iter().position(|&b| b == b'\n').map_or(len, |i| body_start + i + 1);

    const TABLE_SIZE: usize = 262_144;
    let name_table: Vec<AtomicU64> = unsafe { std::mem::transmute(vec![0u64; TABLE_SIZE]) };
    let country_bitsets: Vec<AtomicU64> = unsafe { std::mem::transmute(vec![0u64; num_countries * 2048]) };
    let next_id_block = AtomicU32::new(0);

    let num_threads = std::thread::available_parallelism().map_or(12, |n| n.get()).min(12);
    let mut starts = Vec::with_capacity(num_threads + 1);
    starts.push(body_start);
    let blen = len - body_start;
    for t in 1..num_threads {
        let mut p = body_start + (t * blen) / num_threads;
        let mut found = false;
        while p < len {
            if data[p] == b'\n' {
                let mut np = p + 1;
                if np + 2 < len && data[np] == 0xEF && data[np + 1] == 0xBB && data[np + 2] == 0xBF { np += 3; }
                if np < len && data[np].is_ascii_digit() { starts.push(p + 1); found = true; break; }
            }
            p += 1;
        }
        if !found { starts.push(len); }
    }
    starts.push(len);

    let thread_aggs = std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for t in 0..num_threads {
            let (chunk_start, chunk_end) = (starts[t], starts[t + 1]);
            let (r2c, n_tbl, n_blk, c_bits) = (&region_to_country, &name_table, &next_id_block, &country_bitsets);

            handles.push(s.spawn(move || {
                let mut aggs = [LocalAgg::EMPTY; 256];
                let mut n_cache = [NameCacheEntry { hash: 0, nid: 0 }; 4096];
                let (mut cur_id, mut max_id, mut p) = (0u32, 0u32, chunk_start);

                while p < chunk_end {
                    let (id, nh, name_len, note_len, rid, cents) = unsafe { parse_fast(data, &mut p, len) };

                    let mut cid = if rid < 1_000_000 { unsafe { *r2c.get_unchecked(rid) as usize } } else { unmatched_cid as usize };
                    if cid == 255 { cid = unmatched_cid as usize; }
                    let agg = &mut aggs[cid];
                    agg.count += 1; agg.id_sum += id as i64; agg.amount_sum += cents;
                    agg.amount_min = agg.amount_min.min(cents); agg.amount_max = agg.amount_max.max(cents);
                    agg.name_bytes += name_len as u64; agg.note_bytes += note_len as u64;

                    let c_idx = (nh as usize) & 4095;
                    let c_entry = unsafe { n_cache.get_unchecked(c_idx) };
                    let chosen_nid = if c_entry.hash == nh && c_entry.nid != 0 {
                        c_entry.nid - 1
                    } else {
                        let (h47, mut slot_idx) = (nh & 0x7FFFFFFFFFFF, (nh as usize) & (TABLE_SIZE - 1));
                        let nid: u32;
                        loop {
                            let slot = unsafe { n_tbl.get_unchecked(slot_idx) };
                            let val = slot.load(Ordering::Relaxed);
                            if val == 0 {
                                if cur_id >= max_id { cur_id = n_blk.fetch_add(512, Ordering::Relaxed); max_id = cur_id + 512; }
                                let cand_id = cur_id;
                                cur_id += 1;
                                match slot.compare_exchange(0, (h47 << 17) | ((cand_id + 1) as u64), Ordering::Relaxed, Ordering::Relaxed) {
                                    Ok(_) => { nid = cand_id; break; }
                                    Err(act) => { if (act >> 17) == h47 { nid = (act & 0x1FFFF) as u32 - 1; break; } }
                                }
                            } else if (val >> 17) == h47 { nid = (val & 0x1FFFF) as u32 - 1; break; }
                            slot_idx = (slot_idx + 1) & (TABLE_SIZE - 1);
                        }
                        unsafe { *n_cache.get_unchecked_mut(c_idx) = NameCacheEntry { hash: nh, nid: nid + 1 }; }
                        nid
                    };

                    let slot = unsafe { c_bits.get_unchecked((cid << 11) + (chosen_nid as usize >> 6)) };
                    let mask = 1u64 << (chosen_nid & 63);
                    if (slot.load(Ordering::Relaxed) & mask) == 0 {
                        slot.fetch_or(mask, Ordering::Relaxed);
                    }
                }
                aggs
            }));
        }
        let mut res = Vec::with_capacity(num_threads);
        for h in handles { res.push(h.join().unwrap()); }
        res
    });

    let mut final_aggs = [LocalAgg::EMPTY; 256];
    for tagg in thread_aggs {
        for (i, it) in tagg.iter().enumerate() { final_aggs[i].merge(it); }
    }

    for c in 0..num_countries {
        let base = c << 11;
        let mut distinct = 0u32;
        for i in 0..2048 {
            distinct += country_bitsets[base + i].load(Ordering::Relaxed).count_ones();
        }
        final_aggs[c].names_distinct = distinct;
    }

    let mut order: Vec<usize> = (0..country_codes.len()).collect();
    order.sort_by_key(|&i| country_codes[i]);
    order.push(unmatched_cid as usize);

    let mut out = String::with_capacity(32 * 1024);
    for idx in order {
        let agg = &final_aggs[idx];
        if agg.count > 0 {
            let name = if idx < country_codes.len() { std::str::from_utf8(&country_codes[idx]).unwrap() } else { "_UNMATCHED" };
            use std::fmt::Write;
            let _ = write!(&mut out, "{} {} {} ", name, agg.count, agg.id_sum);
            write_money(agg.amount_sum, &mut out); out.push(' ');
            write_money(agg.amount_min, &mut out); out.push(' ');
            write_money(agg.amount_max, &mut out);
            let _ = writeln!(&mut out, " {} {} {}", agg.names_distinct, agg.name_bytes, agg.note_bytes);
        }
    }

    let _ = std::fs::create_dir_all(out_dir);
    File::create(format!("{}/aggregates.txt", out_dir))?.write_all(out.as_bytes())?;
    std::process::exit(0);
}
