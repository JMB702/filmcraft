//! Automatic relinking: missing media comes back by itself, and only when the answer is certain.
//!
//! When a project opens (or a drive mounts, or a linked file vanishes while the project is open),
//! the frontend starts [`Session::start_auto_relink`]: a worker thread looks for every missing
//! file where it most likely is, checks each candidate against everything the project knows about
//! the original, and hands the result back to [`Session::poll_auto_relink`], which links the
//! certain ones as one undo step and asks about the rest.
//!
//! **Where it looks**, nearest first ([`run`]): the other paths the document stored (Premiere's
//! project-relative path); folder remaps learned from files found earlier (one moved file moves
//! its folder, as Link Media's "relink others", but automatically); the project's folder tree and
//! up to [`Limits::parent_levels`] folders above it; the folders the project's online media are
//! in; the same trailing path on every mounted drive and under the home folder; and the system's
//! search index (Spotlight, [`Services::find_indexed`]) by file name, and by size for files with
//! a recorded fingerprint (that finds a renamed file). The walk is capped by [`Limits`].
//!
//! **When it links** ([`judge`], [`decide`]):
//! - An item with a recorded content fingerprint (`MediaClip::identity`, recorded by import and
//!   by every link) links only to a file with the same size and fingerprint. Name and folder
//!   don't matter, so a renamed file is found. Copies with the same fingerprint are the same
//!   content; the nearest is taken.
//! - An item without one (imported from a `.prproj` while its file was missing) links only to a
//!   file with the same name and extension, the same duration (within a frame; stills excepted),
//!   the same frame size and rate, audio channels and sample rate, and start timecode / file size
//!   when the project records them. And only when no rival passes too: every candidate that
//!   passes is grouped by content (size and fingerprint), and more than one group means the user
//!   decides. A same-named cloud placeholder of a different size is a rival as well.
//! - Anything else (rivals, near misses, nothing found but similarly named files) goes to Link
//!   Media with the candidates filled in and the best one selected: never silently picked.
//! - A placeholder that is the only candidate is downloaded and checked like any file once it is
//!   here ([`crate::relink_cloud`]).
//!
//! Each link records the file's fingerprint, so the next relink of that item is exact.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use filmcraft_media::MediaKind;
use filmcraft_project::{ItemId, MediaClip, MediaIdentity, MediaRef};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use web_time::Instant;

use crate::relink::{Candidate, MatchOptions, Plan, apply, apply_remap, check_media, derive_remap, file_name};
use crate::{MediaPool, Result, Services, Session};

/// How a file was found (the relink report's "Found in" column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Via {
    /// Another path the document stored for the file (Premiere's project-relative path).
    ProjectRelative,
    /// The folder remap another found file implies.
    Remap,
    /// The project's folder, or a folder above it.
    ProjectFolder,
    /// A folder the project's other (online) media are in.
    MediaFolder,
    /// The same trailing path on another drive, or under the home folder.
    OtherDrive,
    /// The system's search index (Spotlight).
    Spotlight,
}

impl Via {
    pub fn label(self) -> &'static str {
        match self {
            Via::ProjectRelative => "project-relative path",
            Via::Remap => "same folder as other found media",
            Via::ProjectFolder => "near the project",
            Via::MediaFolder => "folder of other project media",
            Via::OtherDrive => "same path on another drive",
            Via::Spotlight => "Spotlight",
        }
    }
}

/// One missing item the search looks for.
#[derive(Clone, Debug)]
pub struct Want {
    pub item: ItemId,
    /// The stored path that is missing.
    pub path: String,
    /// Other paths the document stored for it.
    pub alternates: Vec<String>,
    /// The item's media as the project records it.
    pub media: MediaClip,
}

/// Caps on one search.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// Files listed while walking folders, in all.
    pub files: usize,
    /// Wall time for walking folders (candidates already found are still checked).
    pub time: Duration,
    /// Folders above the project's own folder that are walked too.
    pub parent_levels: usize,
    /// Candidate files checked per item.
    pub candidates_per_item: usize,
    /// Wait for the search index.
    pub index_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self { files: 200_000, time: Duration::from_secs(30), parent_levels: 2, candidates_per_item: 16, index_timeout: Duration::from_secs(10) }
    }
}

/// Everything a search needs, taken from the session so it can run on a worker thread.
#[derive(Clone, Debug, Default)]
pub struct Search {
    pub wants: Vec<Want>,
    /// The project (or the interchange document it was opened from).
    pub document: Option<String>,
    /// Folders the project's online media are in.
    pub media_dirs: Vec<String>,
    /// Folder remaps learned from earlier links (old prefix, new prefix).
    pub remaps: Vec<(String, String)>,
    /// Drive roots and the home folder: where a stored path's trailing part is looked up.
    pub roots: Vec<String>,
    /// Ask the system's search index.
    pub index: bool,
    pub limits: Limits,
}

