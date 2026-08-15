//! A weighted blend: several named parts, each with a weight in grams to 2
//! decimals — a tea blend, a sugar blend, a honey blend.
//!
//! The value round-trips as a single string, `"black 12.5g; green 7.25g"`.
//! Parts are separated by a SEMICOLON, not a comma, so a comma decimal
//! (`"12,5g"`) stays unambiguous. Input accepts either separator character
//! for the decimal; output always uses a point.
//!
//! Ported from `parseMix`/`formatMix`/`orderMix`/`mixTotal`/`roundWeight` in
//! `website/features/ferment-tracker-app/src/lib/mix.ts`. Generic
//! weighted-blend math, not tied to any one domain — see
//! `docs/architecture.md` §1.

const SEPARATOR: &str = "; ";
const DECIMALS_FACTOR: f64 = 100.0; // 2 decimals

#[derive(Debug, Clone, PartialEq)]
pub struct MixPart {
    pub option: String,
    pub weight: f64,
}

pub fn round_weight(grams: f64) -> f64 {
    if !grams.is_finite() {
        return 0.0;
    }
    (grams * DECIMALS_FACTOR).round() / DECIMALS_FACTOR
}

fn is_ascii_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

// \d+(?:[.,]\d+)? — matches only, not partial. `pub(crate)` (not private)
// so `tracker_ref.rs`'s own part-amount parser can reuse the exact same
// grammar instead of a second, possibly-drifting copy of it.
pub(crate) fn parse_number_token(s: &str) -> Option<f64> {
    let (int_part, frac_part) = match s.find(['.', ',']) {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };
    if !is_ascii_digits(int_part) {
        return None;
    }
    let normalized = match frac_part {
        Some(frac) => {
            if !is_ascii_digits(frac) {
                return None;
            }
            format!("{int_part}.{frac}")
        }
        None => int_part.to_string(),
    };
    normalized.parse::<f64>().ok()
}

// PART = /^(.*?)\s+(\d+(?:[.,]\d+)?)\s*g$/i — tolerant on purpose: anything
// that doesn't parse as "<option> <number>g" is skipped, not an error, so a
// hand-written or pipeline-written entry degrades to "no parts selected"
// instead of breaking the whole edit form.
fn parse_part(chunk: &str) -> Option<MixPart> {
    let chunk = chunk.trim();
    if chunk.is_empty() {
        return None;
    }
    let last = chunk.chars().next_back()?;
    if last != 'g' && last != 'G' {
        return None;
    }
    let without_g = &chunk[..chunk.len() - last.len_utf8()];
    let without_g = without_g.trim_end(); // \s* before the trailing g

    let last_space = without_g.rfind(char::is_whitespace)?;
    let option = without_g[..last_space].trim_end();
    let number_str = without_g[last_space..].trim();

    if option.is_empty() || number_str.is_empty() {
        return None;
    }
    let weight = parse_number_token(number_str)?;
    Some(MixPart {
        option: option.to_string(),
        weight: round_weight(weight),
    })
}

pub fn parse_mix(value: Option<&str>) -> Vec<MixPart> {
    let value = match value {
        Some(v) if !v.is_empty() => v,
        _ => return Vec::new(),
    };
    value.split(';').filter_map(parse_part).collect()
}

pub fn format_mix(parts: &[MixPart]) -> String {
    parts
        .iter()
        .filter(|p| !p.option.is_empty())
        .map(|p| format!("{} {}g", p.option, round_weight(p.weight)))
        .collect::<Vec<_>>()
        .join(SEPARATOR)
}

pub fn mix_total(parts: &[MixPart]) -> f64 {
    round_weight(
        parts
            .iter()
            .map(|p| if p.weight.is_finite() { p.weight } else { 0.0 })
            .sum(),
    )
}

