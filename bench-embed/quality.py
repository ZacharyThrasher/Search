"""Retrieval-quality + throughput on a BEIR subset (SciFact, NFCorpus).
usage: quality.py MODELS_DIR DATA_DIR OUT_DIR model[:variant] ... [--threads N]
Caches embeddings (npy) so the vector-store study can reuse them.
"""
import sys, os, json, gzip, time, re, resource
import numpy as np
from emb import Embedder

def load(data, name):
    d = os.path.join(data, name)
    corpus = {}
    for l in gzip.open(os.path.join(d, "corpus.jsonl.gz"), "rt"):
        r = json.loads(l); corpus[r["_id"]] = (r.get("title", "") + ". " + r["text"]).strip(". ")
    queries = {}
    for l in gzip.open(os.path.join(d, "queries.jsonl.gz"), "rt"):
        r = json.loads(l); queries[r["_id"]] = r["text"]
    qrels = {}
    q = os.path.join(data, name + "-qrels", "test.tsv")
    for l in open(q):
        a = l.rstrip("\n").split("\t")
        if a[0] == "query-id": continue
        if int(a[2]) > 0: qrels.setdefault(a[0], {})[a[1]] = int(a[2])
    queries = {k: v for k, v in queries.items() if k in qrels}
    return corpus, queries, qrels

def ndcg10(ranked, rel):
    dcg = sum(rel.get(d, 0) / np.log2(i + 2) for i, d in enumerate(ranked[:10]))
    ideal = sorted(rel.values(), reverse=True)[:10]
    idcg = sum(r / np.log2(i + 2) for i, r in enumerate(ideal))
    return dcg / idcg if idcg else 0

def recall(ranked, rel, k=10):
    return len(set(ranked[:k]) & set(rel)) / len(rel)

def bm25_scores(corpus_ids, corpus, queries):
    from rank_bm25 import BM25Okapi
    tok = lambda s: re.findall(r"\w+", s.lower())
    bm = BM25Okapi([tok(corpus[i]) for i in corpus_ids])
    return {qid: bm.get_scores(tok(q)) for qid, q in queries.items()}

def rrf(lists, k=60, weights=None):
    sc = {}
    for li, l in enumerate(lists):
        w = 1 if weights is None else weights[li]
        for r, d in enumerate(l):
            sc[d] = sc.get(d, 0) + w / (k + r + 1)
    return [d for d, _ in sorted(sc.items(), key=lambda x: -x[1])]

if __name__ == "__main__":
    models, data, outd = sys.argv[1:4]
    specs = [a for a in sys.argv[4:] if not a.startswith("--")]
    threads = int(sys.argv[sys.argv.index("--threads") + 1]) if "--threads" in sys.argv else 4
    specs = [a for a in specs if not a.isdigit() and not a.startswith("scifact") and a != "nfcorpus"]
    os.makedirs(outd, exist_ok=True)
    dss = sys.argv[sys.argv.index("--ds") + 1].split(",") if "--ds" in sys.argv else ["scifact", "nfcorpus"]
    for ds in dss:
        corpus, queries, qrels = load(data, ds)
        ids = list(corpus); qids = list(queries)
        bm = bm25_scores(ids, corpus, queries)
        bm_rank = {q: [ids[i] for i in np.argsort(-bm[q])[:100]] for q in qids}
        res = {"bm25": (np.mean([ndcg10(bm_rank[q], qrels[q]) for q in qids]), np.mean([recall(bm_rank[q], qrels[q]) for q in qids]))}
        print(f"[{ds}] docs={len(ids)} queries={len(qids)} BM25 ndcg@10={res['bm25'][0]:.4f}", flush=True)
        for spec in specs:
            key, _, var = spec.partition(":"); var = var or "model"
            tag = f"{ds}.{key}.{var}"
            fe, fq = os.path.join(outd, tag + ".docs.npy"), os.path.join(outd, tag + ".q.npy")
            e = Embedder(models, key, var, threads=threads, maxlen=384)
            if os.path.exists(fe):
                D, Q, dt = np.load(fe), np.load(fq), None
            else:
                t0 = time.time(); D = e.encode([corpus[i] for i in ids], "doc", bs=16); dt = time.time() - t0
                tokens = e.last_tokens
                Q = e.encode([queries[q] for q in qids], "query", bs=16)
                np.save(fe, D); np.save(fq, Q)
            S = Q @ D.T
            vr = {q: [ids[i] for i in np.argsort(-S[n])[:100]] for n, q in enumerate(qids)}
            fu = {q: rrf([vr[q], bm_rank[q]])[:100] for q in qids}
            m = lambda r: (round(float(np.mean([ndcg10(r[q], qrels[q]) for q in qids])), 4), round(float(np.mean([recall(r[q], qrels[q]) for q in qids])), 4))
            row = dict(model=key, variant=var, dataset=ds, dim=int(D.shape[1]), vec=m(vr), bm25=m(bm_rank), rrf=m(fu),
                       docs_per_s=(round(len(ids) / dt, 1) if dt else None), tok_per_s=(round(tokens / dt) if dt else None),
                       size_mb=round(e.size_mb, 1), threads=threads)
            print(json.dumps(row), flush=True)
            with open(os.path.join(outd, "quality.jsonl"), "a") as f: f.write(json.dumps(row) + "\n")