/// A file the search links an item to.
#[derive(Clone)]
pub struct Link {
    pub item: ItemId,
    /// The missing path it replaces.
    pub from: String,
    pub cand: Candidate,
    pub via: Via,
}

/// What one search found.
#[derive(Clone, Default)]
pub struct Outcome {
    /// Certain: link these.
    pub linked: Vec<Link>,
    /// The user decides: candidates best first (passing ones first).
    pub ambiguous: Vec<(ItemId, Vec<Candidate>)>,
    /// The only candidate is a cloud placeholder: download it, then check it.
    pub cloud: Vec<(ItemId, String, Via)>,
    /// Nothing that looks like the file.
    pub not_found: Vec<ItemId>,
    /// Files listed while walking folders.
    pub files_seen: usize,
    /// The walk stopped at a cap.
    pub truncated: bool,
}

/// The result of checking one file against an item under the automatic rules.
pub enum Verdict {
    Pass(Candidate),
    Fail(Candidate),
    /// A cloud placeholder: it can only be checked once it is downloaded.
    Cloud,
}

/// Check `path` as item `m`'s file under the automatic rules (see the module docs).
pub fn judge(m: &MediaClip, pool: &MediaPool, services: &dyn Services, path: &str) -> Verdict {
    if services.is_cloud_placeholder(path) {
        return Verdict::Cloud;
    }
    let exact = m.identity.is_some();
    let o = MatchOptions {
        file_name: !exact,
        extension: !exact,
        clip_id: true,
        duration: true,
        media_start: false,
        metadata: true,
        align_timecode: false,
        relink_others: false,
    };
    let mut c = match check_media(m, pool, services, path, &o) {
        Ok(c) => c,
        Err(e) => Candidate { path: path.to_string(), problems: vec![e.to_string()], ..Default::default() },
    };
    if exact {
        if c.identity_match != Some(true) && c.problems.is_empty() {
            c.problems.push("not the same file: the content fingerprint differs".into());
        }
    } else {
        unverified_checks(m, &mut c);
    }
    c.ok = c.problems.is_empty();
    if c.ok {
        Verdict::Pass(c)
    } else {
        // listed, not played: don't hold its decoder
        c.source = None;
        Verdict::Fail(c)
    }
}

/// The extra checks for an item without a recorded fingerprint: every property the project
/// records must be there to compare, and must match.
fn unverified_checks(m: &MediaClip, c: &mut Candidate) {
    let want = &m.info;
    let still = matches!(want.kind, MediaKind::Still);
    if !still && want.duration <= Tick::ZERO {
        c.problems.push("the project records no duration for this clip, so it can't be confirmed automatically".into());
    }
    if want.video.is_none() && !want.has_audio() {
        c.problems.push("the project records no stream properties for this clip, so it can't be confirmed automatically".into());
    }
    if let (Some(size), Some(id)) = (want.file_size, c.identity)
        && size != id.size
    {
        c.problems.push(format!("file size differs ({} vs {size} bytes)", id.size));
    }
    let Some(info) = &c.info else { return };
    if let (Some(a), Some(b)) = (info.start_timecode, want.start_timecode)
        && a != b
    {
        c.problems.push(format!("media start differs ({a} vs {b})"));
    }
    if want.has_audio() && !info.has_audio() {
        c.problems.push("the file has no audio".into());
    }
    if let (Some(a), Some(b)) = (info.audio(), want.audio())
        && a.sample_rate != b.sample_rate
    {
        c.problems.push(format!("audio sample rate differs ({} vs {})", a.sample_rate, b.sample_rate));
    }
}

/// What the signals that confirmed a link were (the relink report).
pub fn signals(m: &MediaClip, c: &Candidate) -> Vec<&'static str> {
    if c.identity_match == Some(true) {
        return vec!["same content fingerprint and size"];
    }
    let mut v = vec!["file name", "extension"];
    if !matches!(m.info.kind, MediaKind::Still) {
        v.push("duration");
    }
    if m.info.video.is_some() {
        v.push("frame size and rate");
    }
    if m.info.has_audio() {
        v.push("audio channels and sample rate");
    }
    if m.info.start_timecode.is_some() {
        v.push("start timecode");
    }
    if m.info.file_size.is_some() {
        v.push("file size");
    }
    v
}

/// The candidates collected for one item.
#[derive(Default)]
struct Bag {
    pass: Vec<(Candidate, Via)>,
    fail: Vec<(Candidate, Via)>,
    /// Cloud placeholders: path, size, how found.
    cloud: Vec<(String, u64, Via)>,
    tried: BTreeSet<String>,
}

