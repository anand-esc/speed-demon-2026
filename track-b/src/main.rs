use std::{
    fs::File,
    io::Write,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::Instant,
};

#[cfg(unix)]
fn mmap_file(p: &str) -> &'static [u8] {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
        fn madvise(a: *mut u8, l: usize, ad: i32) -> i32;
    }
    let f = File::open(p).expect("open file for mmap");
    let l = f.metadata().expect("file metadata").size() as usize;
    let fd = f.as_raw_fd();
    unsafe {
        let ptr = mmap(std::ptr::null_mut(), l, 1, 1, fd, 0);
        assert!(!ptr.is_null() && ptr != usize::MAX as *mut u8, "mmap failed");
        madvise(ptr, l, 2); // MADV_WILLNEED
        std::slice::from_raw_parts(ptr, l)
    }
}

const K1: u64 = 0x9E37_79B9_7F4A_7C15;
const K2: u64 = 0x517c_c1b7_2722_0a95;

const fn mk_cls() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        t[i] = if (b >= b'a' && b <= b'z') || (b >= b'0' && b <= b'9') {
            b
        } else if b >= b'A' && b <= b'Z' {
            b + 32
        } else {
            0
        };
        i += 1;
    }
    t
}
static CLS: [u8; 256] = mk_cls();

#[inline(always)]
fn pack_token(slice: &[u8]) -> (u64, u64, u32) {
    let len = slice.len();
    let mut w0 = 0u64;
    let mut w1 = 0u64;
    let mut w2_len = len as u32;

    let n0 = len.min(8);
    for i in 0..n0 {
        w0 |= ((slice[i] | 0x20) as u64) << (i * 8);
    }
    if len > 8 {
        let n1 = (len - 8).min(8);
        for i in 0..n1 {
            w1 |= ((slice[8 + i] | 0x20) as u64) << (i * 8);
        }
        if len > 16 {
            let n2 = (len - 16).min(8);
            for i in 0..n2 {
                w2_len |= ((slice[16 + i] | 0x20) as u32) << (8 + i * 8);
            }
        }
    }
    (w0, w1, w2_len)
}

#[inline(always)]
fn token_hash(w0: u64, w1: u64, w2_len: u32) -> u64 {
    let mut h = (w2_len as u64).wrapping_mul(K1);
    h = (h.rotate_left(5) ^ w0).wrapping_mul(K2);
    h = (h.rotate_left(5) ^ w1).wrapping_mul(K1);
    h ^ (h >> 32)
}

const TAB_CAP: usize = 262144;
const TAB_MASK: usize = TAB_CAP - 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Slot {
    w0: u64,
    w1: u64,
    w2_len: u32,
    id: u32,
}

struct FastTable {
    slots: Vec<Slot>,
    filter: Vec<u64>,
}

impl FastTable {
    fn new() -> Self {
        Self {
            slots: vec![Slot { w0: 0, w1: 0, w2_len: 0, id: 0 }; TAB_CAP],
            filter: vec![0u64; 1 << 16],
        }
    }

    #[inline(always)]
    fn insert(&mut self, w0: u64, w1: u64, w2_len: u32, id: u32) {
        let h = token_hash(w0, w1, w2_len);
        let bit_idx = (h >> 42) as usize;
        self.filter[bit_idx >> 6] |= 1u64 << (bit_idx & 63);

        let mut i = (h as usize) & TAB_MASK;
        loop {
            if self.slots[i].id == 0 {
                self.slots[i] = Slot { w0, w1, w2_len, id: id + 1 };
                return;
            }
            if self.slots[i].w0 == w0 && self.slots[i].w1 == w1 && self.slots[i].w2_len == w2_len {
                return;
            }
            i = (i + 1) & TAB_MASK;
        }
    }

