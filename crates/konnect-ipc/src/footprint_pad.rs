//! Reading and setting the solder-mask margin of the pads of a placed
//! footprint.
//!
//! KiCad keeps a pad's mask override in its pad stack:
//! `PadStack.front_outer_layers.solder_mask_settings.solder_mask_margin`,
//! with the same field on `back_outer_layers`. KiCad 10 supports only one
//! margin for both sides, so both are written. `PADSTACK::Deserialize`
//! reads the margin independently of `solder_mask_mode` (the mode only
//! carries the masked / unmasked / from-rules choice), so the mode is left
//! exactly as KiCad sent it.
//!
//! Like the footprint texts, pads travel inside the parent footprint's
//! `definition.items`; an edit rewrites those items and the caller sends the
//! whole footprint back in one `UpdateItems`. The logic here works on
//! decoded messages only, so every refusal is testable without KiCad.

use crate::builders::{any_type_name, mm_to_nm, nm_to_mm, pack_any};
use crate::gen::kiapi;
use crate::types::IpcPadMaskMargin;
use anyhow::Result;
use prost::Message;

use kiapi::board::types::{FootprintInstance, Pad, PadStackOuterLayer, SolderMaskOverrides};

/// The most negative margin an edit may set, in mm.
pub const MIN_PAD_MASK_MARGIN_MM: f64 = -1.0;
/// The largest margin an edit may set, in mm.
pub const MAX_PAD_MASK_MARGIN_MM: f64 = 2.0;

const PAD_TYPE: &str = "kiapi.board.types.Pad";

fn margin_of(layer: Option<&PadStackOuterLayer>) -> Option<f64> {
    layer
        .and_then(|l| l.solder_mask_settings.as_ref())
        .and_then(|s| s.solder_mask_margin.as_ref())
        .map(|d| nm_to_mm(d.value_nm))
}

fn describe(pad: &Pad) -> IpcPadMaskMargin {
    let stack = pad.pad_stack.as_ref();
    IpcPadMaskMargin {
        number: pad.number.clone(),
        margin: margin_of(stack.and_then(|s| s.front_outer_layers.as_ref())),
        back_margin: margin_of(stack.and_then(|s| s.back_outer_layers.as_ref())),
    }
}

/// The pads of a placed footprint in the order it holds them.
fn pads(fp: &FootprintInstance) -> Vec<Pad> {
    fp.definition
        .as_ref()
        .map(|d| d.items.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|any| any_type_name(any) == PAD_TYPE)
        .filter_map(|any| Pad::decode(any.value.as_slice()).ok())
        .collect()
}

/// Every pad's mask margin, in the footprint's order.
pub fn pad_mask_margins(fp: &FootprintInstance) -> Vec<IpcPadMaskMargin> {
    pads(fp).iter().map(describe).collect()
}

