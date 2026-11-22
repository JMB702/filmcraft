//! Speed and accuracy measurements for the speech engines (docs/transcripts.md).
//!
//! ```text
//! speech_bench asr  <models dir> <model id> <samples dir>
//! speech_bench diar <models dir> <model id> <conversations dir> [threshold…]
//! speech_bench fetch <models dir> <model id> -
//! ```
//!
//! `asr`: every `<name>.f32` (mono 16 kHz little-endian f32) with a reference `<name>.txt` under
//! the samples dir (recursively); prints the word error rate and the compute time per minute of
//! audio. `diar`: every `<name>.f32` with `<name>.spk` (lines `start_s end_s speaker`); each
//! conversation is one job; prints the share of words given the wrong speaker (after mapping
//! clusters to speakers by overlap) and how often the speaker count was wrong, for each
//! clustering threshold. `FILMCRAFT_SPEECH_CPU=1` measures the CPU instead of the GPU.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use filmcraft_speech::speakers::{self, Clip, Embedder, Params};
use filmcraft_speech::{Options, SpeechError, models, word_error_rate};

fn files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, ext, out);
        } else if p.extension().is_some_and(|x| x == "f32") && p.with_extension(ext).exists() {
            out.push(p);
        }
    }
}

fn read_f32(p: &Path) -> Vec<f32> {
    std::fs::read(p).map(|b| b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()).unwrap_or_default()
}

/// Remembers embeddings by the audio they came from, so thresholds can be swept cheaply.
struct Cache<'a> {
    inner: &'a dyn Embedder,
    seen: Mutex<HashMap<(usize, u64), Vec<f32>>>,
}

