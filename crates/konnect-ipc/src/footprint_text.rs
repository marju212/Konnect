//! Reading and moving the texts of a placed footprint: its reference and value
//! fields, its custom fields (LCSC, MPN, ...) and its plain `fp_text` items.
//!
//! KiCad carries every one of them in absolute board coordinates over IPC,
//! with an absolute text angle, and re-creates the footprint verbatim from
//! the `FootprintInstance` sent back in `UpdateItems`. Moving a text is
//! therefore an edit of the parent footprint: find the text, rewrite its
//! position, angle, layer or visibility, and send the whole footprint.
//!
//! The logic here works on decoded messages only, so every refusal and every
//! coordinate conversion is testable without a running KiCad.

use crate::builders::{any_type_name, mm_to_nm, nm_to_mm, pack_any};
use crate::gen::kiapi;
use crate::transform::Xform;
use crate::types::{IpcFootprintText, IpcFootprintTextEdit, IpcTextTarget, IpcVector2};
use anyhow::{Context, Result};
use prost::Message;

use kiapi::board::types::{BoardText, Field, FootprintInstance};

/// The reference designator of a placed footprint, `""` when it has none.
pub(crate) fn reference_of(fp: &FootprintInstance) -> &str {
    fp.reference_field
        .as_ref()
        .and_then(|f| f.text.as_ref())
        .and_then(|bt| bt.text.as_ref())
        .map(|t| t.text.as_str())
        .unwrap_or("")
}

fn frame(fp: &FootprintInstance) -> ((i64, i64), f64) {
    let pos = fp.position.unwrap_or_default();
    let angle = fp
        .orientation
        .as_ref()
        .map(|a| a.value_degrees)
        .unwrap_or(0.0);
    ((pos.x_nm, pos.y_nm), angle)
}

fn layer_name(layer: i32) -> String {
    kiapi::board::types::BoardLayer::try_from(layer)
        .ok()
        .and_then(crate::builders::layer_name)
        .unwrap_or("Unknown")
        .to_string()
}

/// Describe one board text of `fp`. `field` is the field name, `None` for a
/// plain text item.
fn describe(
    fp: &FootprintInstance,
    field: Option<&str>,
    visible: bool,
    bt: &BoardText,
) -> IpcFootprintText {
    let (anchor, angle) = frame(fp);
    let text = bt.text.as_ref();
    let pos = text.and_then(|t| t.position).unwrap_or_default();
    let to_local = Xform::Rotate {
        cx_nm: anchor.0,
        cy_nm: anchor.1,
        delta_deg: -angle,
    };
    let (lx, ly) = to_local.point(pos.x_nm, pos.y_nm);
    let attrs = text.and_then(|t| t.attributes.as_ref());
    IpcFootprintText {
        reference: reference_of(fp).to_string(),
        field: field.map(str::to_string),
        uuid: bt
            .id
            .as_ref()
            .map(|k| k.value.clone())
            .filter(|v| !v.is_empty()),
        text: text.map(|t| t.text.clone()).unwrap_or_default(),
        layer: layer_name(bt.layer),
        position: IpcVector2 {
            x: nm_to_mm(pos.x_nm),
            y: nm_to_mm(pos.y_nm),
        },
        local: IpcVector2 {
            x: nm_to_mm(lx - anchor.0),
            y: nm_to_mm(ly - anchor.1),
        },
        angle: attrs
            .and_then(|a| a.angle.as_ref())
            .map(|a| a.value_degrees)
            .unwrap_or(0.0),
        size: attrs.and_then(|a| a.size).map(|s| IpcVector2 {
            x: nm_to_mm(s.x_nm),
            y: nm_to_mm(s.y_nm),
        }),
        visible,
    }
}

fn describe_field(fp: &FootprintInstance, field: &Field) -> Option<IpcFootprintText> {
    let bt = field.text.as_ref()?;
    // Fields are addressed by name; a UUID here would invite a move by UUID,
    // which only reaches plain text items.
    let mut text = describe(fp, Some(&field.name), field.visible, bt);
    text.uuid = None;
    Some(text)
}

