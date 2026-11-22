//! Speaker labels from neural voice embeddings, across every clip of a transcription job.
//!
//! 1. **Segments.** Each clip's words are grouped into turns ([`crate::diarize::chunks`]): a new
//!    segment starts at a pause of [`Params::gap`] or when a segment reaches [`Params::max_len`];
//!    shorter pieces join their neighbour. The recogniser's word times are the voice activity
//!    detection: only audio under words is ever embedded.
//! 2. **Embeddings.** Every segment's audio (levelled to −20 dBFS) goes through the speaker model
//!    ([`Embedder`], TitaNet-large in the app) to a unit vector.
//! 3. **Clustering.** Segments of at least [`Params::min_cluster_len`] are clustered by average-linkage agglomeration on cosine similarity across **all clips of the
//!    job**, so "Speaker 1" is the same voice in every clip. Merging stops when the closest pair
//!    is less similar than [`Params::threshold`], or at exactly [`Params::speakers`] clusters when
//!    the count is given. Shorter segments take the cluster of the most similar centroid.
//! 4. **Names.** The caller matches cluster centroids against named voices ([`match_voices`]).

use filmcraft_project::Word;
use filmcraft_time::Tick;

use crate::{SAMPLE_RATE, SpeechError, TICKS_PER_SAMPLE};

/// A speaker embedding model.
pub trait Embedder: Send + Sync {
    /// Model id (`"titanet-large"`).
    fn id(&self) -> String;
    /// The unit-length embedding of 16 kHz mono speech.
    fn embed(&self, audio: &[f32]) -> Result<Vec<f32>, SpeechError>;
}

#[cfg(feature = "parakeet")]
impl Embedder for crate::titanet::TitaNet {
    fn id(&self) -> String {
        "titanet-large".into()
    }
    fn embed(&self, audio: &[f32]) -> Result<Vec<f32>, SpeechError> {
        crate::titanet::TitaNet::embed(self, audio)
    }
}

/// Tunables (defaults chosen on synthetic LibriSpeech conversations; see `docs/transcripts.md`).
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub gap: Tick,
    pub min_len: Tick,
    pub max_len: Tick,
    /// Segments shorter than this are labelled by their nearest cluster, not clustered.
    pub min_cluster_len: Tick,
    /// Cosine similarity below which clusters stay apart.
    pub threshold: f32,
    /// Exact number of speakers; `None` = decide from the threshold.
    pub speakers: Option<usize>,
    /// Most clusters kept in automatic mode.
    pub max_speakers: usize,
    /// Centre the embeddings on the job's mean before clustering (measured worse with TitaNet,
    /// whose raw embeddings of different speakers are already near orthogonal; kept for tests
    /// and other embedders).
    pub centre: bool,
}

fn ms(m: i64) -> Tick {
    Tick(m * TICKS_PER_SAMPLE * SAMPLE_RATE as i64 / 1000)
}

impl Default for Params {
    fn default() -> Self {
        Self { gap: ms(300), min_len: ms(1000), max_len: ms(3000), min_cluster_len: ms(900), threshold: 0.25, speakers: None, max_speakers: 12, centre: false }
    }
}

/// One clip of a job: its audio (16 kHz mono, word times relative to its first sample) and words.
pub struct Clip<'a> {
    pub audio: &'a [f32],
    pub words: &'a [Word],
}

/// The result of [`diarize`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diarization {
    /// Per clip, per word: the cluster index (None: the clip had no words to embed).
    pub labels: Vec<Vec<Option<u32>>>,
    /// Per cluster: the mean of its segments' embeddings (raw, unit length), numbered in order of
    /// first appearance (clip order, then time).
    pub centroids: Vec<Vec<f32>>,
    /// Per cluster: seconds of speech.
    pub seconds: Vec<f64>,
}

/// Scale `v` to unit length (a zero vector stays zero).
pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-12 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut ab, mut aa, mut bb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa <= 1e-12 || bb <= 1e-12 { 0.0 } else { ab / (aa.sqrt() * bb.sqrt()) }
}

