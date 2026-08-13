//! Renders a set of field sections + values to a Markdown "log body", and
//! reads one back. Ported from `buildChecklistBody`/`parseBodyToValues` in
//! `website/features/ferment-tracker-app/server/fermentData.ts`. Kept
//! generic (`FieldSection`, not the source's `ChecklistSection`) per
//! `docs/architecture.md` §1 — this is a general labeled-field log format,
//! not specific to any one domain.

use std::collections::HashMap;

use crate::field_label::ParsedField;

const BLANK_PLACEHOLDER: char = '\u{2014}'; // em dash, "—"

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSection {
    pub section: String,
    pub fields: Vec<ParsedField>,
}

/// Collapses any embedded `\r\n`/`\r`/`\n` in a field answer to a single
/// space, then trims. Ported from the source's `sanitizeAnswer` (added to
/// `fermentData.ts` 2026-08-12, `.security/findings.md` RT-2026-08-07-03) —
/// this port's `build_log_body` had the same gap the source did before that
/// fix: without this, an embedded newline in an answer terminates its
/// `- Label: value` line early, and the rest of the value becomes
/// attacker/caller-controlled Markdown appended to the body — a forged
/// `## Section` heading gets re-parsed as a real field on the next edit
/// (see `parse_body_to_values` below), or arbitrary content is injected
/// into what's ultimately published. No legitimate answer needs an
/// internal newline; every checklist field this renders is single-line.
fn sanitize_answer(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' => out.push(' '),
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Renders `# {heading} — {date}`, then one `## {section}` heading per
/// section with one `- {label}: {value}` line per field. Unanswered (or
/// whitespace-only, post-sanitize) fields are written as the em-dash
/// placeholder, so the shape stays complete. Always ends with exactly one
/// trailing newline.
pub fn build_log_body(
    heading: &str,
    date: &str,
    sections: &[FieldSection],
    values: &HashMap<String, String>,
) -> String {
    let mut body = format!("# {heading} \u{2014} {date}\n\n");
    for section in sections {
        body.push_str(&format!("## {}\n", section.section));
        for field in &section.fields {
            let key = format!("{}::{}", section.section, field.label);
            let answer = values
                .get(&key)
                .map(|v| sanitize_answer(v))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| BLANK_PLACEHOLDER.to_string());
            body.push_str(&format!("- {}: {}\n", field.label, answer));
        }
        body.push('\n');
    }
    body.trim_end().to_string() + "\n"
}

