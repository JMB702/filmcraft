//! A clip's Premiere effects → FilmCraft effect parameters: Motion, Opacity, Transform, the
//! Motion crop, and Volume. Other effects are named in the report.
//!
//! A parameter is a `time,value,…` start record plus, when animated, `time,value,interp,…;`
//! keyframe records. Keyframe times are in the source media's ticks (they stay on their frames
//! when the clip is trimmed), so a clip-time keyframe is `(time − source In) / speed`. Points are
//! `x:y` fractions of the picture: of the sequence frame for Motion's Position, of the clip's own
//! picture for anchors and for Transform.

use filmcraft_geom::Vec2;
use filmcraft_project::{Interpolation, Keyframe, Param, ParamValue, TrackItem, find_effect};
use filmcraft_time::Tick;

use super::graph::{self, Graph, Rec, Ref};
use crate::Report;
use crate::common::set_param;

/// Keyframes read per parameter.
const MAX_KEYFRAMES: usize = 10_000;

/// Where a clip's effect values land.
pub(super) struct Space {
    /// Sequence frame size.
    pub frame: (f64, f64),
    /// The clip's own picture size.
    pub source: (f64, f64),
    /// Source In (ticks) and speed: keyframe times are source times.
    pub in_point: i64,
    pub speed: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Value {
    Float(f64),
    Point(f64, f64),
    Bool(bool),
}

fn value(field: &str) -> Option<Value> {
    let f = field.trim();
    if f.eq_ignore_ascii_case("true") || f.eq_ignore_ascii_case("false") {
        return Some(Value::Bool(f.eq_ignore_ascii_case("true")));
    }
    if let Some((x, y)) = f.split_once(':') {
        let (x, y) = (x.trim().parse::<f64>().ok()?, y.trim().parse::<f64>().ok()?);
        return (x.is_finite() && y.is_finite()).then_some(Value::Point(x, y));
    }
    f.parse::<f64>().ok().filter(|v| v.is_finite()).map(Value::Float)
}

/// Premiere's keyframe interpolation code (the field after the value). 0 is linear; 5, the
/// default for keyframes made in the Effect Controls panel, is Bezier with its handles.
fn interpolation(code: Option<&str>) -> Interpolation {
    match code.map(str::trim) {
        Some("0") | None => Interpolation::Linear,
        Some("4") => Interpolation::Hold,
        _ => Interpolation::Bezier,
    }
}

/// The start value and the keyframes (clip time) of a parameter.
fn records(p: &graph::Param, space: &Space) -> (Option<Value>, Vec<(Tick, Value, Interpolation)>) {
    let start = p.start.as_deref().and_then(|s| s.split(',').nth(1)).and_then(value);
    let mut keys = Vec::new();
    for rec in p.keyframes.as_deref().unwrap_or("").split(';').filter(|r| !r.trim().is_empty()).take(MAX_KEYFRAMES) {
        let mut fields = rec.split(',');
        let Some(time) = fields.next().and_then(|t| t.trim().parse::<i64>().ok()) else { continue };
        let Some(v) = fields.next().and_then(value) else { continue };
        let interp = interpolation(fields.next());
        let clip = time.saturating_sub(space.in_point) as f64 / space.speed;
        if !clip.is_finite() || clip.abs() > 9.0e18 {
            continue;
        }
        keys.push((Tick(clip.round() as i64), v, interp));
    }
    keys.sort_by_key(|(t, _, _)| *t);
    (start, keys)
}

/// A FilmCraft parameter from a Premiere one; `map` converts each value (None skips it).
fn param(p: &graph::Param, space: &Space, map: impl Fn(Value) -> Option<ParamValue>) -> Option<Param> {
    let (start, keys) = records(p, space);
    let first = start.or_else(|| keys.first().map(|(_, v, _)| *v))?;
    let mut out = Param::new(map(first)?);
    for (time, v, interp) in keys {
        let Some(value) = map(v) else { continue };
        let mut k = Keyframe::new(time, value);
        k.interp = interp;
        out.keyframes.push(k);
    }
    if let Some(k) = out.keyframes.first() {
        out.value = k.value.clone();
    }
    Some(out)
}

fn float(v: Value) -> Option<ParamValue> {
    match v {
        Value::Float(f) => Some(ParamValue::Float(f)),
        _ => None,
    }
}

fn boolean(v: Value) -> Option<ParamValue> {
    match v {
        Value::Bool(b) => Some(ParamValue::Bool(b)),
        _ => None,
    }
}

/// A point parameter in pixels of `size`, or None when it is the default centre and still.
fn point(p: &graph::Param, space: &Space, size: (f64, f64)) -> Option<Param> {
    let pr = param(p, space, |v| match v {
        Value::Point(x, y) => Some(ParamValue::Vec2(Vec2::new(x * size.0, y * size.1))),
        _ => None,
    })?;
    let centre = pr.value.as_vec2().is_some_and(|v| (v.x - size.0 / 2.0).abs() < 1e-6 && (v.y - size.1 / 2.0).abs() < 1e-6);
    (pr.is_animated() || !centre).then_some(pr)
}

/// Premiere's Volume level is linear with 0 dB at 10^(−15/20): the stored value is the
/// amplitude scaled down by 15 dB.
fn volume_db(v: Value) -> Option<ParamValue> {
    match v {
        Value::Float(f) if f > 0.0 => Some(ParamValue::Float((20.0 * f.log10() + 15.0).clamp(-287.5, 15.0))),
        Value::Float(_) => Some(ParamValue::Float(-287.5)),
        _ => None,
    }
}

/// Apply the effects of `chain` to the clip `ti`.
pub(super) fn apply(g: &Graph, chain: Option<&Ref>, ti: &mut TrackItem, space: &Space, report: &mut Report) {
    let Some(Rec::Chain(filters)) = chain.and_then(|c| g.get(c)) else { return };
    for f in filters {
        let Some(Rec::Filter(filter)) = g.get(f) else { continue };
        let params: Vec<&graph::Param> = filter
            .params
            .iter()
            .filter_map(|p| match g.get(p) {
                Some(Rec::Param(p)) => Some(p),
                _ => None,
            })
            .collect();
        let by_id = |id: i64| params.iter().copied().find(|p| p.id == Some(id));
        let by_name = |name: &str| params.iter().copied().find(|p| p.name.eq_ignore_ascii_case(name));
        let set = |ti: &mut TrackItem, effect: &str, pid: &str, value: Option<Param>| {
            if let Some(v) = value {
                set_param(ti, effect, pid, v);
            }
        };
        match filter.match_name.as_str() {
            "AE.ADBE Motion" => {
                set(ti, "motion", "position", by_id(1).and_then(|p| point(p, space, space.frame)));
                set(ti, "motion", "scale", by_id(2).and_then(|p| param(p, space, float)));
                set(ti, "motion", "scale_width", by_id(3).and_then(|p| param(p, space, float)));
                set(ti, "motion", "uniform_scale", by_id(4).and_then(|p| param(p, space, boolean)));
                set(ti, "motion", "rotation", by_id(5).and_then(|p| param(p, space, float)));
                set(ti, "motion", "anchor", by_id(6).and_then(|p| point(p, space, space.source)));
                set(ti, "motion", "anti_flicker", by_id(7).and_then(|p| param(p, space, float)));
                if let Some(m) = ti.effect_mut("motion") {
                    m.enabled = !filter.bypass;
                }
                // Motion's crop is FilmCraft's Crop effect
                let crop: Vec<(&str, Param)> = [(8, "left"), (9, "top"), (10, "right"), (11, "bottom")]
                    .into_iter()
                    .filter_map(|(id, pid)| by_id(id).and_then(|p| param(p, space, float)).map(|v| (pid, v)))
                    .filter(|(_, v)| v.is_animated() || v.value.as_f64().is_some_and(|f| f.abs() > 1e-9))
                    .collect();
                for (pid, v) in crop {
                    set_param(ti, "crop", pid, v);
                }
            }
            "AE.ADBE Opacity" => {
                set(ti, "opacity", "opacity", by_id(1).and_then(|p| param(p, space, float)));
                if by_id(2).and_then(|p| param(p, space, float)).is_some_and(|b| b.is_animated() || b.value.as_f64().is_some_and(|v| v != 0.0)) {
                    report.warn("opacity blend modes were not imported (clips use Normal)");
                }
                if let Some(o) = ti.effect_mut("opacity") {
                    o.enabled = !filter.bypass;
                }
            }
            // each Transform is its own instance (a clip may have several)
            "AE.ADBE Geometry2" => {
                let Some(def) = find_effect("transform") else { continue };
                let mut t = def.instance();
                let mut put = |pid: &str, value: Option<Param>| {
                    if let Some(v) = value {
                        t.params.insert(pid.to_string(), v);
                    }
                };
                put("anchor", by_id(1).and_then(|p| point(p, space, space.source)));
                put("position", by_id(2).and_then(|p| point(p, space, space.source)));
                put("scale_height", by_id(3).and_then(|p| param(p, space, float)));
                put("scale_width", by_id(4).and_then(|p| param(p, space, float)));
                put("skew", by_id(5).and_then(|p| param(p, space, float)));
                put("skew_axis", by_id(6).and_then(|p| param(p, space, float)));
                put("rotation", by_id(7).and_then(|p| param(p, space, float)));
                put("opacity", by_id(8).and_then(|p| param(p, space, float)));
                // "Use Composition's Shutter Angle" is the inverse of FilmCraft's override
                put("shutter_override", by_id(9).and_then(|p| param(p, space, |v| boolean(v).and_then(|b| b.as_bool()).map(|b| ParamValue::Bool(!b)))));
                put("shutter_angle", by_id(10).and_then(|p| param(p, space, float)));
                put("uniform_scale", by_id(11).and_then(|p| param(p, space, boolean)));
                t.enabled = !filter.bypass;
                ti.effects.push(t);
            }
            "AE.ADBE Text" => report.warn("graphic text and shapes were not imported"),
            m if m.starts_with("Internal Volume") => {
                set(
                    ti,
                    "volume",
                    "level",
                    by_name("Level").and_then(|p| param(p, space, volume_db)).filter(|l| l.is_animated() || l.value.as_f64().is_some_and(|d| d.abs() > 1e-3)),
                );
                if filter.bypass {
                    set_param(ti, "volume", "bypass", Param::new(ParamValue::Bool(true)));
                }
            }
            _ => {
                // audio effects carry only an internal id: name them by their controls
                let opaque = filter.name.is_empty() || filter.name.len() == 36 && filter.name.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-');
                let controls: Vec<&str> = params.iter().map(|p| p.name.as_str()).filter(|n| !n.is_empty() && *n != "Bypass").take(4).collect();
                report.warn(match (opaque, controls.is_empty()) {
                    (false, _) => format!("effect \"{}\" is not supported and was skipped", filter.name),
                    (true, false) => format!("an effect with the controls {} is not supported and was skipped", controls.join(", ")),
                    (true, true) => "an unnamed effect is not supported and was skipped".to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> Space {
        Space { frame: (1920.0, 1080.0), source: (3840.0, 2160.0), in_point: 1000, speed: 2.0 }
    }

    #[test]
    fn values_and_keyframes() {
        assert_eq!(value("434."), Some(Value::Float(434.0)));
        assert_eq!(value("0.5:0.25"), Some(Value::Point(0.5, 0.25)));
        assert_eq!(value("true"), Some(Value::Bool(true)));
        assert_eq!(value("NaN"), None);
        assert_eq!(value("1:inf"), None);
        let p = graph::Param {
            id: Some(1),
            name: "Scale".into(),
            start: Some("-91445760000000000,50.,0,0".into()),
            keyframes: Some("3000,80.,5,0;1000,60.,0,0;bad;7,,;".into()),
        };
        let pr = param(&p, &space(), float).unwrap();
        // source times 1000 and 3000 at 200 % from source In 1000 → clip times 0 and 1000
        let keys: Vec<(i64, f64)> = pr.keyframes.iter().map(|k| (k.time.0, k.value.as_f64().unwrap())).collect();
        assert_eq!(keys, [(0, 60.0), (1000, 80.0)]);
        assert_eq!(pr.keyframes[0].interp, Interpolation::Linear);
        assert_eq!(pr.keyframes[1].interp, Interpolation::Bezier);
        assert_eq!(pr.value.as_f64(), Some(60.0));
    }

    #[test]
    fn points_scale_to_pixels_and_centres_are_left_automatic() {
        let centre = graph::Param { start: Some("0,0.5:0.5,0".into()), ..Default::default() };
        assert!(point(&centre, &space(), (1920.0, 1080.0)).is_none());
        let off = graph::Param { start: Some("0,0.25:0.5,0".into()), ..Default::default() };
        let v = point(&off, &space(), (1920.0, 1080.0)).unwrap().value.as_vec2().unwrap();
        assert_eq!((v.x, v.y), (480.0, 540.0));
    }

    #[test]
    fn volume_levels() {
        let db = |v: f64| volume_db(Value::Float(v)).and_then(|p| p.as_f64()).unwrap();
        assert!(db(0.177_827_998_996).abs() < 1e-4);
        assert!((db(1.0) - 15.0).abs() < 1e-9);
        assert_eq!(db(0.0), -287.5);
        assert!((db(0.088_914) - (-6.0206)).abs() < 1e-3);
    }
}
