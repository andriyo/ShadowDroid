//! Explicit pixel contracts and review bundles. No implicit semantic/LLM judge.
use super::{Status, provenance};
use crate::{
    device::client::ServerClient,
    ids::Serial,
    proto::{ScreenResponse, SnapshotState},
};
use anyhow::Result;
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mask {
    pub bounds: [u32; 4],
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    pub reference_png: PathBuf,
    pub reference_metadata: PathBuf,
    pub capture_check: String,
    pub capture_name: String,
    pub cell: Option<String>,
    pub max_changed_fraction: f64,
    #[serde(default)]
    pub channel_tolerance: u8,
    #[serde(default)]
    pub masks: Vec<Mask>,
}
impl Comparison {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            super::plan::valid_id(&self.capture_check)
                && super::plan::valid_id(&self.capture_name)
                && self.cell.as_ref().is_none_or(|s| super::plan::valid_id(s)),
            "invalid capture identity"
        );
        anyhow::ensure!(
            self.max_changed_fraction.is_finite()
                && (0.0..=1.0).contains(&self.max_changed_fraction),
            "pixel threshold must be an explicit fraction in 0..1"
        );
        anyhow::ensure!(self.masks.len() <= 100, "at most 100 masks");
        for mask in &self.masks {
            let [left, top, right, bottom] = mask.bounds;
            anyhow::ensure!(
                left < right && top < bottom && !mask.reason.trim().is_empty(),
                "masks need a nonempty rectangle and reason"
            );
        }
        Ok(())
    }
}
pub async fn capture(
    client: &ServerClient,
    serial: &Serial,
    out: &Path,
    name: &str,
) -> Result<(Status, Value)> {
    let started = crate::runtime::now_ms();
    let configuration = super::configuration::metadata(serial).await?;
    let before = client.stable_screen(200, 4000).await?;
    let bytes = client.screenshot_png().await?;
    let after = client.screen().await?;
    let configuration_after = super::configuration::metadata(serial).await?;
    let consistent = before.stable
        && before.screen.snapshot_state == SnapshotState::Consistent
        && after.snapshot_state == SnapshotState::Consistent
        && before.screen.screen_hash == after.screen_hash
        && configuration == configuration_after;
    if crate::redaction::is_enabled() && !consistent {
        return Ok((
            Status::Blocked,
            json!({"reason":"cannot_redact_unstable_capture_safely"}),
        ));
    }
    let bytes = if crate::redaction::is_enabled() {
        crate::redaction::redact_png_if_active(&bytes, &before.screen)?.0
    } else {
        bytes
    };
    crate::cmd::artifact::write_bytes(&out.join(format!("{name}.png")), &bytes)?;
    let metadata = json!({"schema_version":1,"image_blake3":provenance::hash(&bytes),"capture_consistent":consistent,"started_ms":started,"finished_ms":crate::runtime::now_ms(),"before":before.screen,"after":after,"configuration":configuration,"configuration_after":configuration_after,"pixel_redaction":crate::redaction::is_enabled(),"accessibility_findings":findings(&before.screen,&configuration),"limits":["Bounds do not prove touch bounds, clipping, contrast, full Compose semantics or source mapping.","Image and tree samples are taken within a bounded window, not simultaneously.","Appearance and behavior require separate assertions."]});
    super::journey::save(&out.join(format!("{name}.json")), &metadata)?;
    Ok((
        if consistent {
            Status::Passed
        } else {
            Status::Blocked
        },
        json!({"capture":name,"consistent":consistent,"visual_compliance":"not_evaluated"}),
    ))
}
fn findings(screen: &ScreenResponse, configuration: &Value) -> Vec<Value> {
    let density = configuration["density"]
        .as_str()
        .and_then(|s| {
            s.lines()
                .rev()
                .find_map(|line| line.split_once(':')?.1.trim().parse::<f64>().ok())
        })
        .map(|d| d / 160.0);
    let mut findings = vec![];
    for element in &screen.elements {
        if element.clickable
            && element.text.as_ref().is_none_or(|s| s.trim().is_empty())
            && element.desc.as_ref().is_none_or(|s| s.trim().is_empty())
        {
            findings.push(json!({"kind":"possible_missing_label","element":element.id,"rid":element.rid,"conclusive":false}));
        }
        if let Some([l, t, r, b]) = element.bounds {
            if l < 0 || t < 0 || r > screen.viewport.w as i32 || b > screen.viewport.h as i32 {
                findings.push(json!({"kind":"bounds_extend_beyond_viewport","element":element.id,"bounds":[l,t,r,b],"conclusive":false}));
            }
            if element.clickable
                && density
                    .is_some_and(|d| f64::from(r - l) < 48.0 * d || f64::from(b - t) < 48.0 * d)
            {
                findings.push(json!({"kind":"possible_small_touch_target","element":element.id,"bounds":[l,t,r,b],"reference_dp":48,"conclusive":false}));
            }
        }
    }
    findings
}
fn png(path: &Path) -> Result<(Vec<u8>, RgbaImage)> {
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= 32 * 1024 * 1024,
        "PNG exceeds 32 MiB"
    );
    let bytes = std::fs::read(path)?;
    let (w, h) = image::ImageReader::with_format(Cursor::new(&bytes), image::ImageFormat::Png)
        .into_dimensions()?;
    anyhow::ensure!(
        w > 0 && h > 0 && u64::from(w) * u64::from(h) <= 16_777_216,
        "PNG exceeds 16 megapixels"
    );
    let pixels = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)?.into_rgba8();
    Ok((bytes, pixels))
}
fn diff(
    reference: &RgbaImage,
    actual: &RgbaImage,
    spec: &Comparison,
) -> Result<(Value, RgbaImage)> {
    anyhow::ensure!(
        reference.dimensions() == actual.dimensions(),
        "reference and capture viewport mismatch"
    );
    let (w, h) = actual.dimensions();
    for mask in &spec.masks {
        anyhow::ensure!(
            mask.bounds[2] <= w && mask.bounds[3] <= h,
            "mask extends beyond viewport"
        );
    }
    let mut changed = 0u64;
    let mut compared = 0u64;
    let mut masked = 0u64;
    let mut image = RgbaImage::new(w, h);
    for (x, y, pixel) in actual.enumerate_pixels() {
        if spec
            .masks
            .iter()
            .any(|m| x >= m.bounds[0] && y >= m.bounds[1] && x < m.bounds[2] && y < m.bounds[3])
        {
            masked += 1;
            image.put_pixel(x, y, Rgba([100, 100, 100, 255]));
            continue;
        }
        compared += 1;
        let different = pixel
            .0
            .iter()
            .zip(reference.get_pixel(x, y).0)
            .any(|(a, b)| a.abs_diff(b) > spec.channel_tolerance);
        if different {
            changed += 1;
            image.put_pixel(x, y, Rgba([255, 0, 180, 255]));
        } else {
            image.put_pixel(x, y, Rgba([pixel[0] / 2, pixel[1] / 2, pixel[2] / 2, 255]));
        }
    }
    anyhow::ensure!(compared > 0, "masks exclude every pixel");
    let fraction = changed as f64 / compared as f64;
    Ok((
        json!({"changed_pixels":changed,"compared_pixels":compared,"masked_pixels":masked,"changed_fraction":fraction,"max_changed_fraction":spec.max_changed_fraction,"channel_tolerance":spec.channel_tolerance,"pixel_contract_satisfied":fraction<=spec.max_changed_fraction,"semantic_visual_compliance":"not_evaluated"}),
        image,
    ))
}
pub fn run(
    root: &Path,
    run: &Path,
    out: &Path,
    spec: &Comparison,
) -> Result<(Status, Value, bool, bool)> {
    spec.validate()?;
    let mut capture = run.join("checks").join(&spec.capture_check);
    if let Some(cell) = &spec.cell {
        capture = capture.join(cell);
    }
    let reference_metadata: Value =
        serde_json::from_slice(&std::fs::read(root.join(&spec.reference_metadata))?)?;
    let actual_metadata: Value = serde_json::from_slice(&std::fs::read(
        capture.join(format!("{}.json", spec.capture_name)),
    )?)?;
    let (reference_bytes, reference) = png(&root.join(&spec.reference_png))?;
    let (actual_bytes, actual) = png(&capture.join(format!("{}.png", spec.capture_name)))?;
    for (metadata, bytes) in [
        (&reference_metadata, &reference_bytes),
        (&actual_metadata, &actual_bytes),
    ] {
        anyhow::ensure!(
            metadata["schema_version"] == 1 && metadata["capture_consistent"] == true,
            "reference/capture metadata is incomplete or unstable"
        );
        anyhow::ensure!(
            metadata["image_blake3"] == provenance::hash(bytes),
            "image and metadata hash mismatch"
        );
    }
    anyhow::ensure!(
        reference_metadata["before"]["viewport"] == actual_metadata["before"]["viewport"]
            && reference_metadata["configuration"] == actual_metadata["configuration"],
        "reference/capture configuration mismatch"
    );
    anyhow::ensure!(
        reference_metadata["before"]["current_app"]["package"]
            == actual_metadata["before"]["current_app"]["package"],
        "reference/capture app mismatch"
    );
    anyhow::ensure!(
        reference_metadata["pixel_redaction"] == actual_metadata["pixel_redaction"],
        "reference/capture redaction mismatch"
    );
    let (metric, difference) = diff(&reference, &actual, spec)?;
    crate::cmd::artifact::write_bytes(&out.join("reference.png"), &reference_bytes)?;
    crate::cmd::artifact::write_bytes(&out.join("actual.png"), &actual_bytes)?;
    difference.save(out.join("difference.png"))?;
    let evidence = json!({"adapter":"visual_comparison","metric":metric,"masks":spec.masks,"reference_metadata":reference_metadata,"actual_metadata":actual_metadata,"judge":"explicit_pixel_contract_no_model","limitations":["Small system rendering variations can create large pixel differences.","This is not Android Bench's semantic visual judge.","Review image pairs and accessibility findings; use separate behavioral checks."]});
    super::journey::save(&out.join("review.json"), &evidence)?;
    Ok((
        if metric["pixel_contract_satisfied"] == true {
            Status::Passed
        } else {
            Status::Failed
        },
        evidence,
        false,
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_masks_ignore_only_their_rectangle_and_mismatch_never_passes() {
        let reference = RgbaImage::from_pixel(10, 10, Rgba([0, 0, 0, 255]));
        let mut actual = reference.clone();
        actual.put_pixel(0, 0, Rgba([255, 255, 255, 255]));
        let mut spec = Comparison {
            reference_png: "ref.png".into(),
            reference_metadata: "ref.json".into(),
            capture_check: "c".into(),
            capture_name: "capture".into(),
            cell: None,
            max_changed_fraction: 0.0,
            channel_tolerance: 0,
            masks: vec![],
        };
        assert_eq!(
            diff(&reference, &actual, &spec).unwrap().0["pixel_contract_satisfied"],
            false
        );
        spec.masks.push(Mask {
            bounds: [0, 0, 1, 1],
            reason: "declared system indicator".into(),
        });
        assert_eq!(
            diff(&reference, &actual, &spec).unwrap().0["pixel_contract_satisfied"],
            true
        );
        actual.put_pixel(2, 2, Rgba([255, 255, 255, 255]));
        assert_eq!(
            diff(&reference, &actual, &spec).unwrap().0["pixel_contract_satisfied"],
            false
        );
        assert!(diff(&reference, &RgbaImage::new(11, 10), &spec).is_err());
        spec.masks[0].bounds = [0, 0, 10, 10];
        assert!(diff(&reference, &actual, &spec).is_err());
    }
}