    #[inline(always)]
    fn find(&self, w0: u64, w1: u64, w2_len: u32) -> Option<u32> {
        let h = token_hash(w0, w1, w2_len);
        let bit_idx = (h >> 42) as usize;
        if (self.filter[bit_idx >> 6] & (1u64 << (bit_idx & 63))) == 0 {
            return None;
        }

        let mut i = (h as usize) & TAB_MASK;
        loop {
            let s = self.slots[i];
            if s.id == 0 {
                return None;
            }
            if s.w0 == w0 && s.w1 == w1 && s.w2_len == w2_len {
                return Some(s.id - 1);
            }
            i = (i + 1) & TAB_MASK;
        }
    }
}

const BI_CAP: usize = 131072;
const BI_MASK: usize = BI_CAP - 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct BiSlot {
    key: u64,
    id: u32,
}

struct FastBiTable {
    slots: Vec<BiSlot>,
    filter: Vec<u64>,
}

impl FastBiTable {
    fn new() -> Self {
        Self {
            slots: vec![BiSlot { key: 0, id: 0 }; BI_CAP],
            filter: vec![0u64; 1 << 16],
        }
    }

    #[inline(always)]
    fn insert(&mut self, key: u64, id: u32) {
        let h = (key.wrapping_mul(K1) ^ (key >> 32)).wrapping_mul(K2);
        let bit_idx = (h >> 42) as usize;
        self.filter[bit_idx >> 6] |= 1u64 << (bit_idx & 63);

        let mut i = (h as usize) & BI_MASK;
        loop {
            if self.slots[i].id == 0 {
                self.slots[i] = BiSlot { key, id: id + 1 };
                return;
            }
            if self.slots[i].key == key {
                return;
            }
            i = (i + 1) & BI_MASK;
        }
    }

    #[inline(always)]
    fn find(&self, key: u64) -> Option<u32> {
        let h = (key.wrapping_mul(K1) ^ (key >> 32)).wrapping_mul(K2);
        let bit_idx = (h >> 42) as usize;
        if (self.filter[bit_idx >> 6] & (1u64 << (bit_idx & 63))) == 0 {
            return None;
        }

        let mut i = (h as usize) & BI_MASK;
        loop {
            let s = self.slots[i];
            if s.id == 0 {
                return None;
            }
            if s.key == key {
                return Some(s.id - 1);
            }
            i = (i + 1) & BI_MASK;
        }
    }
}

enum Query {
    Bool(Vec<(u8, u32)>),        // (op, term_id)
    Phrase(Vec<u32>, Vec<u32>),  // (term_ids, bigram_ids)
}

const NONE: u32 = u32::MAX;

#[inline]
fn and(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    let (la, lb) = (a.len(), b.len());
    if la * 16 < lb {
        for &x in a {
            let mut step = 1;
            while j + step < lb && b[j + step] < x {
                j += step;
                step <<= 1;
            }
            let high = (j + step + 1).min(lb);
            match b[j..high].binary_search(&x) {
                Ok(pos) => {
                    out.push(x);
                    j += pos + 1;
                }
                Err(pos) => {
                    j += pos;
                }
            }
            if j >= lb { break; }
        }
        return;
    }
    if lb * 16 < la {
        for &x in b {
            let mut step = 1;
            while i + step < la && a[i + step] < x {
                i += step;
                step <<= 1;
            }
            let high = (i + step + 1).min(la);
            match a[i..high].binary_search(&x) {
                Ok(pos) => {
                    out.push(x);
                    i += pos + 1;
                }
                Err(pos) => {
                    i += pos;
                }
            }
            if i >= la { break; }
        }
        return;
    }
    while i < la && j < lb {
        let (x, y) = (a[i], b[j]);
        if x < y {
            i += 1;
        } else if x > y {
            j += 1;
        } else {
            out.push(x);
            i += 1;
            j += 1;
        }
    }
}