/// What [`decide`] makes of one item's candidates.
pub enum Decision {
    Link(Box<Candidate>, Via),
    Ask(Vec<Candidate>),
    Download(String, Via),
    NotFound,
}

fn content(c: &Candidate) -> Option<MediaIdentity> {
    c.identity
}

/// Decide one item from its candidates (see the module docs).
fn decide(m: &MediaClip, bag: Bag) -> Decision {
    let mut groups: Vec<Option<MediaIdentity>> = Vec::new();
    for (c, _) in &bag.pass {
        if !groups.contains(&content(c)) {
            groups.push(content(c));
        }
    }
    // A placeholder can't be fingerprinted before it is downloaded; its size can. One of another
    // size than every passing file is a rival with other content.
    let rival_cloud = m.identity.is_none() && bag.cloud.iter().any(|(_, size, _)| !groups.iter().any(|g| g.is_some_and(|g| g.size == *size)));
    let ask = |bag: Bag| {
        let mut list: Vec<Candidate> = bag.pass.into_iter().map(|(c, _)| c).collect();
        for (path, _, _) in bag.cloud {
            list.push(Candidate { path, problems: vec![filmcraft_media::reader::NOT_DOWNLOADED.to_string()], ..Default::default() });
        }
        let mut fails: Vec<Candidate> = bag.fail.into_iter().map(|(c, _)| c).collect();
        fails.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)));
        list.extend(fails);
        Decision::Ask(list)
    };
    match groups.len() {
        1 if !rival_cloud => match bag.pass.into_iter().next() {
            Some((c, via)) => Decision::Link(Box::new(c), via),
            None => Decision::NotFound,
        },
        0 if bag.cloud.len() == 1 => match bag.cloud.into_iter().next() {
            Some((path, _, via)) => Decision::Download(path, via),
            None => Decision::NotFound,
        },
        0 if bag.cloud.is_empty() && bag.fail.is_empty() => Decision::NotFound,
        _ => ask(bag),
    }
}

/// A search in progress.
struct Run<'a> {
    search: &'a Search,
    pool: &'a MediaPool,
    services: &'a dyn Services,
    bags: Vec<Bag>,
    files_seen: usize,
    truncated: bool,
    deadline: Instant,
}

/// Lower-case file name of a path in either separator style.
fn lower_name(path: &str) -> String {
    file_name(path).to_lowercase()
}

impl<'a> Run<'a> {
    /// Whether item `i` is settled: it has a recorded fingerprint and a file that matches it.
    fn exact(&self, i: usize) -> bool {
        self.search.wants.get(i).is_some_and(|w| w.media.identity.is_some()) && self.bags.get(i).is_some_and(|b| !b.pass.is_empty())
    }

    fn open(&self) -> Vec<usize> {
        (0..self.search.wants.len()).filter(|&i| !self.exact(i)).collect()
    }

    /// Check `path` for item `i`. `by_name`: found by its name (not only its size), so a failed
    /// check is still worth showing the user.
    fn try_path(&mut self, i: usize, path: &str, via: Via, by_name: bool) {
        let Some(w) = self.search.wants.get(i) else { return };
        let Some(bag) = self.bags.get_mut(i) else { return };
        if path == w.path || !bag.tried.insert(path.to_string()) {
            return;
        }
        if bag.pass.len() + bag.fail.len() + bag.cloud.len() >= self.search.limits.candidates_per_item {
            return;
        }
        let Ok(size) = self.services.file_size(path) else { return };
        match judge(&w.media, self.pool, self.services, path) {
            Verdict::Pass(c) => bag.pass.push((c, via)),
            Verdict::Fail(c) if by_name => bag.fail.push((c, via)),
            Verdict::Cloud if by_name => bag.cloud.push((path.to_string(), size, via)),
            Verdict::Fail(_) | Verdict::Cloud => {}
        }
    }

    /// The document's other paths, and remaps learned earlier.
    fn direct(&mut self) {
        for i in 0..self.search.wants.len() {
            let Some(w) = self.search.wants.get(i) else { continue };
            let mut tries: Vec<(String, Via)> = w.alternates.iter().map(|a| (a.clone(), Via::ProjectRelative)).collect();
            for (from, to) in &self.search.remaps {
                if let Some(p) = apply_remap(&w.path, from, to) {
                    tries.push((p, Via::Remap));
                }
            }
            for (p, via) in tries {
                self.try_path(i, &p, via, true);
            }
        }
    }

