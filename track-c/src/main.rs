use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, length: usize) -> i32;
    fn madvise(addr: *mut u8, length: usize, advice: i32) -> i32;
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

struct NameSet {
    table: Vec<AtomicU64>,
    mask: usize,
}

impl NameSet {
    fn new(size: usize) -> Self {
        let mut table = Vec::with_capacity(size);
        for _ in 0..size {
            table.push(AtomicU64::new(0));
        }
        Self {
            table,
            mask: size - 1,
        }
    }

    #[inline(always)]
    fn insert(&self, country_id: u8, name_hash: u64) -> bool {
        let key = (((country_id as u64) + 1) << 56) | (name_hash & 0x00FFFFFFFFFFFFFF);
        let mut h = name_hash ^ ((country_id as u64).wrapping_mul(0x517cc1b727220a95));
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51afd7ed558ccd);
        let mut idx = (h as usize) & self.mask;

        loop {
            let slot = unsafe { self.table.get_unchecked(idx) };
            let val = slot.load(Ordering::Relaxed);
            if val == key {
                return false;
            }
            if val == 0 {
                match slot.compare_exchange_weak(0, key, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => return true,
                    Err(actual) => {
                        if actual == key {
                            return false;
                        }
                    }
                }
            }
            idx = (idx + 1) & self.mask;
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

impl LocalAgg {
    fn new() -> Self {
        Self {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
            names_distinct: 0,
        }
    }

    fn merge(&mut self, other: &LocalAgg) {
        if other.count == 0 {
            return;
        }
        self.count += other.count;
        self.id_sum += other.id_sum;
        self.amount_sum += other.amount_sum;
        if other.amount_min < self.amount_min {
            self.amount_min = other.amount_min;
        }
        if other.amount_max > self.amount_max {
            self.amount_max = other.amount_max;
        }
        self.name_bytes += other.name_bytes;
        self.note_bytes += other.note_bytes;
        self.names_distinct += other.names_distinct;
    }
}

fn format_money(cents: i64, out: &mut String) {
    if cents < 0 {
        out.push('-');
    }
    let abs_cents = cents.unsigned_abs();
    let whole = abs_cents / 100;
    let frac = abs_cents % 100;
    use std::fmt::Write;
    write!(out, "{}.{:02}", whole, frac).unwrap();
}

fn find_record_starts(data: &[u8], body_start: usize, num_chunks: usize) -> Vec<usize> {
    let mut starts = Vec::with_capacity(num_chunks + 1);
    starts.push(body_start);
    let total_len = data.len();
    let body_len = total_len - body_start;

    for i in 1..num_chunks {
        let approx = body_start + (i * body_len) / num_chunks;
        let mut p = approx;
        while p < total_len {
            if data[p] == b'\n' {
                let mut next_p = p + 1;
                if next_p + 2 < total_len
                    && data[next_p] == 0xEF
                    && data[next_p + 1] == 0xBB
                    && data[next_p + 2] == 0xBF
                {
                    next_p += 3;
                }
                if next_p < total_len && data[next_p] >= b'0' && data[next_p] <= b'9' {
                    starts.push(p + 1);
                    break;
                }
            }
            p += 1;
        }
    }
    starts.push(total_len);
    starts
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

    let records_path = format!("{}/records.csv", data_dir);
    let regions_path = format!("{}/regions.csv", data_dir);
    let out_path = format!("{}/aggregates.txt", out_dir);

    // 1. Read and parse regions.csv
    let regions_file = File::open(&regions_path)?;
    let regions_mmap = MmapSlice::open(&regions_file)?;
    let regions_data = regions_mmap.as_slice();

    let mut rp = 0;
    let rlen = regions_data.len();
    while rp < rlen && regions_data[rp] != b'\n' {
        rp += 1;
    }
    if rp < rlen && regions_data[rp] == b'\n' {
        rp += 1;
    }

    let mut parsed_regions: Vec<(u32, [u8; 3])> = Vec::with_capacity(200_000);
    let mut country_set: std::collections::BTreeSet<[u8; 3]> = std::collections::BTreeSet::new();
    let mut max_region_id: u32 = 0;

    while rp < rlen {
        let mut rid: u32 = 0;
        while rp < rlen && regions_data[rp] != b',' {
            rid = rid * 10 + (regions_data[rp] - b'0') as u32;
            rp += 1;
        }
        rp += 1;
        if rp + 3 <= rlen {
            let code = [regions_data[rp], regions_data[rp + 1], regions_data[rp + 2]];
            country_set.insert(code);
            parsed_regions.push((rid, code));
            if rid > max_region_id {
                max_region_id = rid;
            }
            rp += 3;
        }
        if rp < rlen && regions_data[rp] == b'\r' {
            rp += 1;
        }
        if rp < rlen && regions_data[rp] == b'\n' {
            rp += 1;
        }
    }
    drop(regions_mmap);
    drop(regions_file);

    let mut countries_list: Vec<String> = country_set
        .into_iter()
        .map(|c| String::from_utf8(c.to_vec()).unwrap())
        .collect();
    let unmatched_idx = countries_list.len();
    countries_list.push("_UNMATCHED".to_string());
    let num_countries = countries_list.len();

    let mut code_to_idx = std::collections::HashMap::new();
    for (idx, code_str) in countries_list.iter().enumerate() {
        code_to_idx.insert(code_str.clone(), idx);
    }

    let mut region_to_country: Vec<u8> = vec![unmatched_idx as u8; (max_region_id + 1) as usize];
    for (rid, code) in parsed_regions {
        let code_str = std::str::from_utf8(&code).unwrap();
        let idx = code_to_idx[code_str];
        region_to_country[rid as usize] = idx as u8;
    }

    // 2. Open records.csv
    let records_file = File::open(&records_path)?;
    let records_mmap = MmapSlice::open(&records_file)?;
    let data = records_mmap.as_slice();
    let len = data.len();

    let mut body_start = 0;
    if len >= 3 && data[0] == 0xEF && data[1] == 0xBB && data[2] == 0xBF {
        body_start = 3;
    }
    while body_start < len && data[body_start] != b'\n' {
        body_start += 1;
    }
    if body_start < len && data[body_start] == b'\n' {
        body_start += 1;
    }

    let num_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(12)
        .min(12);
    let starts = find_record_starts(data, body_start, num_threads);

    let name_set = NameSet::new(8_388_608);

    let thread_aggs = thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_threads);
        for t in 0..num_threads {
            let chunk_start = starts[t];
            let chunk_end = starts[t + 1];
            let region_to_country = &region_to_country;
            let name_set = &name_set;

            handles.push(s.spawn(move || {
                let mut local_aggs = vec![LocalAgg::new(); num_countries];
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
                    let mut id: u64 = 0;
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
                    let mut name_hash: u64 = 0xcbf29ce484222325;
                    let mut name_len: u32 = 0;
                    let b = unsafe { *data.get_unchecked(p) };
                    if b == b'"' {
                        p += 1;
                        loop {
                            let curr = unsafe { *data.get_unchecked(p) };
                            if curr == b'"' {
                                if p + 1 < len && unsafe { *data.get_unchecked(p + 1) == b'"' } {
                                    name_hash =
                                        (name_hash ^ (b'"' as u64)).wrapping_mul(0x100000001b3);
                                    name_len += 1;
                                    p += 2;
                                } else {
                                    p += 1;
                                    break;
                                }
                            } else {
                                name_hash = (name_hash ^ (curr as u64)).wrapping_mul(0x100000001b3);
                                name_len += 1;
                                p += 1;
                            }
                        }
                        p += 1;
                    } else {
                        p += 1;
                    }

                    // 3. Parse note
                    let mut note_len: u32 = 0;
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
                        p += 1;
                    }

                    // 4. Parse region_id
                    let mut region_id: u32 = 0;
                    loop {
                        let b = unsafe { *data.get_unchecked(p) };
                        if b == b',' {
                            p += 1;
                            break;
                        }
                        region_id = region_id * 10 + (b - b'0') as u32;
                        p += 1;
                    }

                    // 5. Parse amount
                    let is_neg = if unsafe { *data.get_unchecked(p) } == b'-' {
                        p += 1;
                        true
                    } else {
                        false
                    };

                    let mut cents: i64 = 0;
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

                    // Aggregation
                    let country_id = if (region_id as usize) < region_to_country.len() {
                        unsafe { *region_to_country.get_unchecked(region_id as usize) as usize }
                    } else {
                        unmatched_idx
                    };

                    let agg = unsafe { local_aggs.get_unchecked_mut(country_id) };
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

                    if name_set.insert(country_id as u8, name_hash) {
                        agg.names_distinct += 1;
                    }
                }
                local_aggs
            }));
        }

        let mut all_results = Vec::with_capacity(num_threads);
        for h in handles {
            all_results.push(h.join().unwrap());
        }
        all_results
    });

    // Merge thread aggregates
    let mut final_aggs = vec![LocalAgg::new(); num_countries];
    for t_agg in thread_aggs {
        for (i, item) in t_agg.iter().enumerate() {
            final_aggs[i].merge(item);
        }
    }

    // Format output
    let mut out_str = String::with_capacity(32 * 1024);
    for (i, agg) in final_aggs.iter().enumerate() {
        if agg.count > 0 {
            out_str.push_str(&countries_list[i]);
            out_str.push(' ');
            use std::fmt::Write;
            write!(&mut out_str, "{} {} ", agg.count, agg.id_sum).unwrap();
            format_money(agg.amount_sum, &mut out_str);
            out_str.push(' ');
            format_money(agg.amount_min, &mut out_str);
            out_str.push(' ');
            format_money(agg.amount_max, &mut out_str);
            out_str.push(' ');
            writeln!(
                &mut out_str,
                "{} {} {}",
                agg.names_distinct, agg.name_bytes, agg.note_bytes
            )
            .unwrap();
        }
    }

    std::fs::create_dir_all(&out_dir)?;
    let mut out_file = File::create(&out_path)?;
    out_file.write_all(out_str.as_bytes())?;

    Ok(())
}
