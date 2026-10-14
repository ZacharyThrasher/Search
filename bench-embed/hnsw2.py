"""HNSW recall with a tie-tolerant metric + ef_search sweep (usearch f16), jittered replicas.
usage: hnsw2.py DOCS.f32 QUERIES.f32 DIM N
'near' recall = share of returned results whose true cosine >= (exact 10th best cosine - 0.01).
"""
import sys, time, json, statistics
import numpy as np
from usearch.index import Index
docs = np.fromfile(sys.argv[1], dtype="<f4"); dim = int(sys.argv[3]); docs = docs.reshape(-1, dim)
qs = np.fromfile(sys.argv[2], dtype="<f4").reshape(-1, dim)[::7][:200]
n = int(sys.argv[4]); rng = np.random.default_rng(1)
reps = -(-n // len(docs)); v = np.tile(docs, (reps, 1))[:n].copy()
v[len(docs):] += rng.normal(0, 0.025, size=(n - len(docs), dim)).astype("f4")
v /= np.linalg.norm(v, axis=1, keepdims=True)
tk, tth = [], []
for i in range(0, len(qs), 20):
    s = qs[i:i + 20] @ v.T
    o = np.argsort(-s, axis=1)[:, :10]
    tk.extend(o); tth.extend(np.take_along_axis(s, o, 1)[:, -1])
idx = Index(ndim=dim, metric="ip", dtype="f16", connectivity=16, expansion_add=128)
t0 = time.time(); idx.add(np.arange(n), v, threads=4); print(json.dumps(dict(n=n, build_s=round(time.time() - t0, 1), mem_mb=round(idx.memory_usage / 1e6))), flush=True)
for ef in (32, 64, 128, 256, 512):
    idx.expansion_search = ef
    lat, ex, near = [], [], []
    for qi, q in enumerate(qs):
        t = time.perf_counter(); m = idx.search(q, 10); lat.append((time.perf_counter() - t) * 1e3)
        keys = m.keys.astype(int)
        ex.append(len(set(keys.tolist()) & set(tk[qi].tolist())) / 10)
        sc = v[keys] @ q
        near.append(float(np.mean(sc >= tth[qi] - 0.01)))
    print(json.dumps(dict(n=n, ef=ef, ms_p50=round(statistics.median(lat), 3), exact_recall=round(float(np.mean(ex)), 3), near_recall=round(float(np.mean(near)), 3))), flush=True)
