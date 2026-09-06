//! Encoded duration is evidence, host time is an estimate. Never manufacture
//! an exact seek position when screenrecord's media clock omits idle time.

use super::session::{Manifest, Marker, Segment};
use serde_json::{Value, json};

pub fn report(manifest: &Manifest) -> Value {
    let exported = manifest
        .artifacts
        .iter()
        .any(|a| a.path == "video.mp4" && a.complete);
    let mut value = intervals(
        &manifest.segments,
        &manifest.markers,
        manifest.capture.started_at,
        exported,
    );
    value["wall_elapsed_ms"] = manifest.capture.elapsed_ms.into();
    value["gaps"] = json!(manifest.gaps);
    value
}

pub fn summary(manifest: &Manifest) -> Value {
    let mut report = report(manifest);
    if let Some(fields) = report.as_object_mut() {
        for key in ["segments", "markers", "gaps"] {
            let count = fields
                .remove(key)
                .and_then(|v| v.as_array().map(Vec::len))
                .unwrap_or(0);
            fields.insert(format!("{key}_count"), count.into());
        }
    }
    report
}

fn intervals(segments: &[Segment], markers: &[Marker], started_at: f64, exported: bool) -> Value {
    let mut encoded_ms = 0u64;
    let mut unavailable = Vec::new();
    let mut unverified = Vec::new();
    let mut entries = Vec::new();
    for segment in segments {
        let playable = segment.state == "complete" && segment.playable;
        let duration = segment.media_duration_ms.unwrap_or(0);
        let start = encoded_ms;
        if playable {
            encoded_ms = encoded_ms.saturating_add(duration);
        } else if segment.state == "complete" {
            unavailable.push(segment.index);
        } else {
            unverified.push(segment.index);
        }
        entries.push(json!({
            "segment_index":segment.index, "state":segment.state, "playable":playable,
            "host_start_ms":super::session::elapsed_ms(started_at, segment.started_at),
            "host_elapsed_ms":segment.elapsed_ms, "media_duration_ms":segment.media_duration_ms,
            "sample_count":segment.sample_count,
            "host_media_difference_ms":playable.then(|| segment.elapsed_ms.abs_diff(duration)),
            "concatenation_range_ms":playable.then_some([start, encoded_ms]),
            "export_range_ms":(playable && exported).then_some([start, encoded_ms]),
        }));
    }
    let markers: Vec<_> = markers.iter().map(|marker| {
        let segment = segments.iter().find(|s| Some(s.index) == marker.segment_index);
        let entry = entries.iter().find(|s| s["segment_index"].as_u64() == marker.segment_index.map(u64::from));
        let range = entry.and_then(|e| e.get("export_range_ms")).cloned().unwrap_or(Value::Null);
        // Even similar total durations do not establish frame-to-host alignment.
        // Give the enclosing encoded segment, not an invented point timestamp.
        json!({"label":marker.label, "host_elapsed_ms":marker.elapsed_ms,
            "segment_index":marker.segment_index, "segment_elapsed_ms":marker.segment_elapsed_ms,
            "export_range_ms":range, "export_offset_ms":Value::Null,
            "mapping":if !exported { "not_exported" }
                else if segment.is_some_and(|s| s.state == "complete" && s.playable) { "segment_only" }
                else { "unavailable" },
        })
    }).collect();
    json!({"schema_version":1, "encoded_duration_ms":encoded_ms,
        "export_available":exported, "unavailable_segments":unavailable, "unverified_segments":unverified,
        "segments":entries, "markers":markers,
        "timing_basis":"host observation and encoded media clocks are not frame-synchronized"})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_idle_segment_does_not_shift_markers_into_unrelated_video() {
        let segments: Vec<_> = [(1, 5000, true), (2, 0, false), (3, 3000, true)]
            .into_iter()
            .map(|(index, duration, playable)| Segment {
                index,
                started_at: f64::from(index - 1) * 10.0,
                elapsed_ms: 10000,
                media_duration_ms: Some(duration),
                playable,
                state: "complete".into(),
                ..Default::default()
            })
            .collect();
        let markers = [
            Marker {
                label: "during idle".into(),
                segment_index: Some(2),
                ..Default::default()
            },
            Marker {
                label: "after idle".into(),
                segment_index: Some(3),
                ..Default::default()
            },
        ];
        let report = intervals(&segments, &markers, 0.0, true);
        assert_eq!(report["encoded_duration_ms"], 8000);
        assert_eq!(report["unavailable_segments"], json!([2]));
        assert_eq!(report["markers"][0]["mapping"], "unavailable");
        assert!(report["markers"][0]["export_range_ms"].is_null());
        assert_eq!(report["markers"][1]["export_range_ms"], json!([5000, 8000]));
        assert!(report["markers"][1]["export_offset_ms"].is_null());
        assert!(
            intervals(&segments, &markers, 0.0, false)["markers"][1]["export_range_ms"].is_null()
        );
    }
}
