//! Track B (optimised, v7: compact exact lookup tables). std only.
use std::borrow::Cow;
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const NONE: u32 = u32::MAX;
const OVER: usize = 256 * 1024;
const K1: u64 = 0x9E37_79B9_7F4A_7C15;
const K2: u64 = 0x517c_c1b7_2722_0a95;

type TokBuf = [u8; 32];

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

fn clog2(x: usize) -> u32 {
    if x <= 1 {
        0
    } else {
        usize::BITS - (x - 1).leading_zeros()
    }
}

#[inline(always)]
fn hash_buf(buf: &TokBuf, n: usize) -> u64 {
    let mut h = (n as u64).wrapping_mul(K1);
    let mut k = 0;
    while k < n {
        let mut v = u64::from_le_bytes(buf[k..k + 8].try_into().unwrap());
        let rem = n - k;
        if rem < 8 {
            v &= (1u64 << (rem * 8)) - 1;
        }
        h = (h.rotate_left(5) ^ v).wrapping_mul(K2);
        k += 8;
    }
    h ^ (h >> 32)
}

/// Exact packed key for n <= 8, 64-bit hash for longer tokens.
#[inline(always)]
fn key_of(buf: &TokBuf, n: usize) -> u64 {
    if n <= 8 {
        let v = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        if n < 8 {
            v & ((1u64 << (n * 8)) - 1)
        } else {
            v
        }
    } else {
        hash_buf(buf, n)
    }
}

/// Calls f(buf, n) per token (lowercased in buf[..n]). n == 0 means "token longer
/// than 32 bytes" (keeps position, matches nothing).
#[inline(always)]
fn tokenize(text: &[u8], mut f: impl FnMut(&TokBuf, usize)) {
    let mut buf = [0u8; 32];
    let mut n = 0usize;
    for &b in text {
        let c = CLS[b as usize];
        if c != 0 {
            if n < 32 {
                buf[n] = c;
            }
            n += 1;
        } else if n > 0 {
            f(&buf, if n <= 32 { n } else { 0 });
            n = 0;
        }
    }
    if n > 0 {
        f(&buf, if n <= 32 { n } else { 0 });
    }
}

/// Interning table, used only while parsing the queries.
struct Table {
    mask: usize,
    ent: Vec<u64>, // (tag32 << 32) | (id+1); low half 0 = empty
    spans: Vec<(u32, u32)>,
    arena: Vec<u8>,
}

impl Table {
    fn new(bits: u32) -> Self {
        let size = 1usize << bits;
        Table { mask: size - 1, ent: vec![0; size], spans: Vec::new(), arena: Vec::new() }
    }
    #[inline(always)]
    fn term(&self, id: u32) -> &[u8] {
        let (o, l) = self.spans[id as usize];
        &self.arena[o as usize..(o + l) as usize]
    }
    fn find_buf(&self, buf: &TokBuf, n: usize) -> Option<u32> {
        if n == 0 {
            return None;
        }
        let h = hash_buf(buf, n);
        let tag = h >> 32;
        let mut i = (h as usize) & self.mask;
        loop {
            let e = self.ent[i];
            let id = (e & 0xFFFF_FFFF) as u32;
            if id == 0 {
                return None;
            }
            if (e >> 32) == tag && self.term(id - 1) == &buf[..n] {
                return Some(id - 1);
            }
            i = (i + 1) & self.mask;
        }
    }
    fn intern(&mut self, b: &[u8]) -> u32 {
        let mut buf = [0u8; 32];
        let n = b.len().min(32);
        buf[..n].copy_from_slice(&b[..n]);
        if let Some(id) = self.find_buf(&buf, n) {
            return id;
        }
        let h = hash_buf(&buf, n);
        let id = self.spans.len() as u32;
        self.spans.push((self.arena.len() as u32, n as u32));
        self.arena.extend_from_slice(&b[..n]);
        let mut i = (h as usize) & self.mask;
        while (self.ent[i] & 0xFFFF_FFFF) != 0 {
            i = (i + 1) & self.mask;
        }
        self.ent[i] = ((h >> 32) << 32) | (id as u64 + 1);
        id
    }
}

/// Compact, right-sized lookup table used on the hot path.
/// Slot = (key, len << 32 | id+1); second word 0 = empty.
struct Fast {
    mask: usize,
    ent: Vec<(u64, u64)>,
    spans: Vec<(u32, u32)>,
    arena: Vec<u8>,
}