/// Every text of a placed footprint: the four fixed fields first, then the
/// custom fields and plain text items in the order the footprint holds them.
pub fn list_texts(fp: &FootprintInstance) -> Vec<IpcFootprintText> {
    let mut out: Vec<IpcFootprintText> = [
        &fp.reference_field,
        &fp.value_field,
        &fp.datasheet_field,
        &fp.description_field,
    ]
    .into_iter()
    .flatten()
    .filter_map(|f| describe_field(fp, f))
    .collect();

    for any in fp
        .definition
        .as_ref()
        .map(|d| d.items.as_slice())
        .unwrap_or_default()
    {
        match any_type_name(any) {
            "kiapi.board.types.Field" => {
                if let Ok(field) = Field::decode(any.value.as_slice()) {
                    out.extend(describe_field(fp, &field));
                }
            }
            "kiapi.board.types.BoardText" => {
                if let Ok(bt) = BoardText::decode(any.value.as_slice()) {
                    out.push(describe(fp, None, true, &bt));
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether `name` addresses a field called `field_name`. Matching ignores
/// case so `reference`, `Reference` and `REF` style callers all land, and
/// `ref`/`val` are accepted for the two fields everybody abbreviates.
fn field_matches(name: &str, field_name: &str) -> bool {
    let want = match name.to_ascii_lowercase().as_str() {
        "ref" | "refdes" | "reference" => "reference".to_string(),
        "val" | "value" => "value".to_string(),
        other => other.to_string(),
    };
    field_name.eq_ignore_ascii_case(&want)
}

/// Rewrite one text in place. Refusals happen before any mutation.
fn apply_to_text(
    bt: &mut BoardText,
    visible: Option<&mut bool>,
    edit: &IpcFootprintTextEdit,
    anchor: (i64, i64),
    fp_angle: f64,
) -> Result<()> {
    let layer = match edit.layer.as_deref() {
        Some(name) => Some(crate::builders::try_layer_from_name(name)? as i32),
        None => None,
    };
    if edit.visible.is_some() && visible.is_none() {
        anyhow::bail!("a plain footprint text has no visibility flag; only fields can be hidden");
    }

    let text = bt.text.get_or_insert_with(Default::default);
    let old = text.position.unwrap_or_default();
    let new = match (edit.position, edit.offset, edit.local) {
        (Some((x, y)), None, None) => (mm_to_nm(x), mm_to_nm(y)),
        (None, Some((dx, dy)), None) => (old.x_nm + mm_to_nm(dx), old.y_nm + mm_to_nm(dy)),
        (None, None, Some((lx, ly))) => {
            let to_board = Xform::Rotate {
                cx_nm: 0,
                cy_nm: 0,
                delta_deg: fp_angle,
            };
            let (bx, by) = to_board.point(mm_to_nm(lx), mm_to_nm(ly));
            (anchor.0 + bx, anchor.1 + by)
        }
        (None, None, None) => (old.x_nm, old.y_nm),
        _ => anyhow::bail!(
            "give at most one of position (x, y), offset (dx, dy) or local (local_x, local_y)"
        ),
    };
    text.position = Some(kiapi::common::types::Vector2 {
        x_nm: new.0,
        y_nm: new.1,
    });
    if let Some(angle) = edit.angle {
        text.attributes.get_or_insert_with(Default::default).angle =
            Some(kiapi::common::types::Angle {
                value_degrees: angle,
            });
    }
    if let Some(layer) = layer {
        bt.layer = layer;
    }
    if let (Some(flag), Some(v)) = (visible, edit.visible) {
        *flag = v;
    }
    Ok(())
}

/// Apply `edit` to the text it targets inside `fp`. Returns the text as it
/// stands afterwards. `fp` is untouched when this returns an error.
pub fn apply_edit(
    fp: &mut FootprintInstance,
    edit: &IpcFootprintTextEdit,
) -> Result<IpcFootprintText> {
    let (anchor, fp_angle) = frame(fp);
    let reference = reference_of(fp).to_string();

    match &edit.target {
        IpcTextTarget::Field(name) => {
            // The four fixed fields live on the instance itself.
            let fixed = [
                &mut fp.reference_field,
                &mut fp.value_field,
                &mut fp.datasheet_field,
                &mut fp.description_field,
            ]
            .into_iter()
            .flat_map(|slot| slot.as_mut())
            .find(|field| field_matches(name, &field.name));
            if let Some(field) = fixed {
                let mut probe = field.clone();
                let Field { text, visible, .. } = &mut probe;
                let bt = text.get_or_insert_with(Default::default);
                apply_to_text(bt, Some(visible), edit, anchor, fp_angle)?;
                *field = probe.clone();
                return describe_field(fp, &probe).context("the edited field lost its text");
            }
            // Custom fields live among the footprint's children.
            let items = fp
                .definition
                .as_mut()
                .map(|d| d.items.as_mut_slice())
                .unwrap_or_default();
            for any in items.iter_mut() {
                if any_type_name(any) != "kiapi.board.types.Field" {
                    continue;
                }
                let Ok(mut field) = Field::decode(any.value.as_slice()) else {
                    continue;
                };
                if !field_matches(name, &field.name) {
                    continue;
                }
                let Field { text, visible, .. } = &mut field;
                let bt = text.get_or_insert_with(Default::default);
                apply_to_text(bt, Some(visible), edit, anchor, fp_angle)?;
                *any = pack_any(&field, "kiapi.board.types.Field");
                return describe_field(fp, &field).context("the edited field lost its text");
            }
            anyhow::bail!(
                "footprint '{reference}' has no field named '{name}' — call \
                 list_board_footprint_texts for the ones it has"
            )
        }
        IpcTextTarget::Uuid(uuid) => {
            let items = fp
                .definition
                .as_mut()
                .map(|d| d.items.as_mut_slice())
                .unwrap_or_default();
            for any in items.iter_mut() {
                if any_type_name(any) != "kiapi.board.types.BoardText" {
                    continue;
                }
                let Ok(mut bt) = BoardText::decode(any.value.as_slice()) else {
                    continue;
                };
                if bt.id.as_ref().map(|k| k.value.as_str()) != Some(uuid.as_str()) {
                    continue;
                }
                apply_to_text(&mut bt, None, edit, anchor, fp_angle)?;
                *any = pack_any(&bt, "kiapi.board.types.BoardText");
                return Ok(describe(fp, None, true, &bt));
            }
            anyhow::bail!(
                "footprint '{reference}' has no text with UUID '{uuid}' — call \
                 list_board_footprint_texts for the ones it has"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::vec2;

    fn field(name: &str, text: &str, x: f64, y: f64, layer: i32) -> Field {
        Field {
            name: name.to_string(),
            visible: true,
            text: Some(BoardText {
                text: Some(kiapi::common::types::Text {
                    position: Some(vec2(x, y)),
                    text: text.to_string(),
                    attributes: Some(kiapi::common::types::TextAttributes {
                        size: Some(vec2(1.0, 1.0)),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                layer,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn silk() -> i32 {
        crate::builders::layer_from_name("F.SilkS") as i32
    }

    fn footprint(x: f64, y: f64, rotation: f64) -> FootprintInstance {
        let lcsc = field("LCSC", "C123", x, y + 3.0, silk());
        let user = BoardText {
            id: Some(kiapi::common::types::Kiid {
                value: "u-1".to_string(),
            }),
            text: Some(kiapi::common::types::Text {
                position: Some(vec2(x + 2.0, y)),
                text: "${REFERENCE}".to_string(),
                ..Default::default()
            }),
            layer: crate::builders::layer_from_name("F.Fab") as i32,
            ..Default::default()
        };
        FootprintInstance {
            position: Some(vec2(x, y)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: rotation,
            }),
            reference_field: Some(field("Reference", "R1", x, y - 1.5, silk())),
            value_field: Some(field("Value", "10k", x, y + 1.5, silk())),
            definition: Some(kiapi::board::types::Footprint {
                items: vec![
                    pack_any(&lcsc, "kiapi.board.types.Field"),
                    pack_any(&user, "kiapi.board.types.BoardText"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn edit(target: IpcTextTarget) -> IpcFootprintTextEdit {
        IpcFootprintTextEdit {
            reference: "R1".to_string(),
            target,
            position: None,
            offset: None,
            local: None,
            angle: None,
            layer: None,
            visible: None,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn lists_fixed_fields_custom_fields_and_plain_texts() {
        let texts = list_texts(&footprint(10.0, 20.0, 0.0));
        let names: Vec<_> = texts.iter().map(|t| t.field.clone()).collect();
        assert_eq!(
            names,
            vec![
                Some("Reference".to_string()),
                Some("Value".to_string()),
                Some("LCSC".to_string()),
                None
            ]
        );
        assert_eq!(texts[0].text, "R1");
        assert_eq!(texts[0].layer, "F.SilkS");
        assert!(close(texts[0].local.y, -1.5));
        assert_eq!(texts[3].uuid.as_deref(), Some("u-1"));
    }

    #[test]
    fn local_coordinates_undo_the_footprint_rotation() {
        // At 90° KiCad maps local (lx, ly) to board (x + ly, y − lx); a
        // reference sitting at board (x − 1.5, y) is therefore local (0, −1.5).
        let mut fp = footprint(10.0, 20.0, 90.0);
        fp.reference_field
            .as_mut()
            .unwrap()
            .text
            .as_mut()
            .unwrap()
            .text
            .as_mut()
            .unwrap()
            .position = Some(vec2(8.5, 20.0));
        let r = &list_texts(&fp)[0];
        assert!(close(r.local.x, 0.0), "{:?}", r.local);
        assert!(close(r.local.y, -1.5), "{:?}", r.local);
    }

    #[test]
    fn absolute_position_moves_only_the_named_field() {
        let mut fp = footprint(10.0, 20.0, 0.0);
        let before = list_texts(&fp);
        let mut e = edit(IpcTextTarget::Field("reference".to_string()));
        e.position = Some((12.0, 17.0));
        let after = apply_edit(&mut fp, &e).unwrap();
        assert!(close(after.position.x, 12.0) && close(after.position.y, 17.0));
        let now = list_texts(&fp);
        assert!(close(now[0].position.x, 12.0));
        // Everything else stays where it was.
        for (a, b) in before.iter().zip(now.iter()).skip(1) {
            assert!(close(a.position.x, b.position.x) && close(a.position.y, b.position.y));
        }
    }

    #[test]
    fn offset_shifts_from_the_current_position() {
        let mut fp = footprint(10.0, 20.0, 0.0);
        let mut e = edit(IpcTextTarget::Field("Value".to_string()));
        e.offset = Some((0.5, 1.0));
        let after = apply_edit(&mut fp, &e).unwrap();
        assert!(close(after.position.x, 10.5) && close(after.position.y, 22.5));
    }

    #[test]
    fn local_position_follows_the_footprint_rotation() {
        let mut fp = footprint(10.0, 20.0, 90.0);
        let mut e = edit(IpcTextTarget::Field("ref".to_string()));
        e.local = Some((0.0, -2.0));
        let after = apply_edit(&mut fp, &e).unwrap();
        assert!(close(after.position.x, 8.0), "{:?}", after.position);
        assert!(close(after.position.y, 20.0), "{:?}", after.position);
        assert!(close(after.local.y, -2.0));
    }

    #[test]
    fn custom_fields_angle_layer_and_visibility_are_written() {
        let mut fp = footprint(10.0, 20.0, 0.0);
        let mut e = edit(IpcTextTarget::Field("lcsc".to_string()));
        e.angle = Some(90.0);
        e.layer = Some("F.Fab".to_string());
        e.visible = Some(false);
        let after = apply_edit(&mut fp, &e).unwrap();
        assert_eq!(after.layer, "F.Fab");
        assert!(close(after.angle, 90.0));
        assert!(!after.visible);
        let listed = list_texts(&fp);
        assert_eq!(listed[2].layer, "F.Fab");
        assert!(!listed[2].visible);
    }

    #[test]
    fn plain_text_is_addressed_by_uuid() {
        let mut fp = footprint(10.0, 20.0, 0.0);
        let mut e = edit(IpcTextTarget::Uuid("u-1".to_string()));
        e.offset = Some((-1.0, 0.0));
        let after = apply_edit(&mut fp, &e).unwrap();
        assert!(close(after.position.x, 11.0));
    }

    #[test]
    fn refusals_leave_the_footprint_untouched() {
        let original = footprint(10.0, 20.0, 0.0);
        let cases = {
            let mut two_modes = edit(IpcTextTarget::Field("Reference".to_string()));
            two_modes.position = Some((1.0, 1.0));
            two_modes.offset = Some((1.0, 1.0));
            let mut bad_layer = edit(IpcTextTarget::Field("Reference".to_string()));
            bad_layer.layer = Some("Nowhere".to_string());
            let mut hide_plain = edit(IpcTextTarget::Uuid("u-1".to_string()));
            hide_plain.visible = Some(false);
            let missing = edit(IpcTextTarget::Field("MPN".to_string()));
            let missing_uuid = edit(IpcTextTarget::Uuid("nope".to_string()));
            vec![two_modes, bad_layer, hide_plain, missing, missing_uuid]
        };
        for e in cases {
            let mut fp = original.clone();
            assert!(
                apply_edit(&mut fp, &e).is_err(),
                "{:?} should be refused",
                e.target
            );
            assert_eq!(fp, original, "{:?} mutated the footprint", e.target);
        }
    }
}
