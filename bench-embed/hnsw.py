"""HNSW (usearch) build time, memory, latency and recall at scale, on jittered replicas of real embeddings.
usage: hnsw.py DOCS.f32 QUERIES.f32 DIM N [N...]     -> JSON lines
"""
import sys, time, json, statistics
import numpy as np
from usearch.index import Index

docs = np.fromfile(sys.argv[1], dtype="<f4"); dim = int(sys.argv[3]); docs = docs.reshape(-1, dim)
qs = np.fromfile(sys.argv[2], dtype="<f4").reshape(-1, dim)[::7][:200]
rng = np.random.default_rng(1)

def replicas(n):
    reps = -(-n // len(docs))
    v = np.tile(docs, (reps, 1))[:n].copy()
    v[len(docs):] += rng.normal(0, 0.025, size=(n - len(docs), dim)).astype("f4")
    v /= np.linalg.norm(v, axis=1, keepdims=True)
    return v

for n in map(int, sys.argv[4:]):
    v = replicas(n)
    # exact ground truth
    truth = []
    for i in range(0, len(qs), 20):
        s = qs[i:i + 20] @ v.T
        truth.extend(np.argsort(-s, axis=1)[:, :10])
    for dtype in ("f16", "i8"):
        idx = Index(ndim=dim, metric="ip", dtype=dtype, connectivity=16, expansion_add=128, expansion_search=64)
        t0 = time.time(); idx.add(np.arange(n), v, threads=4); build = time.time() - t0
        lat, rec = [], []
        for qi, q in enumerate(qs):
            t = time.perf_counter(); m = idx.search(q, 10); lat.append((time.perf_counter() - t) * 1e3)
            rec.append(len(set(m.keys.tolist()) & set(truth[qi].tolist())) / 10)
        print(json.dumps(dict(n=n, dtype=dtype, build_s=round(build, 1), build_vec_per_s=round(n / build),
              mem_mb=round(idx.memory_usage / 1e6, 1), ms_p50=round(statistics.median(lat), 3),
              ms_p95=round(sorted(lat)[int(.95 * len(lat))], 3), recall10=round(float(np.mean(rec)), 4))), flush=True)