impl Embedder for Cache<'_> {
    fn id(&self) -> String {
        self.inner.id()
    }
    fn embed(&self, audio: &[f32]) -> Result<Vec<f32>, SpeechError> {
        let key = (audio.len(), audio.iter().fold(0u64, |h, v| h.rotate_left(5) ^ v.to_bits() as u64));
        if let Some(v) = self.seen.lock().unwrap().get(&key) {
            return Ok(v.clone());
        }
        let v = self.inner.embed(audio)?;
        self.seen.lock().unwrap().insert(key, v.clone());
        Ok(v)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(mode), Some(models_dir), Some(id), Some(dir)) = (args.get(1), args.get(2), args.get(3), args.get(4)) else {
        eprintln!("usage: speech_bench asr|diar <models dir> <model id> <dir> [threshold…]");
        std::process::exit(2);
    };
    let models_dir = Path::new(models_dir);
    if mode == "fetch" {
        let m = models::find(id).ok_or("unknown model")?;
        models::download(models_dir, m, &mut |d, t, f| {
            eprint!("\r{f} {:.0}/{:.0} MB   ", d as f64 / 1e6, t as f64 / 1e6);
            true
        })?;
        eprintln!("\n{id} installed: {}", models::installed(models_dir, m));
        return Ok(());
    }
    if mode == "emb" {
        // same-speaker vs different-speaker cosine over LibriSpeech utterances (speaker = name prefix)
        let e = filmcraft_speech::load_embedder(models_dir, id)?;
        let mut list = Vec::new();
        files(Path::new(dir), "txt", &mut list);
        list.sort();
        let mut embs = Vec::new();
        for p in list.iter().take(60) {
            let spk = p.file_name().and_then(|f| f.to_str()).and_then(|f| f.split('-').next()).unwrap_or("").to_string();
            embs.push((spk, e.embed(&read_f32(p))?));
        }
        let report = |embs: &[(String, Vec<f32>)], label: &str| {
            let (mut same, mut diff) = (Vec::new(), Vec::new());
            for i in 0..embs.len() {
                for j in i + 1..embs.len() {
                    let c = speakers::cosine(&embs[i].1, &embs[j].1);
                    if embs[i].0 == embs[j].0 { same.push(c) } else { diff.push(c) }
                }
            }
            let m = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
            // equal error rate: threshold where false accepts = false rejects
            let mut best = (1.0f32, 0f32);
            for k in 0..200 {
                let th = -1.0 + k as f32 * 0.01;
                let fr = same.iter().filter(|&&c| c < th).count() as f32 / same.len().max(1) as f32;
                let fa = diff.iter().filter(|&&c| c >= th).count() as f32 / diff.len().max(1) as f32;
                if (fr - fa).abs() < best.0 {
                    best = ((fr - fa).abs(), (fr + fa) / 2.0)
                }
            }
            println!("{label}: same {:.3} (n {}), different {:.3} (n {}), EER {:.1}%", m(&same), same.len(), m(&diff), diff.len(), best.1 * 100.0);
        };
        report(&embs, "raw");
        let d = embs[0].1.len();
        let mut mean = vec![0f32; d];
        for (_, e) in &embs {
            mean.iter_mut().zip(e).for_each(|(a, b)| *a += b / embs.len() as f32)
        }
        let centred: Vec<(String, Vec<f32>)> = embs
            .iter()
            .map(|(s, e)| {
                let mut v: Vec<f32> = e.iter().zip(&mean).map(|(a, b)| a - b).collect();
                speakers::normalize(&mut v);
                (s.clone(), v)
            })
            .collect();
        report(&centred, "centred");
        // 2-second excerpts
        let mut short = Vec::new();
        for p in list.iter().take(60) {
            let spk = p.file_name().and_then(|f| f.to_str()).and_then(|f| f.split('-').next()).unwrap_or("").to_string();
            let a = read_f32(p);
            short.push((spk, e.embed(&a[a.len().min(8000)..a.len().min(40000)])?));
        }
        report(&short, "2 s raw");
        return Ok(());
    }
    let t0 = Instant::now();
    let asr = filmcraft_speech::load(models_dir, id)?;
    eprintln!("loaded {id} in {:.1}s", t0.elapsed().as_secs_f64());
    let opts = Options { language: Some("en".into()), diarize: false, max_speakers: 8 };
    match mode.as_str() {
        "asr" => {
            let mut list = Vec::new();
            files(Path::new(dir), "txt", &mut list);
            list.sort();
            let (mut audio_s, mut compute_s, mut ref_words, mut errors) = (0f64, 0f64, 0f64, 0f64);
            for p in &list {
                let a = read_f32(p);
                let reference = std::fs::read_to_string(p.with_extension("txt"))?;
                let t = Instant::now();
                let tr = asr.transcribe(&a, &opts, &mut |_, _| true)?;
                let dt = t.elapsed().as_secs_f64();
                let n = reference.split_whitespace().count() as f64;
                let wer = word_error_rate(&reference, &tr.text());
                audio_s += a.len() as f64 / 16_000.0;
                compute_s += dt;
                ref_words += n;
                errors += wer * n;
                println!("{:5.1}% {:6.2}s {:6.2}s {}", wer * 100.0, a.len() as f64 / 16_000.0, dt, p.file_name().and_then(|f| f.to_str()).unwrap_or(""));
                if wer > 0.0 && std::env::var_os("SPEECH_BENCH_SHOW").is_some() {
                    println!("   ref: {}\n   hyp: {}", reference.trim(), tr.text());
                }
            }
            println!(
                "{id}: WER {:.2}% over {} words, {} files, {:.1} min of audio; {:.2} s of compute per minute of audio",
                errors / ref_words.max(1.0) * 100.0,
                ref_words,
                list.len(),
                audio_s / 60.0,
                compute_s / (audio_s / 60.0).max(1e-9)
            );
        }
        "diar" => {
            let emb_model = filmcraft_speech::load_embedder(models_dir, models::SPEAKER_MODEL)?;
            let cache = Cache { inner: emb_model.as_ref(), seen: Mutex::new(HashMap::new()) };
            let mut list = Vec::new();
            files(Path::new(dir), "spk", &mut list);
            list.sort();
            let mut convs = Vec::new();
            for p in &list {
                let a = read_f32(p);
                let turns: Vec<(f64, f64, String)> = std::fs::read_to_string(p.with_extension("spk"))?
                    .lines()
                    .filter_map(|l| {
                        let mut it = l.split_whitespace();
                        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.to_string()))
                    })
                    .collect();
                let tr = asr.transcribe(&a, &opts, &mut |_, _| true)?;
                convs.push((a, turns, tr));
            }
            let mut thresholds: Vec<f32> = args.iter().skip(5).filter_map(|s| s.parse().ok()).collect();
            if thresholds.is_empty() {
                thresholds.push(Params::default().threshold);
            }
            let mut all_embs: Vec<Vec<f32>> = Vec::new();
            for th in thresholds {
                let (mut words, mut wrong, mut count_wrong, mut embed_s) = (0usize, 0usize, 0usize, 0f64);
                for (a, turns, tr) in &convs {
                    let t = Instant::now();
                    let d = speakers::diarize(
                        &cache,
                        &[Clip { audio: a, words: &tr.words }],
                        &Params { threshold: th, centre: std::env::var_os("SPEECH_BENCH_RAW").is_none(), ..Default::default() },
                        &mut |_| true,
                    )?;
                    embed_s += t.elapsed().as_secs_f64();
                    all_embs.extend(d.centroids.iter().cloned());
                    let refs: Vec<String> = turns.iter().map(|t| t.2.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                    if d.centroids.len() != refs.len() {
                        count_wrong += 1;
                    }
                    // overlap counts cluster × reference speaker, then a greedy one-to-one map
                    let mut counts: HashMap<(u32, usize), usize> = HashMap::new();
                    let mut truth = Vec::new();
                    for (w, l) in tr.words.iter().zip(&d.labels[0]) {
                        let mid = (w.start.0 + w.end.0) as f64 / 2.0 / filmcraft_time::TICKS_PER_SECOND as f64;
                        let r = turns.iter().find(|t| mid >= t.0 - 0.15 && mid <= t.1 + 0.15).and_then(|t| refs.iter().position(|x| *x == t.2));
                        truth.push(r);
                        if let (Some(r), Some(l)) = (r, l) {
                            *counts.entry((*l, r)).or_default() += 1;
                        }
                    }
                    let mut pairs: Vec<((u32, usize), usize)> = counts.into_iter().collect();
                    pairs.sort_by_key(|p| std::cmp::Reverse(p.1));
                    let mut map: HashMap<u32, usize> = HashMap::new();
                    let mut used = std::collections::HashSet::new();
                    for ((l, r), _) in pairs {
                        if !map.contains_key(&l) && !used.contains(&r) {
                            map.insert(l, r);
                            used.insert(r);
                        }
                    }
                    for (r, l) in truth.iter().zip(&d.labels[0]) {
                        let Some(r) = r else { continue };
                        words += 1;
                        if l.and_then(|l| map.get(&l)) != Some(r) {
                            wrong += 1;
                        }
                    }
                }
                println!(
                    "threshold {th:.2}: word speaker error {:.2}% over {words} words; speaker count wrong in {count_wrong} of {} conversations; {:.1}s embedding+clustering",
                    wrong as f64 / words.max(1) as f64 * 100.0,
                    convs.len(),
                    embed_s
                );
            }
            // cohort mean of the cluster centroids (one per speaker per conversation)
            if std::env::var_os("SPEECH_BENCH_COHORT").is_some() {
                let d = all_embs.first().map(Vec::len).unwrap_or(0);
                let mut m = vec![0f32; d];
                for e in &all_embs {
                    m.iter_mut().zip(e).for_each(|(a, b)| *a += b / all_embs.len() as f32);
                }
                println!("cohort {m:?}");
            }
        }
        _ => eprintln!("unknown mode {mode}"),
    }
    Ok(())
}