    /// Apply the folder remap every found file implies to the items still open.
    fn learned(&mut self) {
        let mut remaps: Vec<(String, String)> = Vec::new();
        for (w, bag) in self.search.wants.iter().zip(&self.bags) {
            for (c, _) in &bag.pass {
                if let Some(r) = derive_remap(&w.path, &c.path)
                    && !remaps.contains(&r)
                {
                    remaps.push(r);
                }
            }
        }
        for i in self.open() {
            let Some(w) = self.search.wants.get(i) else { continue };
            let tries: Vec<String> = remaps.iter().filter_map(|(f, t)| apply_remap(&w.path, f, t)).collect();
            for p in tries {
                self.try_path(i, &p, Via::Remap, true);
            }
        }
    }

    /// The folders to walk, nearest first, each with how a file found in it is reported.
    fn walk_roots(&self) -> Vec<(PathBuf, Via)> {
        let mut roots: Vec<(PathBuf, Via)> = Vec::new();
        let stop: Vec<PathBuf> = self.search.roots.iter().map(PathBuf::from).collect();
        if let Some(dir) = self.search.document.as_deref().and_then(|d| Path::new(d).parent()) {
            let mut dir = dir.to_path_buf();
            roots.push((dir.clone(), Via::ProjectFolder));
            for _ in 0..self.search.limits.parent_levels {
                let Some(up) = dir.parent().map(Path::to_path_buf) else { break };
                // never a whole drive or the whole home folder, nor anything above them
                if up.parent().is_none() || stop.iter().any(|s| s == &up || s.starts_with(&up)) {
                    break;
                }
                dir = up;
                roots.push((dir.clone(), Via::ProjectFolder));
            }
        }
        // the project's own folder first, then its other media's, then the folders above it
        let (own, above) = roots.split_at(roots.len().min(1));
        let mut out: Vec<(PathBuf, Via)> = own.to_vec();
        for d in &self.search.media_dirs {
            out.push((PathBuf::from(d), Via::MediaFolder));
        }
        out.extend(above.iter().cloned());
        out
    }

    /// Walk the project's folder tree and its media folders; check files named like (or, for a
    /// fingerprinted item, sized like) an open item.
    fn walk(&mut self) {
        let mut names: HashMap<String, Vec<usize>> = HashMap::new();
        let mut sizes: HashMap<u64, Vec<usize>> = HashMap::new();
        for i in self.open() {
            let Some(w) = self.search.wants.get(i) else { continue };
            names.entry(lower_name(&w.path)).or_default().push(i);
            if let Some(id) = w.media.identity {
                sizes.entry(id.size).or_default().push(i);
            }
        }
        if names.is_empty() {
            return;
        }
        let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
        for (root, via) in self.walk_roots() {
            let hits = self.walk_one(root, &mut visited, &names, &sizes);
            for (i, path, by_name) in hits {
                self.try_path(i, &path, via, by_name);
            }
            // every item found by its fingerprint: the folders further out add nothing
            if self.open().is_empty() || self.truncated {
                break;
            }
        }
    }