fn mean(vs: &[&[f32]]) -> Vec<f32> {
    let d = vs.first().map(|v| v.len()).unwrap_or(0);
    let mut m = vec![0f32; d];
    for v in vs {
        for (a, b) in m.iter_mut().zip(v.iter()) {
            *a += b;
        }
    }
    let n = vs.len().max(1) as f32;
    m.iter_mut().for_each(|a| *a /= n);
    m
}

/// Most segments clustered together (the similarity matrix is N²).
const MAX_SEGMENTS: usize = 4000;

/// Average-linkage agglomerative clustering of unit vectors by cosine similarity. Returns a label
/// per vector (numbered by first appearance).
pub fn cluster(vs: &[Vec<f32>], threshold: f32, exact: Option<usize>, max: usize) -> Vec<usize> {
    let n = vs.len();
    if n == 0 {
        return Vec::new();
    }
    let mut sim = vec![0f32; n * n];
    for i in 0..n {
        for j in i + 1..n {
            let s = cosine(&vs[i], &vs[j]);
            sim[i * n + j] = s;
            sim[j * n + i] = s;
        }
    }
    let mut size = vec![1usize; n];
    let mut alive = vec![true; n];
    let mut owner: Vec<usize> = (0..n).collect();
    let row_best = |sim: &[f32], alive: &[bool], i: usize| -> (f32, usize) {
        let mut best = (f32::NEG_INFINITY, usize::MAX);
        for j in 0..n {
            if j != i && alive[j] && sim[i * n + j] > best.0 {
                best = (sim[i * n + j], j);
            }
        }
        best
    };
    let mut best: Vec<(f32, usize)> = (0..n).map(|i| row_best(&sim, &alive, i)).collect();
    let mut clusters = n;
    let target = exact.map(|k| k.max(1));
    loop {
        if clusters <= 1 || target.is_some_and(|k| clusters <= k) {
            break;
        }
        let Some((i, &(s, j))) = (0..n).filter(|&i| alive[i]).map(|i| (i, &best[i])).max_by(|a, b| a.1.0.total_cmp(&b.1.0)) else { break };
        if j == usize::MAX {
            break;
        }
        if target.is_none() && s < threshold && clusters <= max {
            break;
        }
        // merge j into i
        let (si, sj) = (size[i] as f32, size[j] as f32);
        for k in 0..n {
            if alive[k] && k != i && k != j {
                let v = (sim[i * n + k] * si + sim[j * n + k] * sj) / (si + sj);
                sim[i * n + k] = v;
                sim[k * n + i] = v;
            }
        }
        size[i] += size[j];
        alive[j] = false;
        owner.iter_mut().for_each(|o| {
            if *o == j {
                *o = i;
            }
        });
        clusters -= 1;
        for k in 0..n {
            if alive[k] && (k == i || best[k].1 == i || best[k].1 == j) {
                best[k] = row_best(&sim, &alive, k);
            } else if alive[k] && k != i && sim[k * n + i] > best[k].0 {
                best[k] = (sim[k * n + i], i);
            }
        }
    }
    // number by first appearance
    let mut map: Vec<(usize, usize)> = Vec::new();
    owner
        .iter()
        .map(|o| match map.iter().find(|(r, _)| r == o) {
            Some((_, l)) => *l,
            None => {
                map.push((*o, map.len()));
                map.len() - 1
            }
        })
        .collect()
}

/// Progress callback: `(fraction 0..1)`; return `false` to cancel.
pub type Progress<'a> = &'a mut dyn FnMut(f32) -> bool;

/// One embedded turn of a clip.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    /// Index of the clip in the job.
    pub clip: usize,
    /// The clip's words it covers.
    pub words: std::ops::Range<usize>,
    pub seconds: f64,
    /// Unit-length embedding.
    pub embedding: Vec<f32>,
}