fn parse_section_heading(line: &str) -> Option<String> {
    let after_hashes = line.strip_prefix("##")?;
    let first = after_hashes.chars().next()?;
    if !first.is_whitespace() {
        return None;
    }
    let content = after_hashes.trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

fn parse_dash_line(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('-')?;
    let first = rest.chars().next()?;
    if !first.is_whitespace() {
        return None;
    }
    Some(rest.trim_start())
}

fn normalize_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().eq(std::iter::once(BLANK_PLACEHOLDER)) {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Reads a generated log body back into `{"Section::Label": value}` pairs,
/// the shape a field-driven edit form uses. Only reliable for entries this
/// format's writer produced (or anything using the exact `"## Section"` /
/// `"- Label: value"` shape).
///
/// `known_labels` should be every label the caller's field config declares.
/// Matching against them (longest first) — rather than just splitting on the
/// first colon — matters because a label can itself contain a colon (e.g.
/// `"Ratio used (e.g. 1:1:1)"`, or any weighted-blend field declared as
/// `"(mix: ...)"`): splitting at the first colon then lands mid-label and
/// the value becomes unrecoverable. This was a real bug in the source app,
/// fixed 2026-08-01 — dropping this behavior in the port would reintroduce
/// it.
pub fn parse_log_body(body: &str, known_labels: &[String]) -> HashMap<String, String> {
    let mut values = HashMap::new();
    let mut current_section: Option<String> = None;

    let mut labels: Vec<&String> = known_labels.iter().collect();
    labels.sort_by_key(|l| std::cmp::Reverse(l.chars().count()));

    for line in body.split('\n') {
        if let Some(section) = parse_section_heading(line) {
            current_section = Some(section);
            continue;
        }
        let Some(rest) = parse_dash_line(line) else {
            continue;
        };
        let Some(section) = current_section.as_ref() else {
            continue;
        };

        let known = labels.iter().find(|l| rest.starts_with(&format!("{l}:")));
        if let Some(label) = known {
            let value = normalize_value(&rest[label.len() + 1..]);
            values.insert(format!("{section}::{label}"), value);
            continue;
        }

        if let Some(colon_idx) = rest.find(':') {
            let label = &rest[..colon_idx];
            let value = normalize_value(&rest[colon_idx + 1..]);
            values.insert(format!("{section}::{label}"), value);
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field_label::FieldKind;

    fn field(label: &str, kind: FieldKind) -> ParsedField {
        ParsedField { label: label.to_string(), kind, options: None }
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    mod build_log_body_tests {
        use super::*;

        fn checklist() -> Vec<FieldSection> {
            vec![FieldSection {
                section: "Summary".to_string(),
                fields: vec![field("Drawn (mL)", FieldKind::Number), field("Skipped (Yes/No)", FieldKind::YesNo)],
            }]
        }

        #[test]
        fn writes_a_heading_section_headings_and_one_line_per_field() {
            let body = build_log_body(
                "Kombucha Log",
                "2026-07-28",
                &checklist(),
                &map(&[("Summary::Drawn (mL)", "500"), ("Summary::Skipped (Yes/No)", "No")]),
            );
            assert!(body.contains("# Kombucha Log \u{2014} 2026-07-28"));
            assert!(body.contains("## Summary"));
            assert!(body.contains("- Drawn (mL): 500"));
            assert!(body.contains("- Skipped (Yes/No): No"));
        }

        #[test]
        fn records_unanswered_fields_as_placeholder() {
            let body = build_log_body("L", "2026-07-28", &checklist(), &HashMap::new());
            assert!(body.contains("- Drawn (mL): \u{2014}"));
            assert!(body.contains("- Skipped (Yes/No): \u{2014}"));
        }

        #[test]
        fn trims_whitespace_only_answers_to_the_placeholder() {
            let body = build_log_body("L", "2026-07-28", &checklist(), &map(&[("Summary::Drawn (mL)", "   ")]));
            assert!(body.contains("- Drawn (mL): \u{2014}"));
        }

        #[test]
        fn ends_with_exactly_one_trailing_newline() {
            let body = build_log_body("L", "2026-07-28", &checklist(), &HashMap::new());
            assert!(body.ends_with('\n'));
            assert!(!body.ends_with("\n\n"));
        }

        // Regression test for RT-2026-08-07-03: an embedded newline in an
        // answer must not terminate its `- Label: value` line early — the
        // whole crafted payload stays on one line, and in particular no
        // new `## Section` heading (which `parse_log_body` would treat as
        // real structure on a later edit) gets introduced.
        #[test]
        fn collapses_an_embedded_newline_in_an_answer_instead_of_injecting_new_lines() {
            let payload = "ok\n\n<script>alert(1)</script>\n\n## Fake section\n- Notes: spoofed";
            let body = build_log_body("L", "2026-07-28", &checklist(), &map(&[("Summary::Drawn (mL)", payload)]));
            assert!(
                body.contains("- Drawn (mL): ok  <script>alert(1)</script>  ## Fake section - Notes: spoofed"),
                "{body}"
            );
            // The literal text "## Fake section" is now safely embedded
            // inside the one answer line above — what matters is that it
            // never becomes its own LINE (which `parse_log_body` would
            // treat as a real section heading on a later edit). Only the
            // checklist's genuine "## Summary" heading starts a line.
            assert_eq!(body.lines().filter(|l| l.starts_with("## ")).count(), 1, "{body}");
        }

        // `\r\n` (a single logical line break) collapses to one space, not
        // two — same as the source's `/\r\n?|\n/g` matching `\r\n` as one
        // unit rather than `\r` and `\n` separately.
        #[test]
        fn collapses_crlf_as_a_single_space_not_two() {
            let body = build_log_body("L", "2026-07-28", &checklist(), &map(&[("Summary::Drawn (mL)", "a\r\nb")]));
            assert!(body.contains("- Drawn (mL): a b"), "{body}");
        }
    }

    mod parse_log_body_tests {
        use super::*;

        #[test]
        fn keys_each_answer_by_section_and_label() {
            let body = [
                "## Summary",
                "- Drawn (mL): 500",
                "- Refilled (mL): 500",
                "- Draw skipped this week (Yes/No): No",
                "",
                "## Taste & Smell Check",
                "- Notes: Bright, tart, faint apple note",
            ]
            .join("\n");
            assert_eq!(
                parse_log_body(&body, &[]),
                map(&[
                    ("Summary::Drawn (mL)", "500"),
                    ("Summary::Refilled (mL)", "500"),
                    ("Summary::Draw skipped this week (Yes/No)", "No"),
                    ("Taste & Smell Check::Notes", "Bright, tart, faint apple note"),
                ])
            );
        }

        #[test]
        fn maps_the_em_dash_placeholder_back_to_an_empty_answer() {
            assert_eq!(parse_log_body("## S\n- Field: \u{2014}", &[]), map(&[("S::Field", "")]));
        }

        #[test]
        fn keeps_values_that_themselves_contain_colons() {
            assert_eq!(
                parse_log_body("## Feed\n- Ratio used: 1:1:1", &[]),
                map(&[("Feed::Ratio used", "1:1:1")])
            );
        }

        mod when_a_label_itself_contains_a_colon {
            use super::*;

            #[test]
            fn recovers_the_value_using_the_declared_label() {
                let label = "Tea blend (mix: white / black / green / oolong)";
                let line = format!("## Feed\n- {label}: black 60%, green 40%");
                assert_eq!(
                    parse_log_body(&line, &[label.to_string()]),
                    map(&[(&format!("Feed::{label}"), "black 60%, green 40%")])
                );
            }

            #[test]
            fn recovers_sourdoughs_ratio_field_which_had_the_same_bug() {
                let label = "Ratio used (e.g. 1:1:1)";
                let line = format!("## Feed\n- {label}: 1:2:2");
                assert_eq!(
                    parse_log_body(&line, &[label.to_string()]),
                    map(&[(&format!("Feed::{label}"), "1:2:2")])
                );
            }

            #[test]
            fn cuts_in_the_wrong_place_without_the_declared_label_the_old_behaviour() {
                let label = "Tea blend (mix: white / black)";
                let line = format!("## Feed\n- {label}: black 100%");
                let parsed = parse_log_body(&line, &[]);
                assert!(!parsed.contains_key(&format!("Feed::{label}")));
            }

            #[test]
            fn prefers_the_longest_matching_label_when_one_is_a_prefix_of_another() {
                let short = "Tea blend";
                let long = "Tea blend (mix: white / black)";
                let line = format!("## Feed\n- {long}: black 100%");
                assert_eq!(
                    parse_log_body(&line, &[short.to_string(), long.to_string()]),
                    map(&[(&format!("Feed::{long}"), "black 100%")])
                );
            }

            #[test]
            fn still_maps_the_em_dash_placeholder_to_blank() {
                let label = "Sugar blend (mix: cane raw / beet)";
                let line = format!("## Feed\n- {label}: \u{2014}");
                assert_eq!(
                    parse_log_body(&line, &[label.to_string()]),
                    map(&[(&format!("Feed::{label}"), "")])
                );
            }
        }

        #[test]
        fn ignores_list_items_that_appear_before_any_section_heading() {
            assert_eq!(
                parse_log_body("- Stray: value\n## Real\n- Field: 1", &[]),
                map(&[("Real::Field", "1")])
            );
        }

        #[test]
        fn returns_nothing_for_free_form_prose_with_no_checklist_structure() {
            assert_eq!(parse_log_body("Just some notes the scheduled task wrote.", &[]), HashMap::new());
        }

        #[test]
        fn round_trips_values_written_by_build_log_body() {
            let sections = vec![FieldSection {
                section: "Summary".to_string(),
                fields: vec![field("Drawn (mL)", FieldKind::Number), field("Notes", FieldKind::TextWide)],
            }];
            let values = map(&[("Summary::Drawn (mL)", "500"), ("Summary::Notes", "Tart")]);
            let written = build_log_body("Kombucha Log", "2026-07-28", &sections, &values);
            // Strip the leading "# heading — date\n\n" line, same as the
            // source test does before feeding the body back in.
            let stripped = written.split_once("\n\n").map(|(_, rest)| rest).unwrap_or("");
            assert_eq!(parse_log_body(stripped, &[]), values);
        }
    }
}
