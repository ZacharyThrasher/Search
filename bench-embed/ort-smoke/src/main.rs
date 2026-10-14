//! Smoke test: run a sentence-embedding ONNX model in-process from Rust with
//! the `ort` crate (CPU), the same way step 5 would, and print chunks/s.
//!
//! usage: ort-smoke MODEL_DIR VARIANT CHUNKS.jsonl [N] [THREADS]
use ort::{session::Session, value::Tensor};
use std::time::Instant;
use tokenizers::{PaddingParams, Tokenizer, TruncationDirection, TruncationParams};

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|t| t.lines().find(|l| l.starts_with("VmRSS")).and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))).unwrap_or(0.0) / 1024.0
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    let (dir, variant, chunks_f) = (&a[1], &a[2], &a[3]);
    let n: usize = a.get(4).map(|s| s.parse().unwrap()).unwrap_or(160);
    let threads: usize = a.get(5).map(|s| s.parse().unwrap()).unwrap_or(4);
    let mut tok = Tokenizer::from_file(format!("{dir}/tokenizer.json")).map_err(|e| e.to_string())?;
    tok.with_truncation(Some(TruncationParams { max_length: 384, direction: TruncationDirection::Right, ..Default::default() })).map_err(|e| e.to_string())?;
    tok.with_padding(Some(PaddingParams::default()));
    let texts: Vec<String> = std::fs::read_to_string(chunks_f)?
        .lines()
        .take(n)
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["text"].as_str().unwrap().to_string())
        .collect();
    let env = ort::environment::init().build()?;
    let mut session = Session::builder(&env)?
        .with_intra_threads(threads)?
        .commit_from_file(format!("{dir}/onnx/{variant}.onnx"))?;
    let names: Vec<String> = session.inputs().iter().map(|i| i.name().to_string()).collect();
    println!("inputs: {names:?}; RSS after model load: {:.0} MB", rss_mb());
    let mut run = |batch: &[String]| -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let enc = tok.encode_batch(batch.to_vec(), true).map_err(|e| e.to_string())?;
        let (b, t) = (enc.len(), enc[0].get_ids().len());
        let ids: Vec<i64> = enc.iter().flat_map(|e| e.get_ids().iter().map(|&x| x as i64)).collect();
        let mask: Vec<i64> = enc.iter().flat_map(|e| e.get_attention_mask().iter().map(|&x| x as i64)).collect();
        let tt = vec![0i64; b * t];
        let mut inputs: Vec<(String, ort::session::SessionInputValue)> = vec![];
        for nm in &names {
            let v = match nm.as_str() {
                "input_ids" => ids.clone(),
                "attention_mask" => mask.clone(),
                _ => tt.clone(),
            };
            inputs.push((nm.clone(), Tensor::from_array(([b, t], v))?.into()));
        }
        let out = session.run(inputs)?;
        let (shape, data) = out[0].try_extract_tensor::<f32>()?;
        let h = shape[2] as usize;
        // CLS pooling + L2 normalise (bge style)
        let mut res = vec![];
        for i in 0..b {
            let v = &data[i * t * h..i * t * h + h];
            let nrm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            res.extend(v.iter().map(|x| x / nrm));
        }
        Ok(res)
    };
    if n == 0 { let q = vec!["how do i search inside pdf files".to_string()]; let t = Instant::now(); for _ in 0..50 { run(&q)?; } println!("query-only: {:.1} ms/query, RSS {:.0} MB", t.elapsed().as_secs_f64()*1000.0/50.0, rss_mb()); return Ok(()); }
    run(&texts[..16])?; // warm-up
    let t0 = Instant::now();
    let mut first = vec![];
    for (i, ch) in texts.chunks(16).enumerate() {
        let r = run(ch)?;
        if i == 0 {
            first = r;
        }
    }
    let dt = t0.elapsed().as_secs_f64();
    println!("{} chunks in {:.1}s = {:.1} chunks/s ({threads} threads); first vec[0..4] = {:?}", texts.len(), dt, texts.len() as f64 / dt, &first[..4]);
    let q = vec!["how do i search inside pdf files".to_string()];
    let t = Instant::now();
    for _ in 0..20 {
        run(&q)?;
    }
    println!("single query: {:.1} ms; RSS after queries: {:.0} MB", t.elapsed().as_secs_f64() * 1000.0 / 20.0, rss_mb());
    Ok(())
}