    /// Walk one folder tree (skipping folders already walked); the files named like an item, or
    /// sized like a fingerprinted one: (item, path, found by name).
    fn walk_one(
        &mut self,
        root: PathBuf,
        visited: &mut BTreeSet<PathBuf>,
        names: &HashMap<String, Vec<usize>>,
        sizes: &HashMap<u64, Vec<usize>>,
    ) -> Vec<(usize, String, bool)> {
        let mut hits = Vec::new();
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(d) = queue.pop_front() {
            if Instant::now() >= self.deadline || self.files_seen >= self.search.limits.files {
                self.truncated = true;
                break;
            }
            if !visited.insert(d.clone()) {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name.ends_with(".app") || name == "node_modules" {
                    continue;
                }
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    queue.push_back(e.path());
                    continue;
                }
                self.files_seen += 1;
                let path = e.path().to_string_lossy().into_owned();
                for &i in names.get(&name.to_lowercase()).into_iter().flatten() {
                    hits.push((i, path.clone(), true));
                }
                if !sizes.is_empty()
                    && let Ok(meta) = e.metadata()
                {
                    for &i in sizes.get(&meta.len()).into_iter().flatten() {
                        hits.push((i, path.clone(), false));
                    }
                }
            }
        }
        hits
    }

    /// The stored path's trailing part under every drive root and the home folder.
    fn trailing(&mut self) {
        for i in self.open() {
            let Some(w) = self.search.wants.get(i) else { continue };
            let norm = w.path.replace('\\', "/");
            let comps: Vec<&str> = norm.split('/').filter(|c| !c.is_empty()).collect();
            // `/Volumes/<drive>/rest` keeps `rest`; any other path drops its first component
            // (`/Users`, `C:`), then its owner's folder name too when shorter suffixes are tried
            let skip = if comps.first() == Some(&"Volumes") { 2 } else { 1 };
            let rest = comps.get(skip..).unwrap_or_default();
            let mut tries = Vec::new();
            for k in (1..=rest.len().min(8)).rev() {
                let Some(suffix) = rest.get(rest.len() - k..) else { continue };
                let suffix = suffix.join("/");
                for root in &self.search.roots {
                    tries.push(format!("{}/{suffix}", root.trim_end_matches('/')));
                }
            }
            for p in tries {
                self.try_path(i, &p, Via::OtherDrive, true);
            }
        }
    }

    /// Ask the search index for the open items' names (and a fingerprinted item's size).
    fn index(&mut self) {
        if !self.search.index {
            return;
        }
        let mut names: HashMap<String, Vec<usize>> = HashMap::new();
        let mut sizes: HashMap<u64, Vec<usize>> = HashMap::new();
        for i in self.open() {
            let Some(w) = self.search.wants.get(i) else { continue };
            names.entry(lower_name(&w.path)).or_default().push(i);
            if let Some(id) = w.media.identity {
                sizes.entry(id.size).or_default().push(i);
            }
        }
        if names.is_empty() {
            return;
        }
        let name_list: Vec<String> = names.keys().map(|n| file_name(n).to_string()).collect();
        let size_list: Vec<u64> = sizes.keys().copied().collect();
        let Some(Ok(found)) = self.services.find_indexed(&name_list, &size_list, self.search.limits.index_timeout) else { return };
        for path in found.into_iter().take(4096) {
            let by_name = names.get(&lower_name(&path)).cloned().unwrap_or_default();
            for i in &by_name {
                self.try_path(*i, &path, Via::Spotlight, true);
            }
            if !sizes.is_empty()
                && let Ok(size) = self.services.file_size(&path)
            {
                for &i in sizes.get(&size).into_iter().flatten() {
                    if !by_name.contains(&i) {
                        self.try_path(i, &path, Via::Spotlight, false);
                    }
                }
            }
        }
    }
}

/// Look for every wanted item (see the module docs). Pure: nothing in a session changes.
pub fn run(search: &Search, pool: &MediaPool, services: &dyn Services) -> Outcome {
    let mut r = Run {
        search,
        pool,
        services,
        bags: search.wants.iter().map(|_| Bag::default()).collect(),
        files_seen: 0,
        truncated: false,
        deadline: Instant::now() + search.limits.time,
    };
    r.direct();
    r.learned();
    r.walk();
    r.learned();
    r.trailing();
    r.index();
    r.learned();
    let Run { bags, files_seen, truncated, .. } = r;
    let mut out = Outcome { files_seen, truncated, ..Default::default() };
    for (w, bag) in search.wants.iter().zip(bags) {
        match decide(&w.media, bag) {
            Decision::Link(cand, via) => out.linked.push(Link { item: w.item, from: w.path.clone(), cand: *cand, via }),
            Decision::Ask(list) => out.ambiguous.push((w.item, list)),
            Decision::Download(path, via) => out.cloud.push((w.item, path, via)),
            Decision::NotFound => out.not_found.push(w.item),
        }
    }
    out
}

/// The search for the session's missing items (`only`: just these items).
pub fn search_for(s: &Session, only: Option<&[ItemId]>) -> Search {
    let missing = crate::relink::scan(&s.project, &s.media, &*s.services);
    let wants: Vec<Want> = missing
        .iter()
        .filter(|m| m.status == "missing")
        .filter(|m| only.is_none_or(|o| o.contains(&ItemId(m.item))))
        .filter_map(|m| {
            let media = s.project.item(ItemId(m.item))?.as_media()?.clone();
            Some(Want { item: ItemId(m.item), alternates: s.offline.alternates.get(&m.path).cloned().unwrap_or_default(), path: m.path.clone(), media })
        })
        .collect();
    let mut media_dirs: Vec<String> = Vec::new();
    for (_, file) in crate::media_watch::media_files(&s.project, &s.media) {
        if s.services.file_size(&file).is_err() {
            continue;
        }
        if let Some(d) = Path::new(&file).parent().map(|d| d.to_string_lossy().into_owned())
            && !media_dirs.contains(&d)
        {
            media_dirs.push(d);
        }
    }
    let mut roots: Vec<String> = s.services.volumes().into_iter().filter(|v| v.kind == crate::media_browser::VolumeKind::Local).map(|v| v.path).collect();
    roots.extend(s.services.home_dir());
    Search {
        wants,
        document: s.offline.document.clone().or_else(|| s.path.clone()),
        media_dirs,
        remaps: s.offline.remaps.clone(),
        roots,
        index: true,
        limits: Limits::default(),
    }
}