/// Cut clip `index`'s words into turns and embed each (step 1 and 2). Jobs call this as each clip
/// is transcribed, so a clip's audio need not be kept for the clustering at the end.
pub fn embed_segments(model: &dyn Embedder, index: usize, clip: &Clip, p: &Params, progress: Progress) -> Result<Vec<Segment>, SpeechError> {
    let chunk_params = crate::diarize::Params { chunk_gap: p.gap, min_chunk: p.min_len, max_chunk: p.max_len, ..Default::default() };
    let t = filmcraft_project::Transcript { words: clip.words.to_vec(), ..Default::default() };
    let ranges = crate::diarize::chunks(&t, &chunk_params);
    let pad = (SAMPLE_RATE as usize) / 20;
    let mut out = Vec::with_capacity(ranges.len());
    for (k, r) in ranges.iter().enumerate() {
        if !progress(k as f32 / ranges.len().max(1) as f32) {
            return Err(SpeechError::Cancelled);
        }
        let (Some(first), Some(last)) = (clip.words.get(r.start), r.end.checked_sub(1).and_then(|e| clip.words.get(e))) else { continue };
        let a = (first.start.0 / TICKS_PER_SAMPLE).max(0) as usize;
        let b = (last.end.0 / TICKS_PER_SAMPLE).max(0) as usize;
        let a = a.saturating_sub(pad).min(clip.audio.len());
        let b = b.saturating_add(pad).clamp(a, clip.audio.len());
        let Some(audio) = clip.audio.get(a..b).filter(|x| !x.is_empty()) else { continue };
        let seconds = (last.end - first.start).0 as f64 / filmcraft_time::TICKS_PER_SECOND as f64;
        out.push(Segment { clip: index, words: r.clone(), seconds, embedding: model.embed(audio)? });
    }
    Ok(out)
}

/// Cluster the segments of every clip of a job (step 3). `words[c]` is how many words clip `c`
/// has.
pub fn label(segs: &[Segment], words: &[usize], p: &Params) -> Diarization {
    let mut out = Diarization { labels: words.iter().map(|&n| vec![None; n]).collect(), ..Default::default() };
    if segs.is_empty() {
        return out;
    }
    let dim = segs[0].embedding.len();
    let embs: Vec<&[f32]> = segs.iter().map(|s| if s.embedding.len() == dim { s.embedding.as_slice() } else { &[] }).collect();
    let centre = if p.centre && embs.len() >= 4 { mean(&embs) } else { vec![0.0; dim] };
    let centred: Vec<Vec<f32>> = embs
        .iter()
        .map(|e| {
            let mut v: Vec<f32> = (0..dim).map(|i| e.get(i).copied().unwrap_or(0.0) - centre[i]).collect();
            normalize(&mut v);
            v
        })
        .collect();
    // the segments clustered: long enough to carry a voice, at most MAX_SEGMENTS (the longest);
    // the rest take the cluster of the most similar centroid
    let min_s = p.min_cluster_len.0 as f64 / filmcraft_time::TICKS_PER_SECOND as f64;
    let mut long: Vec<usize> = (0..segs.len()).filter(|&i| segs[i].seconds >= min_s).collect();
    if long.is_empty() {
        long = (0..segs.len()).collect();
    }
    if long.len() > MAX_SEGMENTS {
        long.sort_by(|&a, &b| segs[b].seconds.total_cmp(&segs[a].seconds));
        long.truncate(MAX_SEGMENTS);
        long.sort_unstable();
    }
    let lab_long = cluster(&long.iter().map(|&i| centred[i].clone()).collect::<Vec<_>>(), p.threshold, p.speakers, p.max_speakers.max(1));
    let k = lab_long.iter().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut seg_label = vec![0usize; segs.len()];
    for (&i, &l) in long.iter().zip(&lab_long) {
        seg_label[i] = l;
    }
    let cents: Vec<Vec<f32>> = (0..k)
        .map(|c| {
            let mut m = mean(&long.iter().zip(&lab_long).filter(|(_, l)| **l == c).map(|(&i, _)| centred[i].as_slice()).collect::<Vec<_>>());
            normalize(&mut m);
            m
        })
        .collect();
    let is_long: std::collections::HashSet<usize> = long.iter().copied().collect();
    for i in 0..segs.len() {
        if !is_long.contains(&i) {
            seg_label[i] = (0..k).max_by(|&a, &b| cosine(&centred[i], &cents[a]).total_cmp(&cosine(&centred[i], &cents[b]))).unwrap_or(0);
        }
    }
    // renumber by first appearance (segments arrive in clip, then time order)
    let mut order: Vec<usize> = Vec::new();
    for &l in &seg_label {
        if !order.contains(&l) {
            order.push(l);
        }
    }
    let renum = |l: usize| order.iter().position(|&o| o == l).unwrap_or(0);
    out.centroids = vec![Vec::new(); order.len()];
    out.seconds = vec![0.0; order.len()];
    for (new, &old) in order.iter().enumerate() {
        let members: Vec<&[f32]> = (0..segs.len()).filter(|&i| seg_label[i] == old && embs[i].len() == dim).map(|i| embs[i]).collect();
        let mut m = mean(&members);
        normalize(&mut m);
        out.centroids[new] = m;
        out.seconds[new] = (0..segs.len()).filter(|&i| seg_label[i] == old).map(|i| segs[i].seconds).sum();
    }
    for (i, seg) in segs.iter().enumerate() {
        let l = renum(seg_label[i]) as u32;
        let Some(labels) = out.labels.get_mut(seg.clip) else { continue };
        for w in seg.words.clone() {
            if let Some(x) = labels.get_mut(w) {
                *x = Some(l);
            }
        }
    }
    // words outside every segment take their nearest labelled neighbour
    for labels in &mut out.labels {
        let mut last = None;
        for x in labels.iter_mut() {
            match x {
                Some(v) => last = Some(*v),
                None => *x = last,
            }
        }
        let mut next = None;
        for x in labels.iter_mut().rev() {
            match x {
                Some(v) => next = Some(*v),
                None => *x = next,
            }
        }
    }
    out
}

