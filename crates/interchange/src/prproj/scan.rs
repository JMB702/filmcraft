//! The `.prproj` container: a gzip member (RFC 1952) holding one XML document whose root element
//! (`PremiereData`) is a flat list of objects. Each object carries an `ObjectID` (a number) or an
//! `ObjectUID` (a UUID), and objects point at each other with `ObjectRef` / `ObjectURef`.
//!
//! A real project is hundreds of megabytes of XML, so it is never parsed as one tree:
//! [`objects`] walks the document once and hands each top-level object to the caller as a slice,
//! and only the objects the importer uses are parsed (each on its own, see `graph`).

use std::borrow::Cow;

/// Largest uncompressed document accepted. Real projects are a few hundred megabytes; the cap keeps
/// a forged gzip trailer from asking for an absurd allocation.
pub(super) const MAX_XML: usize = 1536 << 20;

/// Whether `bytes` look like a Premiere project: a gzip member, or the XML itself.
pub(crate) fn sniff(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0x1f, 0x8b, 8]) {
        return true;
    }
    let head = String::from_utf8_lossy(bytes.get(..bytes.len().min(512)).unwrap_or_default());
    head.trim_start_matches('\u{feff}').trim_start().starts_with('<') && head.contains("<PremiereData")
}

/// The document text: gunzipped when the file is compressed (as Premiere saves it), as-is when it is
/// already XML. Invalid UTF-8 is replaced rather than refused.
pub(super) fn document(bytes: &[u8]) -> Result<Cow<'_, str>, String> {
    let raw: Cow<'_, [u8]> = if bytes.starts_with(&[0x1f, 0x8b]) { Cow::Owned(gunzip(bytes)?) } else { Cow::Borrowed(bytes) };
    let raw = match raw {
        Cow::Borrowed(b) => b.strip_prefix(&[0xef, 0xbb, 0xbf]).map_or(Cow::Borrowed(b), Cow::Borrowed),
        Cow::Owned(mut v) => {
            if v.starts_with(&[0xef, 0xbb, 0xbf]) {
                v.drain(..3);
            }
            Cow::Owned(v)
        }
    };
    Ok(match raw {
        Cow::Borrowed(b) => String::from_utf8_lossy(b),
        Cow::Owned(v) => match String::from_utf8(v) {
            Ok(s) => Cow::Owned(s),
            Err(e) => Cow::Owned(String::from_utf8_lossy(e.as_bytes()).into_owned()),
        },
    })
}

/// Decompress a single gzip member (RFC 1952). The trailer's size field bounds the output; the
/// CRC is not checked (the XML parse catches corruption).
fn gunzip(b: &[u8]) -> Result<Vec<u8>, String> {
    let header = b.get(..10).ok_or("truncated gzip header")?;
    if header[0] != 0x1f || header[1] != 0x8b || header[2] != 8 {
        return Err("not a gzip (deflate) stream".into());
    }
    let flags = header[3];
    if flags & 0xe0 != 0 {
        return Err("reserved gzip header flags are set".into());
    }
    let mut pos = 10usize;
    if flags & 0x04 != 0 {
        let xlen = b.get(pos..pos + 2).ok_or("truncated gzip extra field")?;
        pos = pos.saturating_add(2).saturating_add(usize::from(u16::from_le_bytes([xlen[0], xlen[1]])));
    }
    for flag in [0x08u8, 0x10] {
        if flags & flag != 0 {
            let rest = b.get(pos..).ok_or("truncated gzip header")?;
            let nul = rest.iter().position(|&c| c == 0).ok_or("unterminated gzip header string")?;
            pos = pos.saturating_add(nul).saturating_add(1);
        }
    }
    if flags & 0x02 != 0 {
        pos = pos.saturating_add(2);
    }
    let body_end = b.len().checked_sub(8).ok_or("truncated gzip trailer")?;
    let body = b.get(pos..body_end).ok_or("truncated gzip stream")?;
    let trailer = b.get(body_end..).ok_or("truncated gzip trailer")?;
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) as usize;
    if size > MAX_XML {
        return Err(format!("the project is too large to open ({} MB uncompressed; the limit is {} MB)", size >> 20, MAX_XML >> 20));
    }
    let out = filmcraft_media::still::exr::inflate::inflate(body, size).map_err(|e| format!("gzip: {e}"))?;
    if out.len() != size {
        return Err("gzip: the stream ends before its stated size (the file is truncated)".into());
    }
    Ok(out)
}

/// One top-level object of the document.
pub(super) struct Object<'a> {
    /// Element name (`Sequence`, `VideoClipTrackItem`, …).
    pub name: &'a str,
    /// The element's full text, start tag to end tag.
    pub text: &'a str,
}

/// Why the walk over the document stopped early. Objects before `at` were delivered.
pub(super) struct Truncated {
    pub at: usize,
    pub why: &'static str,
}

