"""Small ONNX Runtime embedding harness used by the step 5 research.

Mirrors what the Rust side would do with the `ort` crate: tokenizers JSON +
ONNX graph + pooling + L2 normalise. CPU execution provider only.
"""
import os, time, json
import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

BGE_Q = "Represent this sentence for searching relevant passages: "
# model key -> (query prefix, doc prefix, pooling)
SPECS = {
    "bge-small":  (BGE_Q, "", "cls"),
    "e5-small":   ("query: ", "passage: ", "mean"),
    "gte-small":  ("", "", "mean"),
    "arctic-s":   (BGE_Q, "", "cls"),
    "minilm-l6":  ("", "", "mean"),
    "mxbai-xs":   (BGE_Q, "", "mean"),
    "nomic-v1.5": ("search_query: ", "search_document: ", "mean"),
    "me5-small":  ("query: ", "passage: ", "mean"),
}


class Embedder:
    def __init__(self, root, key, variant="model", threads=4, maxlen=512):
        self.key, self.maxlen = key, maxlen
        self.qpre, self.dpre, self.pool = SPECS[key]
        d = os.path.join(root, key)
        self.tok = Tokenizer.from_file(os.path.join(d, "tokenizer.json"))
        self.tok.enable_truncation(max_length=maxlen)
        self.tok.enable_padding()  # pad to longest in batch
        so = ort.SessionOptions()
        so.intra_op_num_threads = threads
        so.inter_op_num_threads = 1
        so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        path = os.path.join(d, "onnx", variant + ".onnx")
        self.size_mb = os.path.getsize(path) / 1e6
        self.sess = ort.InferenceSession(path, so, providers=["CPUExecutionProvider"])
        self.inputs = [i.name for i in self.sess.get_inputs()]
        self.dim = None

    def _run(self, texts):
        enc = self.tok.encode_batch(texts)
        ids = np.array([e.ids for e in enc], dtype=np.int64)
        mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
        feed = {}
        for n in self.inputs:
            if n == "input_ids": feed[n] = ids
            elif n == "attention_mask": feed[n] = mask
            elif n == "token_type_ids": feed[n] = np.zeros_like(ids)
        out = self.sess.run(None, feed)[0]  # (B, T, H)
        if self.pool == "cls":
            v = out[:, 0]
        else:
            m = mask[..., None].astype(np.float32)
            v = (out * m).sum(1) / np.maximum(m.sum(1), 1)
        if self.key == "nomic-v1.5":  # layer norm before truncation/normalise
            v = (v - v.mean(-1, keepdims=True)) / np.sqrt(v.var(-1, keepdims=True) + 1e-5)
        v = v / np.maximum(np.linalg.norm(v, axis=1, keepdims=True), 1e-12)
        self.dim = v.shape[1]
        return v.astype(np.float32), int(mask.sum())

    def encode(self, texts, kind="doc", bs=16):
        pre = self.dpre if kind == "doc" else self.qpre
        texts = [pre + t for t in texts]
        order = np.argsort([len(t) for t in texts])  # batch similar lengths
        res = np.zeros((len(texts), 0), np.float32)
        out, toks = None, 0
        for i in range(0, len(texts), bs):
            idx = order[i:i + bs]
            v, t = self._run([texts[j] for j in idx])
            toks += t
            if out is None:
                out = np.zeros((len(texts), v.shape[1]), np.float32)
            out[idx] = v
        self.last_tokens = toks
        return out
