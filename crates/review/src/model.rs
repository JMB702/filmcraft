//! What a share is, what reviewers send, and what the page is told.

use serde::{Deserialize, Serialize};

/// Version of `share.json`; a newer file is not served (it may mean something this build doesn't know).
pub const SHARE_FORMAT: u32 = 1;

/// One rendered file of a share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rendition {
    /// `low` (weak signal), `small` (phones) or `hd`. Also the file name: `media/<name>.mp4`.
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
}

/// Rendition names the server will serve (anything else in a path is a 404).
pub const RENDITIONS: [&str; 3] = ["low", "small", "hd"];

/// A review link (`shares/<id>/share.json`). Written by the app; the server only reads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRecord {
    pub format: u32,
    /// Random hex; the folder name. Not secret.
    pub id: String,
    /// The secret in the link (64 hex digits).
    pub token: String,
    /// The sequence's name, shown as the page title.
    pub title: String,
    /// The project file the sequence belongs to (comments are applied when it is open).
    pub project: String,
    /// The sequence's item id in that project.
    pub sequence: u64,
    pub rate_num: i64,
    pub rate_den: i64,
    pub drop_frame: bool,
    /// The sequence's Start Time, in frames.
    pub start_timecode: i64,
    /// Sequence time (ticks) of the rendered file's first frame.
    pub range_start: i64,
    /// Frames in the rendered file.
    pub duration_frames: i64,
    pub width: u32,
    pub height: u32,
    pub renditions: Vec<Rendition>,
    pub created_unix: i64,
    /// Unix seconds after which the link stops working (`None`: never).
    #[serde(default)]
    pub expires_unix: Option<i64>,
    #[serde(default)]
    pub revoked: bool,
}

impl ShareRecord {
    /// Not revoked and not past its expiry at `now` (Unix seconds).
    pub fn is_live(&self, now: i64) -> bool {
        !self.revoked && self.format <= SHARE_FORMAT && self.expires_unix.is_none_or(|e| now < e)
    }
}

/// What a comment event does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventKind {
    Add,
    Edit,
    Delete,
}

/// One line of `events.jsonl`: a reviewer added, edited or deleted a comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentEvent {
    /// Random hex, unique per event (the app's ledger records these).
    pub id: String,
    pub kind: EventKind,
    /// The comment this event is about (random hex; also in the marker's link).
    pub comment: String,
    /// Reviewer's name (Add; Edit/Delete repeat it).
    #[serde(default)]
    pub author: String,
    /// Full comment text (Add, Edit).
    #[serde(default)]
    pub text: String,
    /// Frame of the rendered file (Add).
    #[serde(default)]
    pub frame: i64,
    /// SHA-256 (hex) of the device secret that wrote the comment: only that device may change it.
    #[serde(default)]
    pub device: String,
    pub at_unix: i64,
}

/// A comment as it stands after its events, as the page sees it (no device hash).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub text: String,
    pub frame: i64,
    pub at_unix: i64,
    #[serde(default)]
    pub edited: bool,
}

/// Fold a comment log into the comments that stand, ordered by frame then time. Events for an
/// unknown comment, and edits or deletes from another device, are ignored.
pub fn fold(events: &[CommentEvent]) -> Vec<Comment> {
    let mut out: Vec<(Comment, String)> = Vec::new();
    for e in events {
        match e.kind {
            EventKind::Add => {
                if !out.iter().any(|(c, _)| c.id == e.comment) {
                    out.push((
                        Comment { id: e.comment.clone(), author: e.author.clone(), text: e.text.clone(), frame: e.frame, at_unix: e.at_unix, edited: false },
                        e.device.clone(),
                    ));
                }
            }
            EventKind::Edit => {
                if let Some((c, _)) = out.iter_mut().find(|(c, d)| c.id == e.comment && *d == e.device) {
                    c.text = e.text.clone();
                    c.edited = true;
                }
            }
            EventKind::Delete => out.retain(|(c, d)| !(c.id == e.comment && *d == e.device)),
        }
    }
    let mut v: Vec<Comment> = out.into_iter().map(|(c, _)| c).collect();
    v.sort_by(|a, b| a.frame.cmp(&b.frame).then(a.at_unix.cmp(&b.at_unix)));
    v
}

/// What the page needs to play and label the share (`GET /r/<token>/info`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareInfo {
    pub title: String,
    pub rate_num: i64,
    pub rate_den: i64,
    pub drop_frame: bool,
    /// Timecode frame number of the file's first frame (sequence Start Time + range start).
    pub tc_base: i64,
    pub duration_frames: i64,
    pub width: u32,
    pub height: u32,
    /// Renditions whose file is ready, smallest first.
    pub renditions: Vec<Rendition>,
    pub comments: Vec<Comment>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, comment: &str, device: &str, text: &str, frame: i64, at: i64) -> CommentEvent {
        CommentEvent {
            id: format!("{comment}{at}"),
            kind,
            comment: comment.into(),
            author: "Ana".into(),
            text: text.into(),
            frame,
            device: device.into(),
            at_unix: at,
        }
    }

    #[test]
    fn edits_and_deletes_follow_only_the_writing_device() {
        let log = vec![
            ev(EventKind::Add, "c1", "d1", "first", 40, 1),
            ev(EventKind::Add, "c2", "d2", "second", 10, 2),
            ev(EventKind::Edit, "c1", "d2", "hijack", 0, 3),
            ev(EventKind::Edit, "c1", "d1", "first, edited", 0, 4),
            ev(EventKind::Delete, "c2", "d1", "", 0, 5),
            ev(EventKind::Add, "c1", "d1", "duplicate add", 0, 6),
        ];
        let v = fold(&log);
        assert_eq!(v.iter().map(|c| (c.id.as_str(), c.text.as_str(), c.edited)).collect::<Vec<_>>(), [("c2", "second", false), ("c1", "first, edited", true)]);
        let v = fold(&[log[0].clone(), ev(EventKind::Delete, "c1", "d1", "", 0, 9)]);
        assert!(v.is_empty());
    }

    #[test]
    fn a_share_is_live_until_revoked_or_expired() {
        let mut s = ShareRecord {
            format: SHARE_FORMAT,
            id: "a".into(),
            token: "t".into(),
            title: String::new(),
            project: String::new(),
            sequence: 1,
            rate_num: 24,
            rate_den: 1,
            drop_frame: false,
            start_timecode: 0,
            range_start: 0,
            duration_frames: 10,
            width: 1,
            height: 1,
            renditions: vec![],
            created_unix: 0,
            expires_unix: Some(100),
            revoked: false,
        };
        assert!(s.is_live(99));
        assert!(!s.is_live(100));
        s.expires_unix = None;
        assert!(s.is_live(i64::MAX));
        s.revoked = true;
        assert!(!s.is_live(0));
        s.revoked = false;
        s.format = SHARE_FORMAT + 1;
        assert!(!s.is_live(0));
    }
}
