//! Brute-force vector scan benchmark for the step 5 study.
//!
//! usage: vecstore-bench DOCS.f32 QUERIES.f32 DIM [sizes...]
//!
//! DOCS.f32 / QUERIES.f32 are raw little-endian f32 matrices of L2-normalised
//! embeddings (real ones, written by `export.py`). Bigger sets are made by
//! "jittered replicas": every vector is a real one plus Gaussian noise, then
//! re-normalised, so timings are real but recall at large N is only indicative.
//!
//! Measured per size: f32 / int8 / binary+int8-rescore scan latency (all
//! cores), memory, and recall@10 against the exact f32 scan.

use rayon::prelude::*;
use std::time::Instant;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

const K: usize = 10;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn gauss(&mut self) -> f32 {
        let (u1, u2) = (self.f().max(1e-7), self.f());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }
}

fn read_f32(p: &str) -> Vec<f32> {
    let b = std::fs::read(p).unwrap();
    b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    v.iter_mut().for_each(|x| *x /= n);
}

// ------------------------------------------------------------------ kernels

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_f32_avx2(a: &[f32], b: &[f32]) -> f32 {
    unsafe {
        let mut s0 = _mm256_setzero_ps();
        let mut s1 = _mm256_setzero_ps();
        let mut i = 0;
        while i + 16 <= a.len() {
            s0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), _mm256_loadu_ps(b.as_ptr().add(i)), s0);
            s1 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i + 8)), _mm256_loadu_ps(b.as_ptr().add(i + 8)), s1);
            i += 16;
        }
        let s = _mm256_add_ps(s0, s1);
        let mut t = [0f32; 8];
        _mm256_storeu_ps(t.as_mut_ptr(), s);
        t.iter().sum()
    }
}

/// i8 x i8 dot, values in [-127, 127]: |a| as unsigned, b negated where a<0.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_i8_avx2(a: &[i8], b: &[i8]) -> i32 {
    unsafe {
        let ones = _mm256_set1_epi16(1);
        let mut acc = _mm256_setzero_si256();
        let mut i = 0;
        while i + 32 <= a.len() {
            let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
            let vb = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            let ua = _mm256_sign_epi8(va, va);
            let sb = _mm256_sign_epi8(vb, va);
            let p = _mm256_maddubs_epi16(ua, sb);
            acc = _mm256_add_epi32(acc, _mm256_madd_epi16(p, ones));
            i += 32;
        }
        let mut t = [0i32; 8];
        _mm256_storeu_si256(t.as_mut_ptr() as *mut __m256i, acc);
        t.iter().sum()
    }
}