/// A report row: one automatic link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRow {
    pub item: u64,
    pub name: String,
    pub from: String,
    pub to: String,
    pub via: Via,
    pub signals: Vec<String>,
}

/// What the last automatic relink did (the relink report).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub linked: Vec<ReportRow>,
    /// Items the user decides (Link Media lists their candidates).
    pub need_you: Vec<u64>,
    /// Items whose file is downloading from iCloud.
    pub downloading: Vec<u64>,
    pub not_found: Vec<u64>,
    pub files_seen: usize,
    pub truncated: bool,
    /// The automatic links were undone.
    pub undone: bool,
}

impl Report {
    /// "Relinked 41 automatically, 2 need you, 3 downloading from iCloud".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.linked.is_empty() {
            parts.push(format!("Relinked {} automatically", self.linked.len()));
        }
        let ask = self.need_you.len() + self.not_found.len();
        if ask > 0 {
            parts.push(format!("{ask} need you"));
        }
        if !self.downloading.is_empty() {
            parts.push(format!("{} downloading from iCloud", self.downloading.len()));
        }
        if parts.is_empty() {
            return "No missing media found".into();
        }
        let mut s = parts.join(", ");
        if let Some(first) = s.get(..1) {
            s = first.to_uppercase() + s.get(1..).unwrap_or_default();
        }
        s
    }
}

/// A candidate Link Media shows for an item the search couldn't decide.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub path: String,
    pub ok: bool,
    pub identity_match: Option<bool>,
    pub problems: Vec<String>,
}

/// State of the automatic search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutoState {
    #[default]
    Idle,
    /// The frontend should start a search ([`Session::start_auto_relink`]).
    Requested,
    Running,
}

/// The worker's result slot.
pub type Slot = Arc<Mutex<Option<Outcome>>>;

/// Runtime state of automatic relinking (worker handles; not saved, not serialised).
#[derive(Default)]
pub struct Runtime {
    /// The running search and the document epoch it was started for.
    pub search: Option<(u64, Slot)>,
    /// The media of the last automatic links before them (Undo Automatic Relink).
    pub before: Vec<(ItemId, String, MediaClip)>,
    /// Drive roots seen last ([`Session::check_new_volumes`]).
    pub volumes: Vec<String>,
    /// The running search was started by a file vanishing: don't open Link Media for it.
    pub quiet: bool,
    /// Linked files already seen vanishing (each starts one search, not one per look).
    pub vanished: BTreeSet<String>,
    pub downloads: crate::relink_cloud::Downloads,
}

/// Run `f` on a worker thread, catching a panic (AGENTS.md §0.6). Returns the slot it fills.
fn spawn_search(search: Search, pool: Arc<MediaPool>, services: Arc<dyn Services>) -> Option<Slot> {
    let slot: Slot = Arc::default();
    let out = slot.clone();
    let spawned = std::thread::Builder::new().name("auto-relink".into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&search, &pool, &*services)));
        let outcome = r.unwrap_or_else(|_| {
            log::error!("the automatic relink search failed");
            Outcome { not_found: search.wants.iter().map(|w| w.item).collect(), ..Default::default() }
        });
        *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
    });
    spawned.ok().map(|_| slot)
}

impl Session {
    /// Start the automatic search the session asked for ([`AutoState::Requested`]) on a worker
    /// thread, and the downloads of media that are cloud placeholders. Does nothing on the web
    /// (no file system to search) or when Settings ▸ Media ▸ Find missing media automatically is
    /// off (Link Media then opens as before).
    pub fn start_auto_relink(&mut self) {
        if self.offline.auto != AutoState::Requested || self.relink_auto.search.is_some() {
            return;
        }
        if cfg!(target_arch = "wasm32") || !self.prefs.media.auto_relink {
            self.offline.auto = AutoState::Idle;
            return;
        }
        crate::relink_cloud::download_placeholders(self, false);
        let search = search_for(self, None);
        if search.wants.is_empty() {
            self.offline.auto = AutoState::Idle;
            return;
        }
        match spawn_search(search, self.media.clone(), self.services.clone()) {
            Some(slot) => {
                self.relink_auto.search = Some((self.document_epoch, slot));
                self.offline.auto = AutoState::Running;
            }
            None => {
                // no thread: Link Media asks, as before
                self.offline.auto = AutoState::Idle;
                self.offline.prompt = !self.offline.missing.is_empty();
            }
        }
    }

