use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, ad: i32) -> i32;
}

struct MmapSlice(*mut u8, usize);
unsafe impl Send for MmapSlice {}
unsafe impl Sync for MmapSlice {}

impl MmapSlice {
    fn open(p: &str) -> std::io::Result<Self> {
        let f = File::open(p)?;
        let l = f.metadata()?.size() as usize;
        let ptr = unsafe { mmap(std::ptr::null_mut(), l, 1, 0x8002, f.as_raw_fd(), 0) };
        if ptr == usize::MAX as *mut u8 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe {
            madvise(ptr, l, 2);
        }
        Ok(Self(ptr, l))
    }
    #[inline(always)]
    fn slice(&self) -> &[u8] {
        if self.1 == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.0, self.1) }
        }
    }
}

impl Drop for MmapSlice {
    fn drop(&mut self) {
        if !self.0.is_null() && self.1 > 0 {
            unsafe {
                munmap(self.0, self.1);
            }
        }
    }
}

#[derive(Clone, Default)]
struct LocalAgg {
    count: u64,
    id_sum: i64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: u64,
    note_bytes: u64,
    names_distinct: u32,
}

#[inline(always)]
fn parse_field(data: &[u8], p: &mut usize) -> (u64, u32) {
    let mut h = 0xcbf29ce484222325u64;
    let mut len = 0u32;
    if unsafe { *data.get_unchecked(*p) } == b'"' {
        *p += 1;
        loop {
            let b = unsafe { *data.get_unchecked(*p) };
            if b == b'"' {
                if *p + 1 < data.len() && unsafe { *data.get_unchecked(*p + 1) == b'"' } {
                    h = (h ^ (b'"' as u64)).wrapping_mul(0x100000001b3);
                    len += 1;
                    *p += 2;
                } else {
                    *p += 1;
                    break;
                }
            } else {
                h = (h ^ (b as u64)).wrapping_mul(0x100000001b3);
                len += 1;
                *p += 1;
            }
        }
        *p += 1;
    } else {
        loop {
            let b = unsafe { *data.get_unchecked(*p) };
            *p += 1;
            if b == b',' {
                break;
            }
            h = (h ^ (b as u64)).wrapping_mul(0x100000001b3);
            len += 1;
        }
    }
    (h, len)
}