#[inline]
fn or(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        if x < y {
            out.push(x);
            i += 1;
        } else if x > y {
            out.push(y);
            j += 1;
        } else {
            out.push(x);
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}

#[inline]
fn not(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut j = 0;
    for &x in a {
        while j < b.len() && b[j] < x {
            j += 1;
        }
        if j >= b.len() || b[j] != x {
            out.push(x);
        }
    }
}

static DIGITS: &[u8; 200] = b"\
00010203040506070809\
10111213141516171819\
20212223242526272829\
30313233343536373839\
40414243444546474849\
50515253545556575859\
60616263646566676869\
70717273747576777879\
80818283848586878889\
90919293949596979899";

#[inline(always)]
fn write_u64_fast(mut n: u64, buf: &mut Vec<u8>) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    let mut temp = [0u8; 20];
    let mut i = 20;
    while n >= 100 {
        let rem = (n % 100) as usize;
        n /= 100;
        i -= 2;
        let d = &DIGITS[rem * 2..rem * 2 + 2];
        temp[i] = d[0];
        temp[i + 1] = d[1];
    }
    if n >= 10 {
        let rem = n as usize;
        i -= 2;
        let d = &DIGITS[rem * 2..rem * 2 + 2];
        temp[i] = d[0];
        temp[i + 1] = d[1];
    } else if n > 0 {
        i -= 1;
        temp[i] = b'0' + n as u8;
    }
    buf.extend_from_slice(&temp[i..]);
}

struct ChunkCSR {
    term_offs: Vec<u32>,
    term_docs: Vec<u32>,
    bi_offs: Vec<u32>,
    bi_docs: Vec<u32>,
}