impl Fast {
    fn build(t: &Table) -> Fast {
        let nt = t.spans.len();
        let bits = clog2(nt * 3).max(10);
        let size = 1usize << bits;
        let mut f = Fast {
            mask: size - 1,
            ent: vec![(0, 0); size],
            spans: t.spans.clone(),
            arena: t.arena.clone(),
        };
        for id in 0..nt {
            let b = t.term(id as u32);
            let n = b.len();
            let mut buf = [0u8; 32];
            buf[..n].copy_from_slice(b);
            let key = key_of(&buf, n);
            let mut i = (key.wrapping_mul(K1) >> 32) as usize & f.mask;
            while f.ent[i].1 != 0 {
                i = (i + 1) & f.mask;
            }
            f.ent[i] = (key, ((n as u64) << 32) | (id as u64 + 1));
        }
        f
    }
    #[inline(always)]
    fn term(&self, id: u32) -> &[u8] {
        let (o, l) = self.spans[id as usize];
        &self.arena[o as usize..(o + l) as usize]
    }
    #[inline(always)]
    fn find_buf(&self, buf: &TokBuf, n: usize) -> Option<u32> {
        if n == 0 {
            return None;
        }
        let key = key_of(buf, n);
        let mut i = (key.wrapping_mul(K1) >> 32) as usize & self.mask;
        loop {
            let (k, m) = self.ent[i];
            if m == 0 {
                return None;
            }
            if k == key && (m >> 32) == n as u64 {
                let id = ((m & 0xFFFF_FFFF) - 1) as u32;
                if n <= 8 || self.term(id) == &buf[..n] {
                    return Some(id);
                }
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// Table of adjacent term-id pairs that occur inside some PHRASE query.
struct BiTable {
    mask: usize,
    ent: Vec<(u64, u32)>, // (key, id+1); id+1 == 0 means empty
    keys: Vec<u64>,       // keys in id order
    count: u32,
    bits: Vec<u64>, // 2^22-bit prefilter
}

impl BiTable {
    fn new(bits: u32) -> Self {
        let size = 1usize << bits;
        BiTable {
            mask: size - 1,
            ent: vec![(0, 0); size],
            keys: Vec::new(),
            count: 0,
            bits: vec![0u64; 1 << 16],
        }
    }
    /// Same ids, right-sized table.
    fn compact(&self) -> BiTable {
        let bits = clog2(self.keys.len() * 3).max(10);
        let mut n = BiTable::new(bits);
        for &k in &self.keys {
            n.intern(k);
        }
        n
    }
    #[inline(always)]
    fn maybe(&self, key: u64) -> bool {
        let b = (key.wrapping_mul(K1) >> 42) as usize;
        (self.bits[b >> 6] >> (b & 63)) & 1 != 0
    }
    #[inline(always)]
    fn find(&self, key: u64) -> Option<u32> {
        let mut i = (key.wrapping_mul(K1) >> 32) as usize & self.mask;
        loop {
            let (k, id) = self.ent[i];
            if id == 0 {
                return None;
            }
            if k == key {
                return Some(id - 1);
            }
            i = (i + 1) & self.mask;
        }
    }
    fn intern(&mut self, key: u64) -> u32 {
        if let Some(id) = self.find(key) {
            return id;
        }
        let id = self.count;
        self.count += 1;
        self.keys.push(key);
        let b = (key.wrapping_mul(K1) >> 42) as usize;
        self.bits[b >> 6] |= 1u64 << (b & 63);
        let mut i = (key.wrapping_mul(K1) >> 32) as usize & self.mask;
        while self.ent[i].1 != 0 {
            i = (i + 1) & self.mask;
        }
        self.ent[i] = (key, id + 1);
        id
    }
}

enum Q {
    Bool(Vec<(u8, u32)>), // (op: 0 AND, 1 OR, 2 NOT; first op ignored, term id)
    Phrase(Vec<u32>, Vec<u32>), // term ids, distinct bigram ids
}

fn parse_queries(buf: &[u8], tab: &mut Table, bt: &mut BiTable) -> Vec<Q> {
    let mut qs = Vec::with_capacity(50_000);
    for line in buf.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(b"BOOL ") {
            let mut v = Vec::new();
            let mut op = 0u8;
            for (i, w) in line[5..].split(|&b| b == b' ').enumerate() {
                if i % 2 == 0 {
                    v.push((op, tab.intern(w)));
                } else {
                    op = match w {
                        b"AND" => 0,
                        b"OR" => 1,
                        _ => 2,
                    };
                }
            }
            qs.push(Q::Bool(v));
        } else {
            let inner = &line[8..line.len() - 1];
            let v: Vec<u32> = inner.split(|&b| b == b' ').map(|w| tab.intern(w)).collect();
            let mut bis: Vec<u32> =
                v.windows(2).map(|w| bt.intern(((w[0] as u64) << 32) | w[1] as u64)).collect();
            bis.sort_unstable();
            bis.dedup();
            qs.push(Q::Phrase(v, bis));
        }
    }
    qs
}

struct Chunk {
    buf: Vec<u8>,
    starts: Vec<u32>, // line start offsets in buf, one per owned doc
    offs: Vec<u32>,   // CSR per term
    docs: Vec<u32>,
    boffs: Vec<u32>, // CSR per bigram
    bdocs: Vec<u32>,
}

fn csr(pairs: &[(u32, u32)], n: usize) -> (Vec<u32>, Vec<u32>) {
    let mut offs = vec![0u32; n + 1];
    for &(t, _) in pairs {
        offs[t as usize + 1] += 1;
    }
    for i in 0..n {
        offs[i + 1] += offs[i];
    }
    let mut cur = offs.clone();
    let mut docs = vec![0u32; pairs.len()];
    for &(t, d) in pairs {
        let c = &mut cur[t as usize];
        docs[*c as usize] = d;
        *c += 1;
    }
    (offs, docs)
}

/// `last` / `lastb` are per-worker scratch arrays, reused across chunks
/// (doc ids never repeat, so stale entries can never match).
fn build_chunk(
    path: &str,
    s: usize,
    e: usize,
    flen: usize,
    tab: &Fast,
    bt: &BiTable,
    nt: usize,
    nb: usize,
    last: &mut [u32],
    lastb: &mut [u32],
    timing: bool,
) -> Chunk {
    let t_start = Instant::now();
    let b0 = if s == 0 { 0 } else { s - 1 };
    let mut f = File::open(path).expect("open docs");
    f.seek(SeekFrom::Start(b0 as u64)).unwrap();
    let want = (e + OVER).min(flen) - b0;
    let mut buf = vec![0u8; want];
    f.read_exact(&mut buf).unwrap();
    let t_read = t_start.elapsed().as_secs_f64();

    let mut ls = if s == 0 {
        0
    } else {
        match buf.iter().position(|&b| b == b'\n') {
            Some(j) => j + 1,
            None => buf.len(),
        }
    };
    let mut doc: u32 = 0;
    {
        let mut p = ls;
        while p < buf.len() && buf[p].is_ascii_digit() {
            doc = doc * 10 + (buf[p] - b'0') as u32;
            p += 1;
        }
    }
    let mut starts: Vec<u32> = Vec::with_capacity((e - s) / 800 + 8);
    let mut pairs: Vec<(u32, u32)> = Vec::with_capacity((e - s) / 8 + 16);
    let mut bpairs: Vec<(u32, u32)> = Vec::with_capacity((e - s) / 32 + 16);

    loop {
        if b0 + ls >= e || ls >= buf.len() {
            break;
        }
        let le = loop {
            let pos = buf[ls..].iter().position(|&b| b == b'\n');
            match pos {
                Some(x) => break ls + x,
                None => {
                    if b0 + buf.len() >= flen {
                        break buf.len();
                    }
                    let more = OVER.min(flen - (b0 + buf.len()));
                    let old = buf.len();
                    buf.resize(old + more, 0);
                    f.read_exact(&mut buf[old..]).unwrap();
                }
            }
        };
        starts.push(ls as u32);
        let line = &buf[ls..le];
        if let Some(tp) = line.iter().position(|&b| b == b'\t') {
            let mut prev = NONE;
            tokenize(&line[tp + 1..], |b, n| match tab.find_buf(b, n) {
                Some(t) => {
                    let tu = t as usize;
                    if last[tu] != doc {
                        last[tu] = doc;
                        pairs.push((t, doc));
                    }
                    if prev != NONE {
                        let key = ((prev as u64) << 32) | t as u64;
                        if bt.maybe(key) {
                            if let Some(bi) = bt.find(key) {
                                if lastb[bi as usize] != doc {
                                    lastb[bi as usize] = doc;
                                    bpairs.push((bi, doc));
                                }
                            }
                        }
                    }
                    prev = t;
                }
                None => prev = NONE,
            });
        }
        doc += 1;
        ls = le + 1;
    }
    let t_tok = t_start.elapsed().as_secs_f64();

    let (offs, docs) = csr(&pairs, nt);
    let (boffs, bdocs) = csr(&bpairs, nb);
    if timing {
        eprintln!(
            "  chunk0: read {:.3}s, tokenized by {:.3}s, sorted by {:.3}s",
            t_read,
            t_tok,
            t_start.elapsed().as_secs_f64()
        );
    }
    Chunk { buf, starts, offs, docs, boffs, bdocs }
}

fn merge_lists(chunks: &[Chunk], n: usize, nthreads: usize, bi: bool) -> Vec<Vec<u32>> {
    let parts: Vec<Vec<Vec<u32>>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nthreads)
            .map(|i| {
                let lo = n / nthreads * i;
                let hi = if i + 1 == nthreads { n } else { n / nthreads * (i + 1) };
                sc.spawn(move || {
                    let mut out: Vec<Vec<u32>> = Vec::with_capacity(hi - lo);
                    for t in lo..hi {
                        let mut total = 0usize;
                        for c in chunks {
                            let o = if bi { &c.boffs } else { &c.offs };
                            total += (o[t + 1] - o[t]) as usize;
                        }
                        let mut v = Vec::with_capacity(total);
                        for c in chunks {
                            let (o, d) = if bi { (&c.boffs, &c.bdocs) } else { (&c.offs, &c.docs) };
                            v.extend_from_slice(&d[o[t] as usize..o[t + 1] as usize]);
                        }
                        out.push(v);
                    }
                    out
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut res: Vec<Vec<u32>> = Vec::with_capacity(n);
    for p in parts {
        res.extend(p);
    }
    res
}

#[inline]
fn and(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (a, b) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if a.len() * 32 < b.len() {
        return a.iter().copied().filter(|x| b.binary_search(x).is_ok()).collect();
    }
    let mut out = Vec::with_capacity(a.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
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
    out
}

fn or(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
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
    out
}

fn not(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.len() * 32 < b.len() {
        return a.iter().copied().filter(|x| b.binary_search(x).is_err()).collect();
    }
    let mut out = Vec::with_capacity(a.len());
    let mut j = 0;
    for &x in a {
        while j < b.len() && b[j] < x {
            j += 1;
        }
        if j >= b.len() || b[j] != x {
            out.push(x);
        }
    }
    out
}

fn eval_bool<'a>(t: &[(u8, u32)], post: &'a [Vec<u32>]) -> Cow<'a, [u32]> {
    let mut r: Cow<'a, [u32]> = Cow::Borrowed(&post[t[0].1 as usize][..]);
    for &(op, id) in &t[1..] {
        let b = &post[id as usize][..];
        let n = match op {
            0 => and(&r, b),
            1 => or(&r, b),
            _ => not(&r, b),
        };
        r = Cow::Owned(n);
    }
    r
}

fn doc_line<'a>(d: u32, chunks: &'a [Chunk], cum: &[u32]) -> &'a [u8] {
    let c = cum.partition_point(|&x| x <= d) - 1;
    let ch = &chunks[c];
    let local = (d - cum[c]) as usize;
    let s = ch.starts[local] as usize;
    let e = match ch.buf[s..].iter().position(|&b| b == b'\n') {
        Some(x) => s + x,
        None => ch.buf.len(),
    };
    &ch.buf[s..e]
}

fn eval_phrase(
    q: &[u32],
    bis: &[u32],
    bpost: &[Vec<u32>],
    chunks: &[Chunk],
    cum: &[u32],
    tab: &Fast,
    ids: &mut Vec<u32>,
) -> Vec<u32> {
    // candidates = docs containing every adjacent pair of the phrase
    let mut order: Vec<u32> = bis.to_vec();
    order.sort_unstable_by_key(|&b| bpost[b as usize].len());
    let mut cand: Vec<u32> = if order.len() == 1 {
        bpost[order[0] as usize].clone()
    } else {
        and(&bpost[order[0] as usize], &bpost[order[1] as usize])
    };
    for &b in order.iter().skip(2) {
        if cand.is_empty() {
            break;
        }
        cand = and(&cand, &bpost[b as usize]);
    }
    let k = q.len();
    if k == 2 || cand.is_empty() {
        return cand; // a single bigram list is exact
    }
    let mut out = Vec::new();
    for &d in &cand {
        let line = doc_line(d, chunks, cum);
        let tp = match line.iter().position(|&b| b == b'\t') {
            Some(x) => x,
            None => continue,
        };
        ids.clear();
        tokenize(&line[tp + 1..], |b, n| ids.push(tab.find_buf(b, n).unwrap_or(NONE)));
        if ids.len() >= k && ids.windows(k).any(|w| w == q) {
            out.push(d);
        }
    }
    out
}

fn main() {
    let t0 = Instant::now();
    let timing = std::env::var_os("B_TIMING").is_some();
    let lap = |name: &str| {
        if timing {
            eprintln!("{:>10}: {:.3}s", name, t0.elapsed().as_secs_f64());
        }
    };

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

    let qbuf = std::fs::read(qs_p).expect("read queries");
    let occ = qbuf.iter().filter(|&&b| b == b' ' || b == b'\n').count();
    let bits = (usize::BITS - (occ * 2).leading_zeros()).clamp(10, 24);
    let mut tab0 = Table::new(bits);
    let mut bt0 = BiTable::new(bits);
    let queries = parse_queries(&qbuf, &mut tab0, &mut bt0);
    let nt = tab0.spans.len();
    let nb = bt0.count as usize;
    let tab = Fast::build(&tab0);
    let bt = bt0.compact();
    drop(tab0);
    drop(bt0);
    lap("queries");

    let flen = std::fs::metadata(docs_p).expect("stat docs").len() as usize;
    let mut nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(64);
    if let Some(v) = std::env::var_os("B_THREADS") {
        if let Some(n) = v.to_str().and_then(|s| s.parse::<usize>().ok()) {
            nthreads = n.clamp(1, 64);
        }
    }
    let tab_ref = &tab;
    let bt_ref = &bt;

    // Dynamic scheduling: many more chunks than threads, workers pull the next one.
    let nchunks = (nthreads * 4).max(1);
    let next_chunk = AtomicUsize::new(0);
    let mut got: Vec<(usize, Chunk)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nthreads)
            .map(|_| {
                sc.spawn(|| {
                    let mut last = vec![NONE; nt];
                    let mut lastb = vec![NONE; nb];
                    let mut mine: Vec<(usize, Chunk)> = Vec::new();
                    loop {
                        let i = next_chunk.fetch_add(1, Ordering::Relaxed);
                        if i >= nchunks {
                            break;
                        }
                        let s = flen / nchunks * i;
                        let e = if i + 1 == nchunks { flen } else { flen / nchunks * (i + 1) };
                        let c = build_chunk(
                            docs_p, s, e, flen, tab_ref, bt_ref, nt, nb, &mut last, &mut lastb,
                            timing && i == 0,
                        );
                        mine.push((i, c));
                    }
                    mine
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    got.sort_by_key(|x| x.0);
    let chunks: Vec<Chunk> = got.into_iter().map(|x| x.1).collect();
    lap("build");

    let mut cum: Vec<u32> = Vec::with_capacity(chunks.len() + 1);
    cum.push(0);
    for c in &chunks {
        let last = *cum.last().unwrap();
        cum.push(last + c.starts.len() as u32);
    }

    let chunks_r: &[Chunk] = &chunks;
    let post = merge_lists(chunks_r, nt, nthreads, false);
    let bpost = merge_lists(chunks_r, nb, nthreads, true);
    lap("merge");

    let nq = queries.len();
    let next = AtomicUsize::new(0);
    let (post_r, bpost_r, queries_r, cum_r) = (&post, &bpost, &queries, &cum);
    let outs: Vec<Vec<(usize, String)>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nthreads)
            .map(|_| {
                sc.spawn(|| {
                    let mut out = Vec::new();
                    let mut ids: Vec<u32> = Vec::new();
                    loop {
                        let s = next.fetch_add(32, Ordering::Relaxed);
                        if s >= nq {
                            break;
                        }
                        for i in s..(s + 32).min(nq) {
                            let r: Cow<[u32]> = match &queries_r[i] {
                                Q::Bool(t) => eval_bool(t, post_r),
                                Q::Phrase(q, bis) => Cow::Owned(eval_phrase(
                                    q, bis, bpost_r, chunks_r, cum_r, tab_ref, &mut ids,
                                )),
                            };
                            let sum: u64 = r.iter().map(|&x| x as u64).sum();
                            let mut line = String::with_capacity(48);
                            let _ = write!(line, "{} {}", r.len(), sum);
                            for x in r.iter().take(10) {
                                let _ = write!(line, " {}", x);
                            }
                            line.push('\n');
                            out.push((i, line));
                        }
                    }
                    out
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    lap("answer");

    let mut lines = vec![String::new(); nq];
    for part in outs {
        for (i, l) in part {
            lines[i] = l;
        }
    }
    let mut res = String::with_capacity(nq * 30);
    for l in &lines {
        res.push_str(l);
    }
    std::fs::write(out_p, res).expect("write results");
    lap("write");
}