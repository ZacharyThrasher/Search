"""Concatenate cached SciFact+NFCorpus embeddings of one model into raw f32 files for vecstore-bench.
usage: export.py EMB_DIR TAG OUT_PREFIX     (TAG like bge-small.model)"""
import sys, numpy as np
emb, tag, out = sys.argv[1:4]
D = np.concatenate([np.load(f"{emb}/{ds}.{tag}.docs.npy") for ds in sys.argv[4].split(",")])
Q = np.concatenate([np.load(f"{emb}/{ds}.{tag}.q.npy") for ds in sys.argv[4].split(",")])
D.astype("<f4").tofile(out + ".docs.f32"); Q.astype("<f4").tofile(out + ".q.f32")
print(D.shape, Q.shape)