fn main() {
    let t_all = Instant::now();
    let timing = std::env::var_os("B_TIMING").is_some();

    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.windows(2).find(|w| w[0] == "--data").map(|w| w[1].as_str()).unwrap_or("/data");
    let out_dir = args.windows(2).find(|w| w[0] == "--out").map(|w| w[1].as_str()).unwrap_or("/out");

    let def_docs = format!("{}/docs.txt", data_dir);
    let def_qs = format!("{}/queries.txt", data_dir);
    let def_out = format!("{}/results.txt", out_dir);

    let positional: Vec<&str> = args.iter().skip(1).filter(|s| !s.starts_with("--")).map(|s| s.as_str()).collect();
    let docs_p = if positional.len() >= 3 { positional[0] } else { &def_docs };
    let qs_p = if positional.len() >= 3 { positional[1] } else { &def_qs };
    let out_p = if positional.len() >= 3 { positional[2] } else { &def_out };

    let t0 = Instant::now();
    let q_bytes = mmap_file(qs_p);
    let mut tab = FastTable::new();
    let mut bi_tab = FastBiTable::new();
    let mut num_terms = 0u32;
    let mut num_bigrams = 0u32;
    let mut queries = Vec::with_capacity(50_000);

    for line in q_bytes.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(b"BOOL ") {
            let mut ops = Vec::new();
            let mut cur_op = 0u8;
            for (i, part) in line[5..].split(|&b| b == b' ').enumerate() {
                if i % 2 == 0 {
                    let (w0, w1, w2_len) = pack_token(part);
                    let tid = match tab.find(w0, w1, w2_len) {
                        Some(id) => id,
                        None => {
                            let id = num_terms;
                            tab.insert(w0, w1, w2_len, id);
                            num_terms += 1;
                            id
                        }
                    };
                    ops.push((cur_op, tid));
                } else {
                    cur_op = match part {
                        b"AND" => 0,
                        b"OR" => 1,
                        _ => 2,
                    };
                }
            }
            queries.push(Query::Bool(ops));
        } else if line.starts_with(b"PHRASE ") {
            let inner = &line[8..line.len() - 1];
            let mut terms = Vec::new();
            for part in inner.split(|&b| b == b' ') {
                let (w0, w1, w2_len) = pack_token(part);
                let tid = match tab.find(w0, w1, w2_len) {
                    Some(id) => id,
                    None => {
                        let id = num_terms;
                        tab.insert(w0, w1, w2_len, id);
                        num_terms += 1;
                        id
                    }
                };
                terms.push(tid);
            }
            let mut bigrams = Vec::new();
            for w in terms.windows(2) {
                let key = ((w[0] as u64) << 32) | (w[1] as u64);
                let bi_id = match bi_tab.find(key) {
                    Some(id) => id,
                    None => {
                        let id = num_bigrams;
                        bi_tab.insert(key, id);
                        num_bigrams += 1;
                        id
                    }
                };
                bigrams.push(bi_id);
            }
            bigrams.sort_unstable();
            bigrams.dedup();
            queries.push(Query::Phrase(terms, bigrams));
        }
    }
    let d_queries = t0.elapsed();

    let t1 = Instant::now();
    let d_bytes = mmap_file(docs_p);
    let nthreads = 12;
    let nchunks = 48;
    let chunk_size = d_bytes.len() / nchunks;
    let mut bounds = vec![0];
    for i in 1..nchunks {
        let mut p = i * chunk_size;
        while p < d_bytes.len() && d_bytes[p] != b'\n' {
            p += 1;
        }
        bounds.push(if p < d_bytes.len() { p + 1 } else { p });
    }
    bounds.push(d_bytes.len());

    let mut doc_starts = vec![0u32; 250_001];
    let ds_ptr = doc_starts.as_mut_ptr() as usize;

    let next_chunk = AtomicUsize::new(0);
    let bounds_ref = &bounds;
    let mut chunks = (0..nchunks).map(|_| None).collect::<Vec<Option<ChunkCSR>>>();
    let chunks_ptr = chunks.as_mut_ptr() as usize;

    // Scan 48 chunks dynamically across 12 worker threads
    thread::scope(|s| {
        for _ in 0..nthreads {
            let next_ref = &next_chunk;
            let tab_ref = &tab;
            let bi_tab_ref = &bi_tab;

            s.spawn(move || {
                let ds = ds_ptr as *mut u32;
                let c_arr = chunks_ptr as *mut Option<ChunkCSR>;
                let mut doc_terms = Vec::with_capacity(512);
                let mut doc_bigrams = Vec::with_capacity(256);
                let mut tp = Vec::with_capacity(600_000);
                let mut bp = Vec::with_capacity(100_000);

                loop {
                    let c = next_ref.fetch_add(1, Ordering::Relaxed);
                    if c >= nchunks {
                        break;
                    }
                    tp.clear();
                    bp.clear();

                    let chunk_start = bounds_ref[c];
                    let chunk_end = bounds_ref[c + 1];
                    let chunk = &d_bytes[chunk_start..chunk_end];
                    let mut p = 0;
                    let len = chunk.len();

                    while p < len {
                        let doc_start_abs = (chunk_start + p) as u32;
                        let mut doc_id = 0u32;
                        while p < len && chunk[p].is_ascii_digit() {
                            doc_id = doc_id * 10 + (chunk[p] - b'0') as u32;
                            p += 1;
                        }
                        unsafe { *ds.add(doc_id as usize) = doc_start_abs; }

                        if p < len && chunk[p] == b'\t' {
                            p += 1;
                        }

                        doc_terms.clear();
                        doc_bigrams.clear();
                        let mut prev_tid = NONE;

                        while p < len {
                            let b = chunk[p];
                            if b == b'\n' {
                                p += 1;
                                break;
                            }
                            if CLS[b as usize] != 0 {
                                let start = p;
                                while p < len && CLS[chunk[p] as usize] != 0 {
                                    p += 1;
                                }
                                let tok_len = p - start;
                                if tok_len <= 17 {
                                    let (w0, w1, w2_len) = pack_token(&chunk[start..p]);
                                    if let Some(tid) = tab_ref.find(w0, w1, w2_len) {
                                        doc_terms.push(tid);
                                        if prev_tid != NONE {
                                            let bi_key = ((prev_tid as u64) << 32) | (tid as u64);
                                            if let Some(bi_id) = bi_tab_ref.find(bi_key) {
                                                doc_bigrams.push(bi_id);
                                            }
                                        }
                                        prev_tid = tid;
                                    } else {
                                        prev_tid = NONE;
                                    }
                                } else {
                                    prev_tid = NONE;
                                }
                            } else {
                                p += 1;
                            }
                        }

                        if !doc_terms.is_empty() {
                            doc_terms.sort_unstable();
                            doc_terms.dedup();
                            for &tid in &doc_terms {
                                tp.push((tid, doc_id));
                            }
                        }

                        if !doc_bigrams.is_empty() {
                            doc_bigrams.sort_unstable();
                            doc_bigrams.dedup();
                            for &bid in &doc_bigrams {
                                bp.push((bid, doc_id));
                            }
                        }
                    }

                    // Build CSR for chunk
                    let mut term_offs = vec![0u32; num_terms as usize + 1];
                    for &(tid, _) in &tp {
                        term_offs[tid as usize + 1] += 1;
                    }
                    for i in 0..num_terms as usize {
                        term_offs[i + 1] += term_offs[i];
                    }
                    let mut term_docs = vec![0u32; tp.len()];
                    let mut cur_t = term_offs.clone();
                    for &(tid, doc_id) in &tp {
                        let off = cur_t[tid as usize];
                        term_docs[off as usize] = doc_id;
                        cur_t[tid as usize] = off + 1;
                    }

                    let mut bi_offs = vec![0u32; num_bigrams as usize + 1];
                    for &(bid, _) in &bp {
                        bi_offs[bid as usize + 1] += 1;
                    }
                    for i in 0..num_bigrams as usize {
                        bi_offs[i + 1] += bi_offs[i];
                    }
                    let mut bi_docs = vec![0u32; bp.len()];
                    let mut cur_b = bi_offs.clone();
                    for &(bid, doc_id) in &bp {
                        let off = cur_b[bid as usize];
                        bi_docs[off as usize] = doc_id;
                        cur_b[bid as usize] = off + 1;
                    }

                    unsafe {
                        *c_arr.add(c) = Some(ChunkCSR {
                            term_offs,
                            term_docs,
                            bi_offs,
                            bi_docs,
                        });
                    }
                }
            });
        }
    });

    doc_starts[250_000] = d_bytes.len() as u32;

    let ready_chunks: Vec<ChunkCSR> = chunks.into_iter().map(|c| c.unwrap()).collect();

    // Now merge all 48 chunks into flat global postings in parallel
    let nt = num_terms as usize;
    let t_shard = (nt + nthreads - 1) / nthreads;
    let mut term_head = vec![0u32; nt + 1];

    for c in &ready_chunks {
        for i in 0..nt {
            term_head[i + 1] += c.term_offs[i + 1] - c.term_offs[i];
        }
    }
    for i in 0..nt {
        term_head[i + 1] += term_head[i];
    }

    let nb = num_bigrams as usize;
    let b_shard = (nb + nthreads - 1) / nthreads;
    let mut bi_head = vec![0u32; nb + 1];

    for c in &ready_chunks {
        for i in 0..nb {
            bi_head[i + 1] += c.bi_offs[i + 1] - c.bi_offs[i];
        }
    }
    for i in 0..nb {
        bi_head[i + 1] += bi_head[i];
    }

    let total_term_postings = term_head[nt] as usize;
    let total_bi_postings = bi_head[nb] as usize;

    let mut term_postings = vec![0u32; total_term_postings];
    let mut bi_postings = vec![0u32; total_bi_postings];

    let tp_ptr = term_postings.as_mut_ptr() as usize;
    let bp_ptr = bi_postings.as_mut_ptr() as usize;

    let rc_ref = &ready_chunks;
    let th_ref_merge = &term_head;
    let bh_ref_merge = &bi_head;

    thread::scope(|s| {
        for t in 0..nthreads {
            s.spawn(move || {
                let out = tp_ptr as *mut u32;
                let st = t * t_shard;
                let en = (st + t_shard).min(nt);

                for tid in st..en {
                    let mut write_pos = th_ref_merge[tid] as usize;
                    for c in rc_ref {
                        let c_st = c.term_offs[tid] as usize;
                        let c_en = c.term_offs[tid + 1] as usize;
                        let count = c_en - c_st;
                        if count > 0 {
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    c.term_docs.as_ptr().add(c_st),
                                    out.add(write_pos),
                                    count,
                                );
                            }
                            write_pos += count;
                        }
                    }
                }
            });
        }

        for t in 0..nthreads {
            s.spawn(move || {
                let out = bp_ptr as *mut u32;
                let st = t * b_shard;
                let en = (st + b_shard).min(nb);

                for bid in st..en {
                    let mut write_pos = bh_ref_merge[bid] as usize;
                    for c in rc_ref {
                        let c_st = c.bi_offs[bid] as usize;
                        let c_en = c.bi_offs[bid + 1] as usize;
                        let count = c_en - c_st;
                        if count > 0 {
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    c.bi_docs.as_ptr().add(c_st),
                                    out.add(write_pos),
                                    count,
                                );
                            }
                            write_pos += count;
                        }
                    }
                }
            });
        }
    });

    let d_build = t1.elapsed();

    // EVALUATION: 12 threads answer all 50,000 queries in parallel
    let t2 = Instant::now();
    let nq = queries.len();
    let q_chunk = (nq + nthreads - 1) / nthreads;
    let mut thread_results = vec![Vec::<u8>::new(); nthreads];

    let (q_ref, th_ref, tp_ref, bh_ref, bp_ref, ds_ref, db_ref, tab_ref) = (
        &queries,
        &term_head,
        &term_postings,
        &bi_head,
        &bi_postings,
        &doc_starts,
        &d_bytes,
        &tab,
    );

    thread::scope(|s| {
        for (t, out_buf) in thread_results.iter_mut().enumerate() {
            let st = t * q_chunk;
            let en = (st + q_chunk).min(nq);

            s.spawn(move || {
                let mut buf_a = Vec::with_capacity(32768);
                let mut buf_b = Vec::with_capacity(32768);
                let mut doc_words = Vec::with_capacity(512);
                let mut verified = Vec::with_capacity(64);

                for qi in st..en {
                    let res_slice: &[u32];
                    let mut use_buf_a;

                    match &q_ref[qi] {
                        Query::Bool(ops) => {
                            let (_, first_tid) = ops[0];
                            let s = th_ref[first_tid as usize] as usize;
                            let e = th_ref[first_tid as usize + 1] as usize;
                            buf_a.clear();
                            buf_a.extend_from_slice(&tp_ref[s..e]);
                            use_buf_a = true;

                            for &(op, tid) in &ops[1..] {
                                let s = th_ref[tid as usize] as usize;
                                let e = th_ref[tid as usize + 1] as usize;
                                let b = &tp_ref[s..e];

                                if use_buf_a {
                                    match op {
                                        0 => and(&buf_a, b, &mut buf_b),
                                        1 => or(&buf_a, b, &mut buf_b),
                                        _ => not(&buf_a, b, &mut buf_b),
                                    }
                                    use_buf_a = false;
                                } else {
                                    match op {
                                        0 => and(&buf_b, b, &mut buf_a),
                                        1 => or(&buf_b, b, &mut buf_a),
                                        _ => not(&buf_b, b, &mut buf_a),
                                    }
                                    use_buf_a = true;
                                }
                            }
                            res_slice = if use_buf_a { &buf_a } else { &buf_b };
                        }
                        Query::Phrase(terms, bigrams) => {
                            let k = terms.len();
                            if bigrams.len() == 1 && k == 2 {
                                let bi = bigrams[0] as usize;
                                let s = bh_ref[bi] as usize;
                                let e = bh_ref[bi + 1] as usize;
                                res_slice = &bp_ref[s..e];
                            } else {
                                let mut order = bigrams.clone();
                                order.sort_unstable_by_key(|&bi| {
                                    let s = bh_ref[bi as usize] as usize;
                                    let e = bh_ref[bi as usize + 1] as usize;
                                    e - s
                                });

                                let bi0 = order[0] as usize;
                                let s0 = bh_ref[bi0] as usize;
                                let e0 = bh_ref[bi0 + 1] as usize;
                                let list0 = &bp_ref[s0..e0];

                                if order.len() == 1 {
                                    buf_a.clear();
                                    buf_a.extend_from_slice(list0);
                                    use_buf_a = true;
                                } else {
                                    let bi1 = order[1] as usize;
                                    let s1 = bh_ref[bi1] as usize;
                                    let e1 = bh_ref[bi1 + 1] as usize;
                                    let list1 = &bp_ref[s1..e1];

                                    and(list0, list1, &mut buf_a);
                                    use_buf_a = true;

                                    for &bi in &order[2..] {
                                        if (use_buf_a && buf_a.is_empty()) || (!use_buf_a && buf_b.is_empty()) {
                                            break;
                                        }
                                        let s = bh_ref[bi as usize] as usize;
                                        let e = bh_ref[bi as usize + 1] as usize;
                                        let b = &bp_ref[s..e];

                                        if use_buf_a {
                                            and(&buf_a, b, &mut buf_b);
                                            use_buf_a = false;
                                        } else {
                                            and(&buf_b, b, &mut buf_a);
                                            use_buf_a = true;
                                        }
                                    }
                                }
                                let cand = if use_buf_a { &buf_a } else { &buf_b };

                                if k == 2 || cand.is_empty() {
                                    res_slice = cand;
                                } else {
                                    verified.clear();
                                    for &doc_id in cand.iter() {
                                        let d = doc_id as usize;
                                        let st = ds_ref[d] as usize;
                                        let en = ds_ref[d + 1] as usize;
                                        let text = &db_ref[st..en];

                                        let mut p = 0;
                                        while p < text.len() && text[p] != b'\t' {
                                            p += 1;
                                        }
                                        if p < text.len() { p += 1; }

                                        doc_words.clear();
                                        while p < text.len() {
                                            let b = text[p];
                                            if b == b'\n' { break; }
                                            if CLS[b as usize] != 0 {
                                                let start = p;
                                                while p < text.len() && CLS[text[p] as usize] != 0 {
                                                    p += 1;
                                                }
                                                let tok_len = p - start;
                                                let tid = if tok_len <= 17 {
                                                    let (w0, w1, w2) = pack_token(&text[start..p]);
                                                    tab_ref.find(w0, w1, w2).unwrap_or(NONE)
                                                } else {
                                                    NONE
                                                };
                                                doc_words.push(tid);
                                            } else {
                                                p += 1;
                                            }
                                        }

                                        if doc_words.len() >= k && doc_words.windows(k).any(|w| w == terms.as_slice()) {
                                            verified.push(doc_id);
                                        }
                                    }
                                    res_slice = &verified;
                                }
                            }
                        }
                    }

                    let len = res_slice.len();
                    write_u64_fast(len as u64, out_buf);
                    out_buf.push(b' ');
                    let sum: u64 = res_slice.iter().map(|&x| x as u64).sum();
                    write_u64_fast(sum, out_buf);
                    for &x in res_slice.iter().take(10) {
                        out_buf.push(b' ');
                        write_u64_fast(x as u64, out_buf);
                    }
                    out_buf.push(b'\n');
                }
            });
        }
    });

    let d_eval = t2.elapsed();

    let mut out_file = File::create(out_p).expect("create results output file");
    for tr in &thread_results {
        out_file.write_all(tr).expect("write results output");
    }

    if timing {
        eprintln!(
            "Total: {:.2} ms | Queries: {:.2} ms | Build: {:.2} ms | Eval: {:.2} ms",
            t_all.elapsed().as_secs_f64() * 1000.0,
            d_queries.as_secs_f64() * 1000.0,
            d_build.as_secs_f64() * 1000.0,
            d_eval.as_secs_f64() * 1000.0
        );
    }
}
