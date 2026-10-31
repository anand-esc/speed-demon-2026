use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(unix)]
extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, ad: i32) -> i32;
}

#[cfg(unix)]
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

#[cfg(not(unix))]
fn mmap_file(p: &str) -> &'static [u8] {
    Box::leak(std::fs::read(p).unwrap().into_boxed_slice())
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
    let rlen = rdata.len();
    let rp = rdata.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut country_codes = Vec::<[u8; 3]>::with_capacity(256);
    country_codes.push(*b"_UN"); // 0 is _UNMATCHED
    let mut code_map = [0u8; 17576];
    let mut region_to_country = vec![0u8; 1_000_000];

    unsafe {
        let mut p = rdata.as_ptr().add(rp);
        let end = rdata.as_ptr().add(rlen);
        while p < end {
            let mut rid = (*p - b'0') as usize; p = p.add(1);
            while *p != b',' {
                rid = rid * 10 + (*p - b'0') as usize;
                p = p.add(1);
            }
            p = p.add(1);
            let c0 = *p; let c1 = *p.add(1); let c2 = *p.add(2);
            p = p.add(4); // 3 letters + '\n'
            let h = ((c0 - b'A') as usize) * 676 + ((c1 - b'A') as usize) * 26 + ((c2 - b'A') as usize);
            let mut cid = code_map[h];
            if cid == 0 {
                cid = country_codes.len() as u8;
                country_codes.push([c0, c1, c2]);
                code_map[h] = cid;
            }
            if rid < 1_000_000 {
                *region_to_country.get_unchecked_mut(rid) = cid;
            }
        }
    }

    let num_countries = country_codes.len();

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
                if np < len && data[np] == 0xEF { np += 3; }
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
                let (mut cur_id, mut max_id) = (0u32, 0u32);
                let mut ptr = unsafe { data.as_ptr().add(chunk_start) };
                let end_ptr = unsafe { data.as_ptr().add(chunk_end) };

                unsafe {
                    while ptr < end_ptr {
                        if *ptr == 0xEF { ptr = ptr.add(3); }

                        let mut id = (*ptr - b'0') as u64; ptr = ptr.add(1);
                        while *ptr != b',' {
                            id = id * 10 + (*ptr - b'0') as u64;
                            ptr = ptr.add(1);
                        }
                        ptr = ptr.add(1);

                        let (nh, name_len) = if *ptr == b'"' {
                            ptr = ptr.add(1);
                            let (mut h, mut nl) = (0xcbf29ce484222325u64, 0u32);
                            loop {
                                let b = *ptr; ptr = ptr.add(1);
                                if b == b'"' {
                                    if *ptr == b'"' {
                                        h = (h ^ (b'"' as u64)).wrapping_mul(0x100000001b3); nl += 1; ptr = ptr.add(1);
                                    } else { break; }
                                } else {
                                    h = (h ^ (b as u64)).wrapping_mul(0x100000001b3); nl += 1;
                                }
                            }
                            ptr = ptr.add(1);
                            (h, nl)
                        } else {
                            ptr = ptr.add(1);
                            (0xcbf29ce484222325u64, 0u32)
                        };

                        let note_len = if *ptr == b'"' {
                            ptr = ptr.add(1);
                            let mut nl = 0u32;
                            loop {
                                let b = *ptr; ptr = ptr.add(1);
                                if b == b'"' {
                                    if *ptr == b'"' { nl += 1; ptr = ptr.add(1); } else { break; }
                                } else {
                                    nl += 1;
                                }
                            }
                            ptr = ptr.add(1);
                            nl
                        } else {
                            ptr = ptr.add(1);
                            0u32
                        };

                        let mut rid = (*ptr - b'0') as usize; ptr = ptr.add(1);
                        while *ptr != b',' {
                            rid = rid * 10 + (*ptr - b'0') as usize;
                            ptr = ptr.add(1);
                        }
                        ptr = ptr.add(1);

                        let is_neg = if *ptr == b'-' { ptr = ptr.add(1); true } else { false };
                        let mut cents = (*ptr - b'0') as i64; ptr = ptr.add(1);
                        while *ptr != b'.' {
                            cents = cents * 10 + (*ptr - b'0') as i64;
                            ptr = ptr.add(1);
                        }
                        let u16_val = (ptr.add(1) as *const u16).read_unaligned();
                        let frac = ((u16_val & 0xFF) - 0x30) as i64 * 10 + ((u16_val >> 8) - 0x30) as i64;
                        cents = cents * 100 + frac;
                        ptr = ptr.add(3);
                        if is_neg { cents = -cents; }
                        if *ptr == b'\r' { ptr = ptr.add(1); }
                        if ptr < end_ptr && *ptr == b'\n' { ptr = ptr.add(1); }

                        let cid = if rid < 1_000_000 { *r2c.get_unchecked(rid) as usize } else { 0 };
                        let agg = aggs.get_unchecked_mut(cid);
                        agg.count += 1; agg.id_sum += id as i64; agg.amount_sum += cents;
                        agg.amount_min = agg.amount_min.min(cents); agg.amount_max = agg.amount_max.max(cents);
                        agg.name_bytes += name_len as u64; agg.note_bytes += note_len as u64;

                        let (h47, mut slot_idx) = (nh & 0x7FFFFFFFFFFF, (nh as usize) & (TABLE_SIZE - 1));
                        let chosen_nid: u32;
                        loop {
                            let slot = n_tbl.get_unchecked(slot_idx);
                            let val = slot.load(Ordering::Relaxed);
                            if (val >> 17) == h47 {
                                chosen_nid = (val & 0x1FFFF) as u32 - 1;
                                break;
                            }
                            if val == 0 {
                                if cur_id >= max_id { cur_id = n_blk.fetch_add(512, Ordering::Relaxed); max_id = cur_id + 512; }
                                let cand_id = cur_id;
                                cur_id += 1;
                                match slot.compare_exchange(0, (h47 << 17) | ((cand_id + 1) as u64), Ordering::Relaxed, Ordering::Relaxed) {
                                    Ok(_) => { chosen_nid = cand_id; break; }
                                    Err(act) => { if (act >> 17) == h47 { chosen_nid = (act & 0x1FFFF) as u32 - 1; break; } }
                                }
                            }
                            slot_idx = (slot_idx + 1) & (TABLE_SIZE - 1);
                        }

                        let slot = c_bits.get_unchecked((cid << 11) | (chosen_nid as usize >> 6));
                        let mask = 1u64 << (chosen_nid & 63);
                        if (slot.load(Ordering::Relaxed) & mask) == 0 {
                            slot.fetch_or(mask, Ordering::Relaxed);
                        }
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

    let max_words = ((next_id_block.load(Ordering::Relaxed) as usize + 63) / 64).min(2048);
    for c in 0..num_countries {
        if final_aggs[c].count == 0 { continue; }
        let base = c << 11;
        let mut distinct = 0u32;
        for i in 0..max_words {
            distinct += country_bitsets[base + i].load(Ordering::Relaxed).count_ones();
        }
        final_aggs[c].names_distinct = distinct;
    }

    let mut order: Vec<usize> = (1..country_codes.len()).collect();
    order.sort_by_key(|&i| country_codes[i]);
    order.push(0);

    let mut out = String::with_capacity(32 * 1024);
    for idx in order {
        let agg = &final_aggs[idx];
        if agg.count > 0 {
            let name = if idx > 0 { std::str::from_utf8(&country_codes[idx]).unwrap() } else { "_UNMATCHED" };
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
