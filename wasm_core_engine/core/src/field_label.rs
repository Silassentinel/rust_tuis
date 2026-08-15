//! Infers an input kind (+ options) from a label's trailing parenthetical,
//! e.g. `"Drawn (mL)"` -> number, `"Draw skipped (Yes/No)"` -> yes/no,
//! `"Brine level (topped up / low / dry)"` -> select.
//!
//! Ported from `parseFieldLabel` in
//! `website/features/ferment-tracker-app/server/fermentData.ts`. The
//! parenthetical-syntax convention itself is generic (not tied to any one
//! domain), so it lives in this domain-agnostic core — see
//! `docs/architecture.md` §1.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    TextWide,
    Number,
    YesNo,
    Select,
    Mix,
    /// `(tracker-ref)` — this field's value is one or more references to
    /// other trackers (each paired with how much of it was used), not
    /// free text. No domain-specific parsing lives here: `core` stays
    /// domain-agnostic (see this module's own doc comment on where the
    /// parenthetical-syntax convention lives), so this variant only marks
    /// *that* a field is a reference — the referenced-tracker/amount-used
    /// value shape and its own string encoding live in the ferment domain
    /// layer's `tracker_ref` module, alongside `mix.rs`'s analogous
    /// split (`FieldKind::Mix` here, `mix.rs`'s `MixPart`/`parse_mix`
    /// elsewhere). No static `options` list either (unlike `Select`/
    /// `Mix`, which declare their choices in the template) — the actual
    /// choices are which trackers exist right now, which only the app
    /// layer (with a live `Store`) can answer.
    TrackerRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedField {
    pub label: String,
    pub kind: FieldKind,
    pub options: Option<Vec<String>>,
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

// \bg\b — a lone "g" bounded by non-word characters (or string edges) on
// both sides, matching JS's ASCII-only \w without the /u flag.
fn contains_word_g(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != 'g' {
            continue;
        }
        let boundary_before = i == 0 || !is_word_char(chars[i - 1]);
        let boundary_after = i + 1 == chars.len() || !is_word_char(chars[i + 1]);
        if boundary_before && boundary_after {
            return true;
        }
    }
    false
}

// The negation regex used before the select branch: /mL|g\)|°C|hours?|weeks?|days?|%/.
// Note `g\)` can never match here since `inner` (by construction) never
// contains a ')' character — faithfully preserved from the source rather
// than "fixed", per the port's job to be behavior-exact.
fn matches_unit_negation(inner: &str) -> bool {
    inner.contains("mL")
        || inner.contains("g)")
        || inner.contains("\u{B0}C")
        || inner.contains("hour")
        || inner.contains("week")
        || inner.contains("day")
        || inner.contains('%')
}

// The number-kind regex: /mL|°C|hours?|weeks?|days?|%|\bg\b/.
fn matches_number_pattern(inner: &str) -> bool {
    inner.contains("mL")
        || inner.contains("\u{B0}C")
        || inner.contains("hour")
        || inner.contains("week")
        || inner.contains("day")
        || inner.contains('%')
        || contains_word_g(inner)
}

// /\(([^)]+)\)\s*$/ — the trailing parenthetical, content excluding ')'.
fn trailing_parenthetical(raw: &str) -> Option<&str> {
    let trimmed = raw.trim_end();
    if !trimmed.ends_with(')') {
        return None;
    }
    let without_close = &trimmed[..trimmed.len() - 1];
    let open_idx = without_close.rfind('(')?;
    let inner = &without_close[open_idx + 1..];
    if inner.is_empty() || inner.contains(')') {
        return None;
    }
    Some(inner)
}