fn hamming(a: &[u64], b: &[u64]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

// ------------------------------------------------------------------- top-k

#[derive(Clone)]
struct Top<T: Copy + PartialOrd> {
    k: usize,
    v: Vec<(T, u32)>, // unsorted, worst tracked by scan when full (k small)
    worst: usize,
    higher_better: bool,
}
impl<T: Copy + PartialOrd> Top<T> {
    fn new(k: usize, higher_better: bool) -> Self {
        Top { k, v: Vec::with_capacity(k), worst: 0, higher_better }
    }
    fn better(&self, a: T, b: T) -> bool {
        if self.higher_better { a > b } else { a < b }
    }
    fn push(&mut self, s: T, id: u32) {
        if self.v.len() < self.k {
            self.v.push((s, id));
            if self.v.len() == self.k {
                self.recompute();
            }
        } else if self.better(s, self.v[self.worst].0) {
            self.v[self.worst] = (s, id);
            self.recompute();
        }
    }
    fn recompute(&mut self) {
        let mut w = 0;
        for i in 1..self.v.len() {
            if self.better(self.v[w].0, self.v[i].0) {
                w = i;
            }
        }
        self.worst = w;
    }
    fn sorted(mut self) -> Vec<(T, u32)> {
        let hb = self.higher_better;
        self.v.sort_by(|a, b| if hb { b.0.partial_cmp(&a.0).unwrap() } else { a.0.partial_cmp(&b.0).unwrap() });
        self.v
    }
}

fn merge<T: Copy + PartialOrd>(parts: Vec<Top<T>>, k: usize, hb: bool) -> Vec<(T, u32)> {
    let mut t = Top::new(k, hb);
    for p in parts {
        for (s, id) in p.v {
            t.push(s, id);
        }
    }
    t.sorted()
}

// ------------------------------------------------------------------ stores

struct Store {
    dim: usize,
    n: usize,
    f32s: Vec<f32>,
    i8s: Vec<i8>,
    scale: f32,
    bits: Vec<u64>,
    words: usize,
}

impl Store {
    fn build(vecs: Vec<f32>, dim: usize) -> Store {
        let n = vecs.len() / dim;
        let maxabs = vecs.iter().fold(0f32, |m, x| m.max(x.abs()));
        let scale = 127.0 / maxabs;
        let i8s: Vec<i8> = vecs.iter().map(|x| (x * scale).round().clamp(-127.0, 127.0) as i8).collect();
        let words = dim.div_ceil(64);
        let mut bits = vec![0u64; n * words];
        for r in 0..n {
            for d in 0..dim {
                if vecs[r * dim + d] > 0.0 {
                    bits[r * words + d / 64] |= 1 << (d % 64);
                }
            }
        }
        Store { dim, n, f32s: vecs, i8s, scale, bits, words }
    }

    fn q_i8(&self, q: &[f32]) -> Vec<i8> {
        q.iter().map(|x| (x * self.scale).round().clamp(-127.0, 127.0) as i8).collect()
    }
    fn q_bits(&self, q: &[f32]) -> Vec<u64> {
        let mut b = vec![0u64; self.words];
        for (d, x) in q.iter().enumerate() {
            if *x > 0.0 {
                b[d / 64] |= 1 << (d % 64);
            }
        }
        b
    }

    fn scan_f32(&self, q: &[f32]) -> Vec<(f32, u32)> {
        let dim = self.dim;
        let parts: Vec<Top<f32>> = self
            .f32s
            .par_chunks(dim * 8192)
            .enumerate()
            .map(|(ci, ch)| {
                let mut t = Top::new(K, true);
                for (r, row) in ch.chunks_exact(dim).enumerate() {
                    let s = unsafe { dot_f32_avx2(row, q) };
                    t.push(s, (ci * 8192 + r) as u32);
                }
                t
            })
            .collect();
        merge(parts, K, true)
    }

    fn scan_i8(&self, q: &[f32]) -> Vec<(i32, u32)> {
        let dim = self.dim;
        let qi = self.q_i8(q);
        let parts: Vec<Top<i32>> = self
            .i8s
            .par_chunks(dim * 8192)
            .enumerate()
            .map(|(ci, ch)| {
                let mut t = Top::new(K, true);
                for (r, row) in ch.chunks_exact(dim).enumerate() {
                    let s = unsafe { dot_i8_avx2(row, &qi) };
                    t.push(s, (ci * 8192 + r) as u32);
                }
                t
            })
            .collect();
        merge(parts, K, true)
    }

    /// Hamming scan for `cand` candidates, then i8 rescoring.
    fn scan_bin(&self, q: &[f32], cand: usize) -> Vec<(i32, u32)> {
        let w = self.words;
        let qb = self.q_bits(q);
        let parts: Vec<Top<u32>> = self
            .bits
            .par_chunks(w * 8192)
            .enumerate()
            .map(|(ci, ch)| {
                let mut t = Top::new(cand, false);
                for (r, row) in ch.chunks_exact(w).enumerate() {
                    t.push(hamming(row, &qb), (ci * 8192 + r) as u32);
                }
                t
            })
            .collect();
        let c = merge(parts, cand, false);
        let qi = self.q_i8(q);
        let mut t = Top::new(K, true);
        for (_, id) in c {
            let row = &self.i8s[id as usize * self.dim..(id as usize + 1) * self.dim];
            t.push(unsafe { dot_i8_avx2(row, &qi) }, id);
        }
        t.sorted()
    }
}

fn recall(truth: &[(f32, u32)], got: &[u32]) -> f32 {
    truth.iter().filter(|(_, id)| got.contains(id)).count() as f32 / truth.len() as f32
}

fn med(v: &mut Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let docs = read_f32(&a[1]);
    let queries = read_f32(&a[2]);
    let dim: usize = a[3].parse().unwrap();
    let sizes: Vec<usize> = a[4..].iter().map(|s| s.parse().unwrap()).collect();
    let base_n = docs.len() / dim;
    let nq = queries.len() / dim;
    eprintln!("real docs {base_n}, queries {nq}, dim {dim}, threads {}", rayon::current_num_threads());
    for &n in &sizes {
        // Build n vectors: the real ones first, then jittered replicas.
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let mut v = Vec::with_capacity(n * dim);
        for i in 0..n {
            let src = &docs[(i % base_n) * dim..(i % base_n + 1) * dim];
            if i < base_n {
                v.extend_from_slice(src);
            } else {
                let mut x: Vec<f32> = src.iter().map(|s| s + rng.gauss() * 0.025).collect();
                normalize(&mut x);
                v.extend_from_slice(&x);
            }
        }
        let t0 = Instant::now();
        let st = Store::build(v, dim);
        let build_s = t0.elapsed().as_secs_f64();
        let mut lat = [vec![], vec![], vec![], vec![]];
        let (mut r_i8, mut r_b100, mut r_b300) = (0f32, 0f32, 0f32);
        let reps = nq.min(60);
        for qi in 0..reps {
            let q = &queries[qi * dim..(qi + 1) * dim];
            let t = Instant::now();
            let truth = st.scan_f32(q);
            lat[0].push(t.elapsed().as_secs_f64() * 1e3);
            let t = Instant::now();
            let r = st.scan_i8(q);
            lat[1].push(t.elapsed().as_secs_f64() * 1e3);
            r_i8 += recall(&truth, &r.iter().map(|x| x.1).collect::<Vec<_>>());
            let t = Instant::now();
            let r = st.scan_bin(q, 100);
            lat[2].push(t.elapsed().as_secs_f64() * 1e3);
            r_b100 += recall(&truth, &r.iter().map(|x| x.1).collect::<Vec<_>>());
            let t = Instant::now();
            let r = st.scan_bin(q, 300);
            lat[3].push(t.elapsed().as_secs_f64() * 1e3);
            r_b300 += recall(&truth, &r.iter().map(|x| x.1).collect::<Vec<_>>());
        }
        let f = reps as f32;
        let mb = |b: usize| (b as f64 / 1e6 * 10.0).round() / 10.0;
        let row = serde_json::json!({
            "n": n, "dim": dim, "threads": rayon::current_num_threads(),
            "f32_ms_p50": med(&mut lat[0]), "i8_ms_p50": med(&mut lat[1]),
            "bin100_ms_p50": med(&mut lat[2]), "bin300_ms_p50": med(&mut lat[3]),
            "recall10_i8": r_i8 / f, "recall10_bin100": r_b100 / f, "recall10_bin300": r_b300 / f,
            "mb_f32": mb(n * dim * 4), "mb_i8": mb(n * dim), "mb_bits": mb(n * st.words * 8),
            "mb_per_million_f32": mb(1_000_000 * dim * 4), "mb_per_million_i8": mb(1_000_000 * dim),
            "mb_per_million_bin_plus_i8": mb(1_000_000 * (st.words * 8 + dim)),
            "build_s": build_s,
        });
        println!("{row}");
    }
}