    /// Apply a finished automatic search (`None` while there is none or it is still running).
    pub fn poll_auto_relink(&mut self) -> Option<Result<Value>> {
        let (epoch, slot) = self.relink_auto.search.as_ref()?;
        let epoch = *epoch;
        let outcome = slot.lock().unwrap_or_else(PoisonError::into_inner).take()?;
        self.relink_auto.search = None;
        self.offline.auto = AutoState::Idle;
        if epoch != self.document_epoch {
            // another project was opened meanwhile
            return None;
        }
        let quiet = std::mem::take(&mut self.relink_auto.quiet);
        Some(apply_outcome(self, outcome, quiet))
    }

    /// Request a search when a drive was mounted since the last look and media are missing.
    /// Cheap (lists the drive roots); the frontend calls it every few seconds.
    pub fn check_new_volumes(&mut self) -> bool {
        let now: Vec<String> = self.services.volumes().into_iter().map(|v| v.path).collect();
        let before = std::mem::replace(&mut self.relink_auto.volumes, now.clone());
        let mounted = !before.is_empty() && now.iter().any(|v| !before.contains(v));
        let missing = self.offline.missing.iter().any(|i| self.project.item(*i).and_then(|it| it.as_media()).is_some_and(|m| !m.offline));
        if mounted && missing && self.offline.auto == AutoState::Idle {
            self.offline.auto = AutoState::Requested;
            return true;
        }
        false
    }

    /// Linked files that disappeared while the project was open (moved or renamed): search for
    /// them by fingerprint without asking ([`crate::media_watch`] calls this).
    pub(crate) fn note_vanished(&mut self, paths: &[String]) {
        let new: Vec<&String> = paths.iter().filter(|p| !self.relink_auto.vanished.contains(*p)).collect();
        if new.is_empty() || self.offline.auto != AutoState::Idle || !self.prefs.media.auto_relink {
            return;
        }
        let new: Vec<String> = new.into_iter().cloned().collect();
        self.relink_auto.vanished.extend(new);
        crate::relink::refresh(self);
        self.relink_auto.quiet = true;
        self.offline.auto = AutoState::Requested;
    }
}

/// Link what a search is certain of (one undo step), record the rest for Link Media, start the
/// downloads, and report.
pub fn apply_outcome(s: &mut Session, o: Outcome, quiet: bool) -> Result<Value> {
    // the project may have changed while the search ran: link only items still missing at the
    // same path, and never one made offline on purpose
    let current = |s: &Session, id: ItemId, from: &str| {
        s.project.item(id).and_then(|i| i.as_media()).is_some_and(|m| !m.offline && matches!(&m.media, MediaRef::File { path } if path == from))
    };
    let mut report = Report { files_seen: o.files_seen, truncated: o.truncated, ..Default::default() };
    let mut plans = Vec::new();
    let mut before = Vec::new();
    for l in o.linked {
        if !current(s, l.item, &l.from) {
            continue;
        }
        let Some(it) = s.project.item(l.item) else { continue };
        let Some(m) = it.as_media() else { continue };
        report.linked.push(ReportRow {
            item: l.item.0,
            name: it.name.clone(),
            from: l.from.clone(),
            to: l.cand.path.clone(),
            via: l.via,
            signals: signals(m, &l.cand).into_iter().map(str::to_string).collect(),
        });
        before.push((l.item, l.cand.path.clone(), m.clone()));
        if let Some(r) = derive_remap(&l.from, &l.cand.path)
            && !s.offline.remaps.contains(&r)
        {
            s.offline.remaps.push(r);
        }
        plans.push(Plan { item: l.item, cand: l.cand });
    }
    if !plans.is_empty() {
        apply(s, plans, false, "Automatic Relink")?;
        s.relink_auto.before = before;
    }
    for (id, list) in o.ambiguous {
        report.need_you.push(id.0);
        let sugg = list.into_iter().map(|c| Suggestion { path: c.path, ok: c.ok, identity_match: c.identity_match, problems: c.problems }).collect();
        s.offline.suggestions.insert(id.0, sugg);
    }
    for (id, path, _) in o.cloud {
        if crate::relink_cloud::start(s, id, &path, true, false).is_ok() {
            report.downloading.push(id.0);
        } else {
            report.need_you.push(id.0);
        }
    }
    report.not_found = o.not_found.iter().map(|i| i.0).collect();
    report.downloading.extend(s.offline.downloads.iter().filter(|d| !d.candidate && d.state == crate::relink_cloud::DlState::Downloading).map(|d| d.item));
    report.downloading.sort_unstable();
    report.downloading.dedup();
    crate::relink::refresh(s);
    let waiting: Vec<u64> = s.offline.downloads.iter().filter(|d| d.state == crate::relink_cloud::DlState::Downloading).map(|d| d.item).collect();
    let need = s
        .offline
        .missing
        .iter()
        .filter(|i| !waiting.contains(&i.0))
        .filter(|i| s.project.item(**i).and_then(|it| it.as_media()).is_some_and(|m| !m.offline))
        .count();
    s.offline.prompt = need > 0 && !quiet;
    let summary = report.summary();
    let value = json!({
        "summary": summary,
        "relinked": report.linked,
        "needYou": report.need_you,
        "downloading": report.downloading,
        "notFound": report.not_found,
        "filesSeen": report.files_seen,
        "truncated": report.truncated,
        "quiet": quiet,
    });
    if !(quiet && report.linked.is_empty()) {
        s.toast(summary);
    }
    s.offline.report = Some(report);
    Ok(value)
}

