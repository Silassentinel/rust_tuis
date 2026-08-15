//! A `FieldKind::TrackerRef` field's value: one or more `(source tracker
//! slug, amount used)` pairs — e.g. a check-in on an F2 batch recording it
//! used 0.5 L from `kombucha-f1-g` and 0.5 L from `kombucha-f1-goz`.
//!
//! The value round-trips as a single string, `"kombucha-f1-g 0.5;
//! kombucha-f1-goz 0.5"` — same separator convention as `mix.rs`
//! (semicolon, so a comma decimal stays unambiguous) and the same
//! `\d+(?:[.,]\d+)?` amount grammar (reused directly via
//! `mix::parse_number_token`, not a second copy of it), but without
//! `mix`'s trailing unit suffix: a reference's amount is always in
//! whatever unit the *referenced* tracker itself declared
//! (`quantity_unit`), which this module has no access to and no reason to
//! repeat per part.
//!
//! Not a port — new in chunk 11 of the relm4 app's `docs/TODO.md`, no TS
//! source equivalent. Generic reference-list math, not tied to any one
//! domain (a tracker slug is just an identifier string here, and this
//! module never touches `Store`) — see `docs/architecture.md` §1, same
//! posture as `mix.rs`.

use crate::mix::{parse_number_token, round_weight};

const SEPARATOR: &str = "; ";

#[derive(Debug, Clone, PartialEq)]
pub struct TrackerRefPart {
    pub slug: String,
    pub amount: f64,
}

// PART = /^(.*?)\s+(\d+(?:[.,]\d+)?)$/ — tolerant on purpose, same posture
// as `mix.rs`'s `parse_part`: anything that doesn't parse as "<slug>
// <number>" is skipped, not an error, so a hand-written or
// pipeline-written entry degrades to "no parts selected" instead of
// breaking the whole edit form.
fn parse_part(chunk: &str) -> Option<TrackerRefPart> {
    let chunk = chunk.trim();
    if chunk.is_empty() {
        return None;
    }
    let last_space = chunk.rfind(char::is_whitespace)?;
    let slug = chunk[..last_space].trim_end();
    let amount_str = chunk[last_space..].trim();
    if slug.is_empty() || amount_str.is_empty() {
        return None;
    }
    let amount = parse_number_token(amount_str)?;
    Some(TrackerRefPart { slug: slug.to_string(), amount: round_weight(amount) })
}

pub fn parse_tracker_refs(value: Option<&str>) -> Vec<TrackerRefPart> {
    let value = match value {
        Some(v) if !v.is_empty() => v,
        _ => return Vec::new(),
    };
    value.split(';').filter_map(parse_part).collect()
}

pub fn format_tracker_refs(parts: &[TrackerRefPart]) -> String {
    parts
        .iter()
        .filter(|p| !p.slug.is_empty())
        .map(|p| format!("{} {}", p.slug, round_weight(p.amount)))
        .collect::<Vec<_>>()
        .join(SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(slug: &str, amount: f64) -> TrackerRefPart {
        TrackerRefPart { slug: slug.to_string(), amount }
    }

    mod parse_tracker_refs_tests {
        use super::*;

        #[test]
        fn reads_a_single_reference() {
            assert_eq!(parse_tracker_refs(Some("kombucha-f1-g 0.5")), vec![part("kombucha-f1-g", 0.5)]);
        }

        #[test]
        fn reads_a_multi_select_blend_reference() {
            assert_eq!(
                parse_tracker_refs(Some("kombucha-f1-g 0.5; kombucha-f1-goz 0.5")),
                vec![part("kombucha-f1-g", 0.5), part("kombucha-f1-goz", 0.5)]
            );
        }

        #[test]
        fn accepts_comma_decimals_too() {
            assert_eq!(parse_tracker_refs(Some("kombucha-f1-g 0,5")), vec![part("kombucha-f1-g", 0.5)]);
        }

        #[test]
        fn rounds_to_2_decimals() {
            assert_eq!(parse_tracker_refs(Some("kombucha-f1-g 0.567")), vec![part("kombucha-f1-g", 0.57)]);
        }

        #[test]
        fn treats_empty_and_em_dash_as_no_selection() {
            assert_eq!(parse_tracker_refs(Some("")), Vec::<TrackerRefPart>::new());
            assert_eq!(parse_tracker_refs(None), Vec::<TrackerRefPart>::new());
            assert_eq!(parse_tracker_refs(Some("\u{2014}")), Vec::<TrackerRefPart>::new());
        }

        #[test]
        fn skips_malformed_chunks_instead_of_erroring() {
            assert_eq!(
                parse_tracker_refs(Some("kombucha-f1-g 0.5; mostly used up")),
                vec![part("kombucha-f1-g", 0.5)]
            );
            assert_eq!(parse_tracker_refs(Some("just some prose")), Vec::<TrackerRefPart>::new());
        }
    }

    mod format_tracker_refs_tests {
        use super::*;

        #[test]
        fn round_trips_through_parse_tracker_refs() {
            let parts = vec![part("kombucha-f1-g", 0.5), part("kombucha-f1-goz", 0.5)];
            assert_eq!(parse_tracker_refs(Some(&format_tracker_refs(&parts))), parts);
        }

        #[test]
        fn drops_parts_with_no_slug() {
            assert_eq!(format_tracker_refs(&[part("", 0.5)]), "");
        }

        #[test]
        fn renders_nothing_for_no_references() {
            assert_eq!(format_tracker_refs(&[]), "");
        }

        #[test]
        fn writes_amounts_back_with_a_point_whatever_was_typed() {
            assert_eq!(format_tracker_refs(&parse_tracker_refs(Some("kombucha-f1-g 0,5"))), "kombucha-f1-g 0.5");
        }
    }
}