pub fn parse_field_label(raw: &str) -> ParsedField {
    if let Some(inner) = trailing_parenthetical(raw) {
        let inner_lower = inner.to_lowercase();

        // "(e.g. ...)" is an example hint, not a unit/option list — always free text.
        if inner_lower.starts_with("e.g") {
            return ParsedField {
                label: raw.to_string(),
                kind: FieldKind::TextWide,
                options: None,
            };
        }

        // "(tracker-ref)" — checked before "mix:"/select/number below,
        // same reasoning as those: a marker keyword takes priority over
        // the more general rules it would otherwise also match (though in
        // practice "tracker-ref" contains no "/", no lone "g", and no unit
        // substring, so it wouldn't accidentally hit any of them anyway —
        // checked first purely for the same "special markers first"
        // reading order the rest of this function already follows).
        if inner_lower == "tracker-ref" {
            return ParsedField {
                label: raw.to_string(),
                kind: FieldKind::TrackerRef,
                options: None,
            };
        }

        // "(mix: a / b / c)" — checked before the plain `select` rule below,
        // which would otherwise claim it on the " / " separator and collapse
        // it to a single-choice field.
        if inner_lower.starts_with("mix:") {
            let colon_idx = inner.find(':').expect("starts_with mix: guarantees a colon");
            let options: Vec<String> = inner[colon_idx + 1..]
                .split('/')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            return ParsedField {
                label: raw.to_string(),
                kind: FieldKind::Mix,
                options: Some(options),
            };
        }

        if inner_lower.contains("yes/no") {
            return ParsedField {
                label: raw.to_string(),
                kind: FieldKind::YesNo,
                options: None,
            };
        }

        if inner.contains(" / ") && !matches_unit_negation(inner) {
            let options: Vec<String> = inner.split('/').map(|s| s.trim().to_string()).collect();
            return ParsedField {
                label: raw.to_string(),
                kind: FieldKind::Select,
                options: Some(options),
            };
        }

        if matches_number_pattern(inner) {
            return ParsedField {
                label: raw.to_string(),
                kind: FieldKind::Number,
                options: None,
            };
        }
    }
    ParsedField {
        label: raw.to_string(),
        kind: FieldKind::TextWide,
        options: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn treats_labels_as_numeric() {
        for label in [
            "Drawn (mL)",
            "Refilled (mL)",
            "Ambient temperature (\u{B0}C)",
            "Starter kept before feeding (g)",
            "Time to peak / double (hours)",
            "Incubation time (hours)",
        ] {
            assert_eq!(parse_field_label(label).kind, FieldKind::Number, "{label}");
        }
    }

    #[test]
    fn treats_labels_as_yesno() {
        for label in [
            "Draw skipped this week (Yes/No)",
            "Mold present (Yes/No)",
            "Float test (Yes/No, optional)",
            "Hooch present (Yes/No \u{2014} if yes, starter was hungry, consider feeding more often)",
        ] {
            assert_eq!(parse_field_label(label).kind, FieldKind::YesNo, "{label}");
        }
    }

    #[test]
    fn turns_slash_list_into_select_with_trimmed_options() {
        let field = parse_field_label("Brine level (topped up / low / dry)");
        assert_eq!(field.kind, FieldKind::Select);
        assert_eq!(
            field.options,
            Some(vec![
                "topped up".to_string(),
                "low".to_string(),
                "dry".to_string()
            ])
        );
    }

    #[test]
    fn handles_two_option_select() {
        let field = parse_field_label("Bubbles (surface only / throughout)");
        assert_eq!(field.kind, FieldKind::Select);
        assert_eq!(
            field.options,
            Some(vec!["surface only".to_string(), "throughout".to_string()])
        );
    }

    #[test]
    fn recognizes_a_tracker_ref_marker() {
        let field = parse_field_label("Source F1 batch (tracker-ref)");
        assert_eq!(field.kind, FieldKind::TrackerRef);
        assert_eq!(field.options, None);
    }

    #[test]
    fn tracker_ref_marker_is_case_insensitive() {
        assert_eq!(parse_field_label("Source (TRACKER-REF)").kind, FieldKind::TrackerRef);
    }

    #[test]
    fn falls_back_to_free_text_with_no_parenthetical() {
        assert_eq!(parse_field_label("Notes").kind, FieldKind::TextWide);
        assert_eq!(parse_field_label("Follow-up action").kind, FieldKind::TextWide);
    }

    #[test]
    fn turns_mix_list_into_weighted_multi_select() {
        let field = parse_field_label("Tea blend (mix: white / black / green / oolong)");
        assert_eq!(field.kind, FieldKind::Mix);
        assert_eq!(
            field.options,
            Some(vec![
                "white".to_string(),
                "black".to_string(),
                "green".to_string(),
                "oolong".to_string(),
            ])
        );
    }

    #[test]
    fn keeps_multi_word_mix_options_intact() {
        let field = parse_field_label("Sugar blend (mix: cane raw / cane white / beet / kandij)");
        assert_eq!(field.kind, FieldKind::Mix);
        assert_eq!(
            field.options,
            Some(vec![
                "cane raw".to_string(),
                "cane white".to_string(),
                "beet".to_string(),
                "kandij".to_string(),
            ])
        );
    }

    // The mix branch has to run before the select branch: both match on
    // " / ", and select would silently collapse a blend field to a single
    // choice.
    #[test]
    fn prefers_mix_over_select_when_both_could_match() {
        assert_eq!(parse_field_label("Blend (mix: a / b / c)").kind, FieldKind::Mix);
        assert_eq!(parse_field_label("Blend (a / b / c)").kind, FieldKind::Select);
    }

    #[test]
    fn is_case_insensitive_on_mix_marker() {
        assert_eq!(parse_field_label("Blend (MIX: a / b)").kind, FieldKind::Mix);
    }

    // Regression guard for a bug that actually shipped: the "g" in "e.g."
    // matched the grams-unit rule, so "Ratio used (e.g. 1:1:1)" rendered as
    // a number input and silently refused the value the user typed.
    #[test]
    fn keeps_e_g_labels_as_free_text_not_a_number() {
        for label in [
            "Ratio used (e.g. 1:1:1)",
            "Follow-up action (e.g. adjust ratio, move to fridge, increase feed frequency)",
            "Follow-up action (e.g. skip next week, recheck acidity)",
        ] {
            assert_eq!(parse_field_label(label).kind, FieldKind::TextWide, "{label}");
        }
    }

    #[test]
    fn preserves_original_label_verbatim_for_round_tripping() {
        let label = "Smell (tangy/sour = normal; foul/rotten/sulfurous = spoiled, discard)";
        assert_eq!(parse_field_label(label).label, label);
    }
}