/// Walk the children of the document's root element, calling `f` with each one. The walk needs no
/// memory beyond the text: it tracks only the element depth.
pub(super) fn objects<'a>(text: &'a str, mut f: impl FnMut(Object<'a>)) -> Result<usize, Truncated> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut open: Option<(usize, &'a str)> = None;
    let mut count = 0usize;
    let find = |from: usize, pat: &[u8]| -> Option<usize> { b.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|p| p + from) };
    loop {
        let Some(lt) = b.get(i..).and_then(|r| r.iter().position(|&c| c == b'<')) else {
            return if depth == 0 { Ok(count) } else { Err(Truncated { at: i, why: "the document ends inside an element" }) };
        };
        let at = i + lt;
        let rest = &b[at..];
        if rest.starts_with(b"<?") {
            i = find(at, b"?>").ok_or(Truncated { at, why: "unterminated processing instruction" })? + 2;
        } else if rest.starts_with(b"<!--") {
            i = find(at, b"-->").ok_or(Truncated { at, why: "unterminated comment" })? + 3;
        } else if rest.starts_with(b"<![CDATA[") {
            i = find(at, b"]]>").ok_or(Truncated { at, why: "unterminated CDATA section" })? + 3;
        } else if rest.starts_with(b"<!") {
            i = find(at, b">").ok_or(Truncated { at, why: "unterminated declaration" })? + 1;
        } else if rest.starts_with(b"</") {
            let gt = find(at, b">").ok_or(Truncated { at, why: "unterminated end tag" })?;
            depth = depth.checked_sub(1).ok_or(Truncated { at, why: "an end tag without a start tag" })?;
            i = gt + 1;
            if depth == 1
                && let Some((start, name)) = open.take()
                && let Some(t) = text.get(start..i)
            {
                f(Object { name, text: t });
                count += 1;
            }
        } else {
            let gt = tag_end(b, at).ok_or(Truncated { at, why: "unterminated start tag" })?;
            let self_closing = gt > at && b.get(gt - 1) == Some(&b'/');
            let name_end = b[at + 1..gt].iter().position(|c| c.is_ascii_whitespace() || *c == b'/' || *c == b'>').map_or(gt, |p| at + 1 + p);
            let name = text.get(at + 1..name_end).unwrap_or("");
            i = gt + 1;
            if depth == 1 {
                if self_closing {
                    if let Some(t) = text.get(at..i) {
                        f(Object { name, text: t });
                        count += 1;
                    }
                } else {
                    open = Some((at, name));
                }
            }
            if !self_closing {
                depth += 1;
            }
        }
    }
}

/// Index of the `>` closing the start tag at `at`, skipping quoted attribute values.
fn tag_end(b: &[u8], at: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (k, &c) in b.get(at..)?.iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => return Some(at + k),
            None => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> (Vec<String>, Result<usize, Truncated>) {
        let mut v = Vec::new();
        let r = objects(text, |o| v.push(format!("{}:{}", o.name, o.text.len())));
        (v, r)
    }

    #[test]
    fn walks_top_level_objects_only() {
        let doc = "<?xml version=\"1.0\"?>\n<PremiereData Version=\"3\">\n\t<A ObjectRef=\"1\"/>\n\t<B ObjectID=\"1\"><C x=\"a>b\"><D/></C><!-- </B> --><![CDATA[</B>]]></B>\n</PremiereData>\n";
        let (v, r) = names(doc);
        assert!(r.is_ok());
        assert_eq!(v.len(), 2);
        assert!(v[0].starts_with("A:"));
        assert!(v[1].starts_with("B:"));
    }

    #[test]
    fn truncation_is_reported_with_the_objects_before_it() {
        let doc = "<PremiereData><A ObjectID=\"1\"></A><B ObjectID=\"2\"><C>";
        let mut n = 0;
        let r = objects(doc, |_| n += 1);
        assert_eq!(n, 1);
        assert!(r.is_err());
        let r = objects("<PremiereData></X></Y></Z>", |_| ());
        assert!(r.is_err());
    }

    #[test]
    fn gzip_round_trip_and_hostile_trailers() {
        // a stored-block gzip member holding "<PremiereData/>"
        let payload = b"<PremiereData/>";
        let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 1];
        gz.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        gz.extend_from_slice(&(!(payload.len() as u16)).to_le_bytes());
        gz.extend_from_slice(payload);
        gz.extend_from_slice(&[0, 0, 0, 0]);
        gz.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        assert!(sniff(&gz));
        assert_eq!(document(&gz).unwrap(), "<PremiereData/>");
        // trailer claims more than the stream holds
        let mut short = gz.clone();
        let n = short.len();
        short[n - 4..].copy_from_slice(&100u32.to_le_bytes());
        assert!(document(&short).is_err());
        // trailer claims less
        short[n - 4..].copy_from_slice(&3u32.to_le_bytes());
        assert!(document(&short).is_err());
        // absurd size
        short[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(document(&short).is_err());
        for cut in 0..gz.len() {
            let _ = document(&gz[..cut]);
        }
    }
}