/// Label every word of every clip with a speaker cluster, clustering across all clips
/// ([`embed_segments`] for each clip, then [`label`]).
pub fn diarize(model: &dyn Embedder, clips: &[Clip], p: &Params, progress: Progress) -> Result<Diarization, SpeechError> {
    let mut segs = Vec::new();
    let n = clips.len().max(1) as f32;
    for (i, clip) in clips.iter().enumerate() {
        segs.extend(embed_segments(model, i, clip, p, &mut |f| progress((i as f32 + f) / n * 0.95))?);
    }
    let out = label(&segs, &clips.iter().map(|c| c.words.len()).collect::<Vec<_>>(), p);
    progress(1.0);
    Ok(out)
}

/// Match cluster centroids to known voices: for each centroid, the index of the known voice it
/// belongs to (cosine similarity of raw embeddings at least `threshold`), each voice used once,
/// best pairs first.
pub fn match_voices(centroids: &[Vec<f32>], known: &[&[f32]], threshold: f32) -> Vec<Option<usize>> {
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (i, c) in centroids.iter().enumerate() {
        for (j, k) in known.iter().enumerate() {
            if k.len() == c.len() {
                let s = cosine(c, k);
                if s >= threshold {
                    pairs.push((s, i, j));
                }
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out = vec![None; centroids.len()];
    let mut used = vec![false; known.len()];
    for (_, i, j) in pairs {
        if out[i].is_none() && !used[j] {
            out[i] = Some(j);
            used[j] = true;
        }
    }
    out
}

/// Similarity of raw embeddings above which a cluster is taken to be a known voice.
pub const VOICE_MATCH: f32 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seconds_tick;

    /// Embeds by the audio's constant level: each speaker in a test is a different level, so
    /// the embedding is a one-hot of the level bucket plus a small shared direction.
    struct Fake;
    impl Embedder for Fake {
        fn id(&self) -> String {
            "fake".into()
        }
        fn embed(&self, audio: &[f32]) -> Result<Vec<f32>, SpeechError> {
            let m = audio.iter().map(|v| v.abs()).fold(0f32, f32::max);
            let mut v = vec![0.05f32; 8];
            let bucket = ((m * 10.0).round() as usize).min(7);
            v[bucket] += 1.0;
            normalize(&mut v);
            Ok(v)
        }
    }

    fn clip(turns: &[(f64, f64, f32)]) -> (Vec<f32>, Vec<Word>) {
        let end = turns.iter().map(|t| t.1).fold(0.0, f64::max) + 1.0;
        let mut audio = vec![0f32; (end * 16_000.0) as usize];
        let mut words = Vec::new();
        for &(a, b, level) in turns {
            for s in (a * 16_000.0) as usize..(b * 16_000.0) as usize {
                audio[s] = level;
            }
            let mut t = a;
            while t + 0.3 <= b {
                words.push(Word::new("w", seconds_tick(t), seconds_tick(t + 0.3)));
                t += 0.35;
            }
        }
        (audio, words)
    }

    #[test]
    fn clusters_are_shared_across_clips() {
        let (a1, w1) = clip(&[(0.0, 2.0, 0.3), (2.6, 4.6, 0.6), (5.2, 7.2, 0.3)]);
        let (a2, w2) = clip(&[(0.0, 2.0, 0.6), (2.6, 4.6, 0.3)]);
        let clips = [Clip { audio: &a1, words: &w1 }, Clip { audio: &a2, words: &w2 }];
        let d = diarize(&Fake, &clips, &Params::default(), &mut |_| true).unwrap();
        assert_eq!(d.centroids.len(), 2);
        let first = |c: usize, t: f64| d.labels[c][w_at(if c == 0 { &w1 } else { &w2 }, t)];
        assert_eq!(first(0, 0.1), Some(0));
        assert_eq!(first(0, 2.7), Some(1));
        assert_eq!(first(0, 5.3), Some(0));
        assert_eq!(first(1, 0.1), Some(1));
        assert_eq!(first(1, 2.7), Some(0));
        assert!(d.labels.iter().flatten().all(Option::is_some));
    }

    fn w_at(words: &[Word], t: f64) -> usize {
        words.iter().position(|w| w.start >= seconds_tick(t)).unwrap()
    }

    #[test]
    fn an_exact_count_is_honoured() {
        let (a, w) = clip(&[(0.0, 2.0, 0.1), (2.6, 4.6, 0.2), (5.2, 7.2, 0.6)]);
        let clips = [Clip { audio: &a, words: &w }];
        let p = Params { speakers: Some(1), ..Default::default() };
        let d = diarize(&Fake, &clips, &p, &mut |_| true).unwrap();
        assert_eq!(d.centroids.len(), 1);
        let p = Params { speakers: Some(3), ..Default::default() };
        assert_eq!(diarize(&Fake, &clips, &p, &mut |_| true).unwrap().centroids.len(), 3);
    }

    #[test]
    fn empty_and_cancelled_jobs() {
        let d = diarize(&Fake, &[Clip { audio: &[], words: &[] }], &Params::default(), &mut |_| true).unwrap();
        assert_eq!(d.labels, vec![Vec::<Option<u32>>::new()]);
        let (a, w) = clip(&[(0.0, 2.0, 0.3)]);
        assert_eq!(diarize(&Fake, &[Clip { audio: &a, words: &w }], &Params::default(), &mut |_| false), Err(SpeechError::Cancelled));
        // words whose times lie outside the audio never panic
        let w = vec![Word::new("x", seconds_tick(50.0), seconds_tick(52.0))];
        assert!(diarize(&Fake, &[Clip { audio: &a, words: &w }], &Params::default(), &mut |_| true).is_ok());
    }

    #[test]
    fn clustering_merges_until_the_threshold() {
        let v = |x: f32, y: f32| {
            let mut v = vec![x, y];
            normalize(&mut v);
            v
        };
        let vs = vec![v(1.0, 0.0), v(1.0, 0.1), v(0.0, 1.0), v(0.1, 1.0), v(1.0, 0.05)];
        assert_eq!(cluster(&vs, 0.5, None, 10), vec![0, 0, 1, 1, 0]);
        assert_eq!(cluster(&vs, 0.5, Some(1), 10), vec![0; 5]);
        assert_eq!(cluster(&vs, 0.999_999, None, 10).iter().max(), Some(&4));
        assert!(cluster(&[], 0.5, None, 10).is_empty());
    }

    #[test]
    fn voices_match_once_each() {
        let c = vec![vec![1.0, 0.0], vec![0.9, 0.1], vec![0.0, 1.0]];
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        let short = [1.0f32];
        assert_eq!(match_voices(&c, &[&a, &b, &short], 0.6), vec![Some(0), None, Some(1)]);
        assert_eq!(match_voices(&c, &[], 0.6), vec![None, None, None]);
    }
}
