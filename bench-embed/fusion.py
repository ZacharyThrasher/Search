"""Fusion study on cached embeddings: RRF (k, weights) vs normalised-score blend.
usage: fusion.py DATA_DIR EMB_DIR DATASET TAG [TAG...]     (TAG like bge-small.model)
"""
import sys, json, numpy as np
from quality import load, bm25_scores, ndcg10, recall, rrf

data, emb, ds = sys.argv[1:4]
corpus, queries, qrels = load(data, ds)
ids = list(corpus); qids = list(queries)
bm = bm25_scores(ids, corpus, queries)

def z(x):
    return (x - x.mean()) / (x.std() + 1e-9)

for tag in sys.argv[4:]:
    D = np.load(f"{emb}/{ds}.{tag}.docs.npy"); Q = np.load(f"{emb}/{ds}.{tag}.q.npy")
    S = Q @ D.T
    def ev(f):
        nd, rc = [], []
        for n, q in enumerate(qids):
            r = f(n, q)
            nd.append(ndcg10(r, qrels[q])); rc.append(recall(r, qrels[q], 10))
        return round(float(np.mean(nd)), 4), round(float(np.mean(rc)), 4)
    top = lambda s, n=100: [ids[i] for i in np.argsort(-s)[:n]]
    out = {"tag": tag, "ds": ds}
    out["vec"] = ev(lambda n, q: top(S[n]))
    out["bm25"] = ev(lambda n, q: top(bm[q]))
    for k in (10, 60):
        out[f"rrf_k{k}"] = ev(lambda n, q: rrf([top(S[n]), top(bm[q])], k=k))
    for w in (0.6, 0.75):  # vector weight
        out[f"rrf_k60_vec{w}"] = ev(lambda n, q: rrf([top(S[n]), top(bm[q])], k=60, weights=[w, 1 - w]))
    for a in (0.5, 0.7, 0.85):
        out[f"blend_z_vec{a}"] = ev(lambda n, q: top(a * z(S[n]) + (1 - a) * z(bm[q])))
    print(json.dumps(out), flush=True)
