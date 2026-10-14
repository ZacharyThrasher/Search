"""Embedding speed / RAM for one model variant, in its own process.
usage: speed.py MODELS_DIR CHUNKS_JSONL QUERIES_GZ model[:variant]
Prints one JSON line.
"""
import sys, os, json, time, gzip, random, statistics
import numpy as np
from emb import Embedder

def rss_mb():
    for l in open("/proc/self/status"):
        if l.startswith("VmRSS"): return int(l.split()[1]) / 1024

def main():
    models, chunks_f, qf, spec = sys.argv[1:5]
    key, _, var = spec.partition(":"); var = var or "model"
    n_chunks = int(os.environ.get("N_CHUNKS", 320))
    chunks = [json.loads(l)["text"] for l in open(chunks_f)]
    random.Random(7).shuffle(chunks)
    chunks = chunks[:n_chunks]
    qs = [json.loads(l)["text"] for l in gzip.open(qf, "rt")][:200]
    random.Random(3).shuffle(qs); qs = qs[:60]
    r0 = rss_mb()
    row = dict(model=key, variant=var)
    for threads in (4, 1):
        e = Embedder(models, key, var, threads=threads, maxlen=384)
        row["size_mb"] = round(e.size_mb, 1)
        e.encode(chunks[:16], "doc")  # warm-up
        sub = chunks if threads == 4 else chunks[:max(48, n_chunks // 4)]
        t0 = time.time(); e.encode(sub, "doc", bs=16); dt = time.time() - t0
        row[f"chunks_per_s_t{threads}"] = round(len(sub) / dt, 1)
        row[f"tok_per_chunk"] = round(e.last_tokens / len(sub), 1)
        if threads == 4:
            lat = []
            for q in qs:
                t0 = time.perf_counter(); e.encode([q], "query", bs=1); lat.append((time.perf_counter() - t0) * 1000)
            lat = lat[5:]
            row["query_ms_p50_t4"] = round(statistics.median(lat), 1)
            row["query_ms_p95_t4"] = round(sorted(lat)[int(len(lat) * .95)], 1)
            row["rss_mb_after"] = round(rss_mb() - r0, 0)
        else:
            lat = []
            for q in qs[:30]:
                t0 = time.perf_counter(); e.encode([q], "query", bs=1); lat.append((time.perf_counter() - t0) * 1000)
            row["query_ms_p50_t1"] = round(statistics.median(lat[3:]), 1)
        row["dim"] = e.dim
        del e
    print(json.dumps(row), flush=True)

main()