fn write_money(cents: i64, out: &mut String) {
    if cents < 0 {
        out.push('-');
    }
    let a = cents.unsigned_abs();
    use std::fmt::Write;
    write!(out, "{}.{:02}", a / 100, a % 100).unwrap();
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args
        .windows(2)
        .find(|w| w[0] == "--data")
        .map(|w| w[1].as_str())
        .unwrap_or("/data");
    let out_dir = args
        .windows(2)
        .find(|w| w[0] == "--out")
        .map(|w| w[1].as_str())
        .unwrap_or("/out");

    let r_mmap = MmapSlice::open(&format!("{}/regions.csv", data_dir))?;
    let rdata = r_mmap.slice();
    let rlen = rdata.len();
    let mut rp = rdata.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);

    let mut country_codes: Vec<[u8; 3]> = Vec::with_capacity(256);
    let mut code_map = [0xFFu8; 17576];
    let mut region_to_country = vec![255u8; 1_000_000];

    while rp < rlen {
        let mut rid = 0u32;
        while rp < rlen && rdata[rp] != b',' {
            rid = rid * 10 + (rdata[rp] - b'0') as u32;
            rp += 1;
        }
        rp += 1;
        if rp + 3 <= rlen {
            let h = ((rdata[rp] - b'A') as usize) * 676
                + ((rdata[rp + 1] - b'A') as usize) * 26
                + ((rdata[rp + 2] - b'A') as usize);
            let mut cid = code_map[h];
            if cid == 0xFF {
                cid = country_codes.len() as u8;
                country_codes.push([rdata[rp], rdata[rp + 1], rdata[rp + 2]]);
                code_map[h] = cid;
            }
            if (rid as usize) < 1_000_000 {
                region_to_country[rid as usize] = cid;
            }
            rp += 3;
        }
        while rp < rlen && (rdata[rp] == b'\r' || rdata[rp] == b'\n') {
            rp += 1;
        }
    }
    drop(r_mmap);

    let unmatched_cid = country_codes.len() as u8;
    for x in &mut region_to_country {
        if *x == 255 {
            *x = unmatched_cid;
        }
    }
    let num_countries = country_codes.len() + 1;

    let rec_mmap = MmapSlice::open(&format!("{}/records.csv", data_dir))?;
    let data = rec_mmap.slice();
    let len = data.len();
    let mut body_start = if data.starts_with(b"\xEF\xBB\xBF") {
        3
    } else {
        0
    };
    body_start = data[body_start..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(len, |i| body_start + i + 1);

    const TABLE_SIZE: usize = 262_144;
    let table: Vec<u64> = vec![0u64; TABLE_SIZE];
    let name_table: Vec<AtomicU64> = unsafe { std::mem::transmute(table) };
    let next_id_block = AtomicU32::new(0);
    let b_table: Vec<u64> = vec![0u64; num_countries * 2048];
    let country_bitsets: Vec<AtomicU64> = unsafe { std::mem::transmute(b_table) };

    let num_threads = thread::available_parallelism()
        .map_or(12, |n| n.get())
        .min(12);
    let mut starts = Vec::with_capacity(num_threads + 1);
    starts.push(body_start);
    let blen = len - body_start;
    for t in 1..num_threads {
        let mut p = body_start + (t * blen) / num_threads;
        while p < len {
            if data[p] == b'\n' {
                let mut np = p + 1;
                if np + 2 < len && data[np] == 0xEF && data[np + 1] == 0xBB && data[np + 2] == 0xBF
                {
                    np += 3;
                }
                if np < len && data[np].is_ascii_digit() {
                    starts.push(p + 1);
                    break;
                }
            }
            p += 1;
        }
    }
    starts.push(len);

    let thread_aggs = thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for t in 0..num_threads {
            let chunk_start = starts[t];
            let chunk_end = starts[t + 1];
            let (r2c, n_tbl, n_blk, c_bits) = (
                &region_to_country,
                &name_table,
                &next_id_block,
                &country_bitsets,
            );

            handles.push(s.spawn(move || {
                let mut aggs = vec![
                    LocalAgg {
                        amount_min: i64::MAX,
                        amount_max: i64::MIN,
                        ..Default::default()
                    };
                    num_countries
                ];
                let (mut cur_id, mut max_id, mut p) = (0u32, 0u32, chunk_start);

                while p < chunk_end {
                    if p + 2 < len
                        && unsafe {
                            *data.get_unchecked(p) == 0xEF
                                && *data.get_unchecked(p + 1) == 0xBB
                                && *data.get_unchecked(p + 2) == 0xBF
                        }
                    {
                        p += 3;
                    }
                    let mut id = 0u64;
                    loop {
                        let b = unsafe { *data.get_unchecked(p) };
                        p += 1;
                        if b == b',' {
                            break;
                        }
                        id = id * 10 + (b - b'0') as u64;
                    }

                    let (nh, name_len) = parse_field(data, &mut p);
                    let (_, note_len) = parse_field(data, &mut p);

                    let mut rid = 0u32;
                    loop {
                        let b = unsafe { *data.get_unchecked(p) };
                        p += 1;
                        if b == b',' {
                            break;
                        }
                        rid = rid * 10 + (b - b'0') as u32;
                    }

                    let is_neg = if unsafe { *data.get_unchecked(p) } == b'-' {
                        p += 1;
                        true
                    } else {
                        false
                    };
                    let mut cents = 0i64;
                    loop {
                        let b = unsafe { *data.get_unchecked(p) };
                        p += 1;
                        if b == b'.' {
                            break;
                        }
                        cents = cents * 10 + (b - b'0') as i64;
                    }
                    cents = cents * 100
                        + ((unsafe { *data.get_unchecked(p) } - b'0') as i64) * 10
                        + (unsafe { *data.get_unchecked(p + 1) } - b'0') as i64;
                    p += 2;
                    if is_neg {
                        cents = -cents;
                    }
                    if p < len && unsafe { *data.get_unchecked(p) == b'\r' } {
                        p += 1;
                    }
                    if p < len && unsafe { *data.get_unchecked(p) == b'\n' } {
                        p += 1;
                    }

                    let cid = if (rid as usize) < 1_000_000 {
                        unsafe { *r2c.get_unchecked(rid as usize) as usize }
                    } else {
                        unmatched_cid as usize
                    };
                    let agg = unsafe { aggs.get_unchecked_mut(cid) };
                    agg.count += 1;
                    agg.id_sum += id as i64;
                    agg.amount_sum += cents;
                    if cents < agg.amount_min {
                        agg.amount_min = cents;
                    }
                    if cents > agg.amount_max {
                        agg.amount_max = cents;
                    }
                    agg.name_bytes += name_len as u64;
                    agg.note_bytes += note_len as u64;

                    let h47 = nh & 0x7FFFFFFFFFFF;
                    let mut slot_idx = (nh as usize) & (TABLE_SIZE - 1);
                    let nid: u32;
                    loop {
                        let slot = unsafe { n_tbl.get_unchecked(slot_idx) };
                        let val = slot.load(Ordering::Relaxed);
                        if val == 0 {
                            if cur_id >= max_id {
                                cur_id = n_blk.fetch_add(512, Ordering::Relaxed);
                                max_id = cur_id + 512;
                            }
                            let cand_id = cur_id;
                            cur_id += 1;
                            match slot.compare_exchange(
                                0,
                                (h47 << 17) | ((cand_id + 1) as u64),
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            ) {
                                Ok(_) => {
                                    nid = cand_id;
                                    break;
                                }
                                Err(act) => {
                                    if (act >> 17) == h47 {
                                        nid = (act & 0x1FFFF) as u32 - 1;
                                        break;
                                    }
                                }
                            }
                        } else if (val >> 17) == h47 {
                            nid = (val & 0x1FFFF) as u32 - 1;
                            break;
                        }
                        slot_idx = (slot_idx + 1) & (TABLE_SIZE - 1);
                    }

                    let slot = unsafe { c_bits.get_unchecked(cid * 2048 + (nid as usize >> 6)) };
                    let mask = 1u64 << (nid & 63);
                    if (slot.load(Ordering::Relaxed) & mask) == 0
                        && (slot.fetch_or(mask, Ordering::Relaxed) & mask) == 0
                    {
                        agg.names_distinct += 1;
                    }
                }
                aggs
            }));
        }
        let mut res = Vec::with_capacity(num_threads);
        for h in handles {
            res.push(h.join().unwrap());
        }
        res
    });

    let mut final_aggs = vec![
        LocalAgg {
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            ..Default::default()
        };
        num_countries
    ];
    for tagg in thread_aggs {
        for (i, it) in tagg.iter().enumerate() {
            let f = &mut final_aggs[i];
            f.count += it.count;
            f.id_sum += it.id_sum;
            f.amount_sum += it.amount_sum;
            if it.amount_min < f.amount_min {
                f.amount_min = it.amount_min;
            }
            if it.amount_max > f.amount_max {
                f.amount_max = it.amount_max;
            }
            f.name_bytes += it.name_bytes;
            f.note_bytes += it.note_bytes;
            f.names_distinct += it.names_distinct;
        }
    }

    let mut order: Vec<usize> = (0..country_codes.len()).collect();
    order.sort_by_key(|&i| country_codes[i]);
    order.push(unmatched_cid as usize);

    let mut out = String::with_capacity(32 * 1024);
    for idx in order {
        let agg = &final_aggs[idx];
        if agg.count > 0 {
            let name = if idx < country_codes.len() {
                std::str::from_utf8(&country_codes[idx]).unwrap()
            } else {
                "_UNMATCHED"
            };
            use std::fmt::Write;
            write!(&mut out, "{} {} {} ", name, agg.count, agg.id_sum).unwrap();
            write_money(agg.amount_sum, &mut out);
            out.push(' ');
            write_money(agg.amount_min, &mut out);
            out.push(' ');
            write_money(agg.amount_max, &mut out);
            writeln!(
                &mut out,
                " {} {} {}",
                agg.names_distinct, agg.name_bytes, agg.note_bytes
            )
            .unwrap();
        }
    }

    std::fs::create_dir_all(out_dir)?;
    File::create(format!("{}/aggregates.txt", out_dir))?.write_all(out.as_bytes())?;
    Ok(())
}