/// Set the mask margin of the pads numbered in `numbers` (every pad when
/// empty) to `margin_mm`, front and back. Returns the margins of the edited
/// pads as written. Every refusal — a margin out of range, a pad number the
/// footprint does not have, a footprint without pads, a pad without a pad
/// stack — happens before anything is touched, so `fp` is unchanged when
/// this returns an error.
pub fn set_pad_mask_margin(
    fp: &mut FootprintInstance,
    numbers: &[String],
    margin_mm: f64,
) -> Result<Vec<IpcPadMaskMargin>> {
    if !(margin_mm.is_finite()
        && (MIN_PAD_MASK_MARGIN_MM..=MAX_PAD_MASK_MARGIN_MM).contains(&margin_mm))
    {
        anyhow::bail!(
            "mask margin {margin_mm} mm is outside [{MIN_PAD_MASK_MARGIN_MM}, \
             {MAX_PAD_MASK_MARGIN_MM}] mm"
        );
    }
    let reference = crate::footprint_text::reference_of(fp).to_string();
    let present = pads(fp);
    if present.is_empty() {
        anyhow::bail!("footprint '{reference}' has no pads");
    }
    if let Some(missing) = numbers
        .iter()
        .find(|n| !present.iter().any(|p| &p.number == *n))
    {
        let mut have: Vec<String> = present.iter().map(|p| format!("'{}'", p.number)).collect();
        have.dedup();
        anyhow::bail!(
            "footprint '{reference}' has no pad '{missing}' (it has {})",
            have.join(", ")
        );
    }
    let selected = |pad: &Pad| numbers.is_empty() || numbers.contains(&pad.number);
    if let Some(bare) = present
        .iter()
        .find(|p| selected(p) && p.pad_stack.is_none())
    {
        anyhow::bail!(
            "pad '{}' of footprint '{reference}' carries no pad stack, so it has no mask \
             settings to change",
            bare.number
        );
    }

    let margin = kiapi::common::types::Distance {
        value_nm: mm_to_nm(margin_mm),
    };
    let mut edited = Vec::new();
    let items = fp
        .definition
        .as_mut()
        .map(|d| d.items.as_mut_slice())
        .unwrap_or_default();
    for any in items.iter_mut() {
        if any_type_name(any) != PAD_TYPE {
            continue;
        }
        let Ok(mut pad) = Pad::decode(any.value.as_slice()) else {
            continue;
        };
        if !selected(&pad) {
            continue;
        }
        let Some(stack) = pad.pad_stack.as_mut() else {
            continue;
        };
        for side in [&mut stack.front_outer_layers, &mut stack.back_outer_layers] {
            // Only the margin is replaced: the mask mode, paste settings and
            // via covering/plugging modes stay as KiCad sent them.
            side.get_or_insert_with(Default::default)
                .solder_mask_settings
                .get_or_insert_with(SolderMaskOverrides::default)
                .solder_mask_margin = Some(margin);
        }
        *any = pack_any(&pad, PAD_TYPE);
        edited.push(describe(&pad));
    }
    Ok(edited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::vec2;
    use kiapi::board::types::{BoardText, Field, PadStack, SolderMaskMode};

    fn pad(number: &str, front_margin_nm: Option<i64>) -> Pad {
        Pad {
            number: number.to_string(),
            position: Some(vec2(0.0, 0.0)),
            pad_stack: Some(PadStack {
                front_outer_layers: Some(PadStackOuterLayer {
                    solder_mask_mode: SolderMaskMode::SmmFromDesignRules as i32,
                    solder_mask_settings: front_margin_nm.map(|nm| SolderMaskOverrides {
                        solder_mask_margin: Some(kiapi::common::types::Distance { value_nm: nm }),
                    }),
                    ..Default::default()
                }),
                // The back side is absent, as KiCad sends it for an SMD pad
                // with nothing set there.
                back_outer_layers: None,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn footprint(pads: Vec<Pad>) -> FootprintInstance {
        let text = BoardText {
            text: Some(kiapi::common::types::Text {
                text: "FID1".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut items: Vec<_> = pads.iter().map(|p| pack_any(p, PAD_TYPE)).collect();
        items.push(pack_any(&text, "kiapi.board.types.BoardText"));
        FootprintInstance {
            reference_field: Some(Field {
                name: "Reference".to_string(),
                text: Some(text),
                visible: true,
                ..Default::default()
            }),
            definition: Some(kiapi::board::types::Footprint {
                items,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn reads_the_margin_or_none() {
        let fp = footprint(vec![pad("1", Some(50_000)), pad("2", None)]);
        let margins = pad_mask_margins(&fp);
        assert_eq!(margins.len(), 2);
        assert!(close(margins[0].margin, 0.05));
        assert_eq!(margins[1].margin, None);
        assert_eq!(margins[0].back_margin, None);
    }

    #[test]
    fn sets_every_pad_front_and_back_and_keeps_the_mode() {
        // A fiducial's single pad has an empty number.
        let mut fp = footprint(vec![pad("", None)]);
        let edited = set_pad_mask_margin(&mut fp, &[], 0.5).unwrap();
        assert_eq!(edited.len(), 1);
        assert_eq!(edited[0].number, "");
        assert!(close(edited[0].margin, 0.5));
        assert!(close(edited[0].back_margin, 0.5));
        // Read back from the re-packed footprint, not from the writer's answer.
        let after = pad_mask_margins(&fp);
        assert!(close(after[0].margin, 0.5) && close(after[0].back_margin, 0.5));
        let pad = pads(&fp).remove(0);
        let front = pad.pad_stack.unwrap().front_outer_layers.unwrap();
        assert_eq!(
            front.solder_mask_mode,
            SolderMaskMode::SmmFromDesignRules as i32
        );
        // The footprint's other items are untouched.
        assert_eq!(fp.definition.as_ref().unwrap().items.len(), 2);
    }

    #[test]
    fn named_pads_only_and_zero_or_negative_margins() {
        let mut fp = footprint(vec![
            pad("1", Some(50_000)),
            pad("2", Some(50_000)),
            pad("2", None),
        ]);
        let edited = set_pad_mask_margin(&mut fp, &["2".to_string()], -0.05).unwrap();
        assert_eq!(edited.len(), 2, "both pads numbered 2");
        let after = pad_mask_margins(&fp);
        assert!(close(after[0].margin, 0.05), "pad 1 untouched");
        assert!(close(after[1].margin, -0.05) && close(after[2].margin, -0.05));
        set_pad_mask_margin(&mut fp, &["1".to_string()], 0.0).unwrap();
        assert!(close(pad_mask_margins(&fp)[0].margin, 0.0));
    }

    #[test]
    fn refusals_leave_the_footprint_untouched() {
        let original = footprint(vec![pad("1", Some(50_000)), pad("2", None)]);
        let mut bare = pad("3", None);
        bare.pad_stack = None;
        let with_bare = footprint(vec![pad("1", None), bare]);
        let cases: Vec<(FootprintInstance, Vec<String>, f64)> = vec![
            (original.clone(), vec!["9".to_string()], 0.5),
            (original.clone(), vec!["1".to_string(), "".to_string()], 0.5),
            (original.clone(), vec![], 2.5),
            (original.clone(), vec![], -1.5),
            (original.clone(), vec![], f64::NAN),
            (footprint(vec![]), vec![], 0.5),
            (with_bare.clone(), vec![], 0.5),
        ];
        for (fp, numbers, margin) in cases {
            let mut edited = fp.clone();
            assert!(
                set_pad_mask_margin(&mut edited, &numbers, margin).is_err(),
                "{numbers:?} / {margin} should be refused"
            );
            assert_eq!(edited, fp, "{numbers:?} / {margin} mutated the footprint");
        }
    }
}
