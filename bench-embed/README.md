# Step 5 research: embedding models and vector store

Throwaway benchmark code behind the step 5 report (not part of the product build).
Everything ran on a 4 vCPU Xeon @ 2.8 GHz with AVX-512 VNNI, 15 GB RAM, no GPU.

| File | What it does |
|---|---|
| `../examples/embed_chunk_stats.rs` | Runs the real step 4 extractor + chunker over folders: chunks per file by extension, bytes per chunk, share of unique chunks, and a sample dump for the speed test. |
| `emb.py` | ONNX Runtime embedding harness (tokenizers + pooling + L2 norm), same steps the Rust side will do. |
| `speed.py` | Chunks/s (4 and 1 threads), query latency, RSS for one model variant, on real chunks. |
| `quality.py` | nDCG@10 / recall@10 on BEIR SciFact and NFCorpus: vectors, BM25 and RRF. Caches embeddings. |
| `fusion.py` | RRF (k, weights) vs normalised score blend on cached embeddings. |
| `export.py` | Writes cached embeddings to raw f32 for `vecstore/`. |
| `vecstore/` | Brute-force scan benchmark: f32, int8, binary + int8 rescoring, 100k to 1M vectors. |
| `hnsw.py`, `hnsw2.py` | HNSW (usearch) build time, memory, latency, recall at 100k to 1M. |
| `ort-smoke/` | The same model run from Rust with the `ort` crate: chunks/s, query latency, resident memory. |
| `results/` | Raw JSON lines behind the report. |

Models come from Hugging Face ONNX exports (Xenova/*, Snowflake, sentence-transformers, mixedbread, nomic).
Large sets above 5k vectors are "jittered replicas" of real embeddings, so timings are real but recall at
scale is only indicative.
