use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_POPULATE: i32 = 0x08000;
const MADV_SEQUENTIAL: i32 = 2;

struct MmapSlice {
    ptr: *mut u8,
    len: usize,
}
unsafe impl Send for MmapSlice {}
unsafe impl Sync for MmapSlice {}

impl MmapSlice {
    fn open(file: &File) -> std::io::Result<Self> {
        let len = file.metadata()?.size() as usize;
        if len == 0 {
            return Ok(Self {
                ptr: std::ptr::null_mut(),
                len: 0,
            });
        }
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_PRIVATE | MAP_POPULATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == usize::MAX as *mut u8 {
            return Err(std::io::Error::last_os_error());
        }
        unsafe {
            madvise(ptr, len, MADV_SEQUENTIAL);
        }
        Ok(Self { ptr, len })
    }
    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

impl Drop for MmapSlice {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe {
                munmap(self.ptr, self.len);
            }
        }
    }
}

#[derive(Clone)]
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

fn format_money(cents: i64, out: &mut String) {
    if cents < 0 {
        out.push('-');
    }
    let abs_cents = cents.unsigned_abs();
    use std::fmt::Write;
    write!(out, "{}.{:02}", abs_cents / 100, abs_cents % 100).unwrap();
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() {
            data_dir = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--out" && i + 1 < args.len() {
            out_dir = args[i + 1].clone();
            i += 2;
        } else {
            i += 1;
        }
    }

    // 1. Fast regions parser
    let regions_mmap = MmapSlice::open(&File::open(format!("{}/regions.csv", data_dir))?)?;
    let rdata = regions_mmap.as_slice();
    let rlen = rdata.len();
    let mut rp = 0;
    while rp < rlen && rdata[rp] != b'\n' {
        rp += 1;
    }
    if rp < rlen {
        rp += 1;
    }

    let mut country_codes: Vec<[u8; 3]> = Vec::with_capacity(256);
    let mut code_map = [0xFFu8; 26 * 26 * 26];
    let mut region_to_country = vec![255u8; 1_000_000];

    while rp < rlen {
        let mut rid = 0u32;
        while rp < rlen && rdata[rp] != b',' {
            rid = rid * 10 + (rdata[rp] - b'0') as u32;
            rp += 1;
        }
        rp += 1;
        if rp + 3 <= rlen {
            let hash = ((rdata[rp] - b'A') as usize) * 676
                + ((rdata[rp + 1] - b'A') as usize) * 26
                + ((rdata[rp + 2] - b'A') as usize);
            let mut cid = code_map[hash];
            if cid == 0xFF {
                cid = country_codes.len() as u8;
                country_codes.push([rdata[rp], rdata[rp + 1], rdata[rp + 2]]);
                code_map[hash] = cid;
            }
            if (rid as usize) < region_to_country.len() {
                region_to_country[rid as usize] = cid;
            }
            rp += 3;
        }
        while rp < rlen && (rdata[rp] == b'\r' || rdata[rp] == b'\n') {
            rp += 1;
        }
    }
    drop(regions_mmap);

    let unmatched_cid = country_codes.len() as u8;
    for x in &mut region_to_country {
        if *x == 255 {
            *x = unmatched_cid;
        }
    }
    let num_countries = country_codes.len() + 1;

    // 2. Map records.csv
    let records_mmap = MmapSlice::open(&File::open(format!("{}/records.csv", data_dir))?)?;
    let data = records_mmap.as_slice();
    let len = data.len();

    let mut body_start = 0;
    if data.starts_with(b"\xEF\xBB\xBF") {
        body_start = 3;
    }
    while body_start < len && data[body_start] != b'\n' {
        body_start += 1;
    }
    if body_start < len {
        body_start += 1;
    }

    const TABLE_SIZE: usize = 262_144;
    let name_table: Vec<AtomicU64> = (0..TABLE_SIZE).map(|_| AtomicU64::new(0)).collect();
    let next_id_block = AtomicU32::new(0);
    let country_bitsets: Vec<AtomicU64> = (0..num_countries * 2048)
        .map(|_| AtomicU64::new(0))
        .collect();

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(12)
        .min(12);
    let mut starts = Vec::with_capacity(num_threads + 1);
    starts.push(body_start);
    let body_len = len - body_start;
    for t in 1..num_threads {
        let mut p = body_start + (t * body_len) / num_threads;
        while p < len {
            if data[p] == b'\n' {
                let mut np = p + 1;
                if np + 2 < len && data[np] == 0xEF && data[np + 1] == 0xBB && data[np + 2] == 0xBF
                {
                    np += 3;
                }
                if np < len && data[np] >= b'0' && data[np] <= b'9' {
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
            let region_to_country = &region_to_country;
            let name_table = &name_table;
            let next_id_block = &next_id_block;
            let country_bitsets = &country_bitsets;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![
                    LocalAgg {
                        count: 0,
                        id_sum: 0,
                        amount_sum: 0,
                        amount_min: i64::MAX,
                        amount_max: i64::MIN,
                        name_bytes: 0,
                        note_bytes: 0,
                        names_distinct: 0,
                    };
                    num_countries
                ];

                let mut cur_id = 0u32;
                let mut max_id = 0u32;
                let mut p = chunk_start;

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

                    // 1. Parse id
                    let mut id = 0u64;
                    loop {
                        let b = unsafe { *data.get_unchecked(p) };
                        if b == b',' {
                            p += 1;
                            break;
                        }
                        id = id * 10 + (b - b'0') as u64;
                        p += 1;
                    }

                    // 2. Parse name
                    let mut nh: u64 = 0xcbf29ce484222325;
                    let mut name_len = 0u32;
                    let b = unsafe { *data.get_unchecked(p) };
                    if b == b'"' {
                        p += 1;
                        loop {
                            let curr = unsafe { *data.get_unchecked(p) };
                            if curr == b'"' {
                                if p + 1 < len && unsafe { *data.get_unchecked(p + 1) == b'"' } {
                                    nh = (nh ^ (b'"' as u64)).wrapping_mul(0x100000001b3);
                                    name_len += 1;
                                    p += 2;
                                } else {
                                    p += 1;
                                    break;
                                }
                            } else {
                                nh = (nh ^ (curr as u64)).wrapping_mul(0x100000001b3);
                                name_len += 1;
                                p += 1;
                            }
                        }
                        p += 1;
                    } else {
                        while unsafe { *data.get_unchecked(p) } != b',' {
                            let curr = unsafe { *data.get_unchecked(p) };
                            nh = (nh ^ (curr as u64)).wrapping_mul(0x100000001b3);
                            name_len += 1;
                            p += 1;
                        }
                        p += 1;
                    }

                    // 3. Parse note
                    let mut note_len = 0u32;
                    let b = unsafe { *data.get_unchecked(p) };
                    if b == b'"' {
                        p += 1;
                        loop {
                            let curr = unsafe { *data.get_unchecked(p) };
                            if curr == b'"' {
                                if p + 1 < len && unsafe { *data.get_unchecked(p + 1) == b'"' } {
                                    note_len += 1;
                                    p += 2;
                                } else {
                                    p += 1;
                                    break;
                                }
                            } else {
                                note_len += 1;
                                p += 1;
                            }
                        }
                        p += 1;
                    } else {
                        while unsafe { *data.get_unchecked(p) } != b',' {
                            note_len += 1;
                            p += 1;
                        }
                        p += 1;
                    }

                    // 4. Parse region_id
                    let mut rid = 0u32;
                    loop {
                        let b = unsafe { *data.get_unchecked(p) };
                        if b == b',' {
                            p += 1;
                            break;
                        }
                        rid = rid * 10 + (b - b'0') as u32;
                        p += 1;
                    }

                    // 5. Parse amount
                    let is_neg = if unsafe { *data.get_unchecked(p) } == b'-' {
                        p += 1;
                        true
                    } else {
                        false
                    };
                    let mut cents = 0i64;
                    loop {
                        let b = unsafe { *data.get_unchecked(p) };
                        if b == b'.' {
                            p += 1;
                            break;
                        }
                        cents = cents * 10 + (b - b'0') as i64;
                        p += 1;
                    }
                    cents = cents * 10 + (unsafe { *data.get_unchecked(p) } - b'0') as i64;
                    p += 1;
                    cents = cents * 10 + (unsafe { *data.get_unchecked(p) } - b'0') as i64;
                    p += 1;
                    if is_neg {
                        cents = -cents;
                    }

                    if p < len && unsafe { *data.get_unchecked(p) == b'\r' } {
                        p += 1;
                    }
                    if p < len && unsafe { *data.get_unchecked(p) == b'\n' } {
                        p += 1;
                    }

                    let cid = if (rid as usize) < region_to_country.len() {
                        unsafe { *region_to_country.get_unchecked(rid as usize) as usize }
                    } else {
                        unmatched_cid as usize
                    };

                    let agg = unsafe { local_aggs.get_unchecked_mut(cid) };
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

                    // Assign or look up name_id
                    let h47 = nh & 0x7FFFFFFFFFFF;
                    let mut slot_idx = (nh as usize) & (TABLE_SIZE - 1);
                    let nid: u32;
                    loop {
                        let slot = unsafe { name_table.get_unchecked(slot_idx) };
                        let val = slot.load(Ordering::Relaxed);
                        if val == 0 {
                            if cur_id >= max_id {
                                cur_id = next_id_block.fetch_add(512, Ordering::Relaxed);
                                max_id = cur_id + 512;
                            }
                            let cand_id = cur_id;
                            cur_id += 1;
                            let cand_val = (h47 << 17) | ((cand_id + 1) as u64);
                            match slot.compare_exchange(
                                0,
                                cand_val,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            ) {
                                Ok(_) => {
                                    nid = cand_id;
                                    break;
                                }
                                Err(actual) => {
                                    if (actual >> 17) == h47 {
                                        nid = (actual & 0x1FFFF) as u32 - 1;
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

                    // Test and set bit for this country
                    let bit_word_idx = cid * 2048 + (nid as usize >> 6);
                    let mask = 1u64 << (nid & 63);
                    let slot = unsafe { country_bitsets.get_unchecked(bit_word_idx) };
                    let val = slot.load(Ordering::Relaxed);
                    if (val & mask) == 0 {
                        let prev = slot.fetch_or(mask, Ordering::Relaxed);
                        if (prev & mask) == 0 {
                            agg.names_distinct += 1;
                        }
                    }
                }
                local_aggs
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
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
            names_distinct: 0,
        };
        num_countries
    ];

    for tagg in thread_aggs {
        for (i, item) in tagg.iter().enumerate() {
            let f = &mut final_aggs[i];
            f.count += item.count;
            f.id_sum += item.id_sum;
            f.amount_sum += item.amount_sum;
            if item.amount_min < f.amount_min {
                f.amount_min = item.amount_min;
            }
            if item.amount_max > f.amount_max {
                f.amount_max = item.amount_max;
            }
            f.name_bytes += item.name_bytes;
            f.note_bytes += item.note_bytes;
            f.names_distinct += item.names_distinct;
        }
    }

    let mut country_indices: Vec<usize> = (0..country_codes.len()).collect();
    country_indices.sort_by_key(|&idx| country_codes[idx]);

    let mut out = String::with_capacity(32 * 1024);
    for idx in country_indices {
        let agg = &final_aggs[idx];
        if agg.count > 0 {
            let code = std::str::from_utf8(&country_codes[idx]).unwrap();
            use std::fmt::Write;
            write!(&mut out, "{} {} {} ", code, agg.count, agg.id_sum).unwrap();
            format_money(agg.amount_sum, &mut out);
            out.push(' ');
            format_money(agg.amount_min, &mut out);
            out.push(' ');
            format_money(agg.amount_max, &mut out);
            writeln!(
                &mut out,
                " {} {} {}",
                agg.names_distinct, agg.name_bytes, agg.note_bytes
            )
            .unwrap();
        }
    }
    let agg = &final_aggs[unmatched_cid as usize];
    if agg.count > 0 {
        use std::fmt::Write;
        write!(&mut out, "_UNMATCHED {} {} ", agg.count, agg.id_sum).unwrap();
        format_money(agg.amount_sum, &mut out);
        out.push(' ');
        format_money(agg.amount_min, &mut out);
        out.push(' ');
        format_money(agg.amount_max, &mut out);
        writeln!(
            &mut out,
            " {} {} {}",
            agg.names_distinct, agg.name_bytes, agg.note_bytes
        )
        .unwrap();
    }

    std::fs::create_dir_all(&out_dir)?;
    let mut out_file = File::create(format!("{}/aggregates.txt", out_dir))?;
    out_file.write_all(out.as_bytes())?;
    Ok(())
}