/// `media.findAndLink {index?, parentLevels?, timeLimit?}`: the automatic search, run now (CLI,
/// agents, tests). The desktop app runs it in the background when a project opens.
pub fn find_and_link(s: &mut Session, p: &Value) -> Result<Value> {
    crate::relink::refresh(s);
    let mut search = search_for(s, None);
    if let Some(b) = p.get("index").and_then(Value::as_bool) {
        search.index = b;
    }
    if let Some(n) = p.get("parentLevels").and_then(Value::as_u64) {
        search.limits.parent_levels = usize::try_from(n.min(8)).unwrap_or(2);
    }
    if let Some(t) = p.get("timeLimit").and_then(Value::as_f64).filter(|t| t.is_finite() && *t > 0.0) {
        search.limits.time = Duration::from_secs_f64(t.min(600.0));
    }
    let o = run(&search, &s.media, &*s.services);
    apply_outcome(s, o, false)
}

/// `media.undoAutoRelink`: put the last automatic links back as they were (offline), as one undo
/// step. Items relinked again since are left alone.
pub fn undo_auto(s: &mut Session, _: &Value) -> Result<Value> {
    let before = std::mem::take(&mut s.relink_auto.before);
    let back: Vec<(ItemId, MediaClip)> = before
        .into_iter()
        .filter(|(id, to, _)| s.project.item(*id).and_then(|i| i.as_media()).is_some_and(|m| matches!(&m.media, MediaRef::File { path } if path == to)))
        .map(|(id, _, m)| (id, m))
        .collect();
    if back.is_empty() {
        return Err(crate::commands::bad("media.undoAutoRelink", "no automatic relink to undo"));
    }
    s.edit("Undo Automatic Relink", |proj, _| {
        for (id, old) in &back {
            if let Some(m) = proj.item_mut(*id).and_then(|i| i.as_media_mut()) {
                m.media = old.media.clone();
                m.info = old.info.clone();
                m.identity = old.identity;
            }
        }
        Ok(())
    })?;
    for (id, _) in &back {
        s.media.remove(*id);
    }
    if let Some(r) = s.offline.report.as_mut() {
        r.undone = true;
    }
    crate::relink::refresh(s);
    Ok(json!({"restored": back.iter().map(|(i, _)| i.0).collect::<Vec<_>>()}))
}

/// The Spotlight query for files named one of `names` (case-insensitive) or `sizes` bytes long.
/// Names come from project files, so they are hostile: the query language's quote, backslash and
/// wildcard characters are escaped, empty names and names longer than a file name can be are
/// dropped, and at most 512 of each are asked for. `None`: nothing to ask.
pub fn spotlight_query(names: &[String], sizes: &[u64]) -> Option<String> {
    let escape = |n: &str| n.replace('\\', "\\\\").replace('"', "\\\"").replace('*', "\\*");
    let mut terms: Vec<String> =
        names.iter().filter(|n| !n.is_empty() && n.len() <= 255).take(512).map(|n| format!("kMDItemFSName == \"{}\"c", escape(n))).collect();
    terms.extend(sizes.iter().take(512).map(|s| format!("kMDItemFSSize == {s}")));
    (!terms.is_empty()).then(|| terms.join(" || "))
}

/// Query Spotlight (`mdfind`) for files named one of `names` (case-insensitive) or of one of
/// `sizes` bytes, waiting at most `timeout`. The query is passed as one argument (no shell), with
/// the query language's quote, backslash and wildcard characters escaped.
#[cfg(target_os = "macos")]
pub fn spotlight(names: &[String], sizes: &[u64], timeout: Duration) -> std::io::Result<Vec<String>> {
    use std::io::Read;
    let Some(query) = spotlight_query(names, sizes) else { return Ok(Vec::new()) };
    let mut child = std::process::Command::new("/usr/bin/mdfind")
        .arg("-0")
        .arg(query)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("mdfind has no output"))?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(16 << 20).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let buf = reader.join().unwrap_or_default();
    Ok(buf.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect())
}