// Keeps selected parts in the order the template declares its options, so
// the same blend always serializes identically no matter what order the
// boxes were ticked in.
pub fn order_mix(parts: &[MixPart], options: &[String]) -> Vec<MixPart> {
    let mut ranked: Vec<MixPart> = parts.to_vec();
    ranked.sort_by_key(|p| {
        options
            .iter()
            .position(|opt| opt == &p.option)
            .unwrap_or(options.len())
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(option: &str, weight: f64) -> MixPart {
        MixPart {
            option: option.to_string(),
            weight,
        }
    }

    mod parse_mix_tests {
        use super::*;

        #[test]
        fn reads_a_two_part_blend() {
            assert_eq!(
                parse_mix(Some("black 60g; green 40g")),
                vec![part("black", 60.0), part("green", 40.0)]
            );
        }

        #[test]
        fn handles_multi_word_options() {
            assert_eq!(
                parse_mix(Some("cane raw 70g; kandij 30g")),
                vec![part("cane raw", 70.0), part("kandij", 30.0)]
            );
        }

        #[test]
        fn accepts_point_decimals() {
            assert_eq!(
                parse_mix(Some("green 33.5g; white 66.5g")),
                vec![part("green", 33.5), part("white", 66.5)]
            );
        }

        // Parts are separated by a semicolon precisely so a comma decimal
        // stays unambiguous — this project follows Belgian/metric
        // convention, where "33,5 g" is how a weight is written by hand.
        #[test]
        fn accepts_comma_decimals_too() {
            assert_eq!(
                parse_mix(Some("green 33,5g; white 66,5g")),
                vec![part("green", 33.5), part("white", 66.5)]
            );
        }

        #[test]
        fn rounds_to_2_decimals() {
            assert_eq!(parse_mix(Some("green 12.567g")), vec![part("green", 12.57)]);
        }

        #[test]
        fn writes_weights_back_with_a_point_whatever_was_typed() {
            assert_eq!(format_mix(&parse_mix(Some("green 33,5g"))), "green 33.5g");
        }

        #[test]
        fn treats_empty_and_em_dash_as_no_selection() {
            assert_eq!(parse_mix(Some("")), Vec::<MixPart>::new());
            assert_eq!(parse_mix(None), Vec::<MixPart>::new());
            // buildChecklistBody writes "—" for a field left blank; it must
            // not come back as a phantom part when the entry is edited.
            assert_eq!(parse_mix(Some("\u{2014}")), Vec::<MixPart>::new());
        }

        #[test]
        fn skips_malformed_chunks_instead_of_erroring() {
            assert_eq!(
                parse_mix(Some("black 60g; mostly green")),
                vec![part("black", 60.0)]
            );
            assert_eq!(parse_mix(Some("just some prose")), Vec::<MixPart>::new());
        }
    }

    mod format_mix_tests {
        use super::*;

        #[test]
        fn round_trips_through_parse_mix() {
            let parts = vec![part("black", 60.0), part("green", 40.0)];
            assert_eq!(parse_mix(Some(&format_mix(&parts))), parts);
        }

        #[test]
        fn drops_parts_with_no_option_name() {
            assert_eq!(format_mix(&[part("", 50.0)]), "");
        }

        #[test]
        fn renders_nothing_for_an_empty_blend() {
            assert_eq!(format_mix(&[]), "");
        }
    }

    mod order_mix_tests {
        use super::*;

        fn options() -> Vec<String> {
            ["white", "black", "green", "oolong"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        }

        #[test]
        fn sorts_selected_parts_into_declared_order() {
            let ticked = vec![part("green", 40.0), part("white", 60.0)];
            assert_eq!(format_mix(&order_mix(&ticked, &options())), "white 60g; green 40g");
        }

        #[test]
        fn produces_same_string_regardless_of_tick_order() {
            let a = order_mix(&[part("oolong", 25.0), part("black", 75.0)], &options());
            let b = order_mix(&[part("black", 75.0), part("oolong", 25.0)], &options());
            assert_eq!(format_mix(&a), format_mix(&b));
        }

        #[test]
        fn keeps_undeclared_options_appended_last() {
            let parts = vec![part("rooibos", 10.0), part("black", 90.0)];
            assert_eq!(format_mix(&order_mix(&parts, &options())), "black 90g; rooibos 10g");
        }
    }

    mod mix_total_tests {
        use super::*;

        #[test]
        fn sums_the_weights() {
            assert_eq!(mix_total(&parse_mix(Some("black 60g; green 40g"))), 100.0);
        }

        #[test]
        fn is_zero_for_no_selection() {
            assert_eq!(mix_total(&[]), 0.0);
        }

        #[test]
        fn ignores_non_finite_weights_rather_than_returning_nan() {
            assert_eq!(mix_total(&[part("black", f64::NAN)]), 0.0);
        }
    }
}
