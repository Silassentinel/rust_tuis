//! Generic text-formatting helpers.
//!
//! `title_case` is ported from `titleCase` in
//! `website/features/ferment-tracker-app/server/fermentData.ts` — a plain
//! slug-to-display-name transform, not tied to any one domain.
//!
//! `slugify` is ported from the local (unexported) `slugify` helper in
//! `website/features/ferment-tracker-app/src/features/new-tracker/
//! NewTrackerForm.tsx` — its inverse: a display-name-to-slug transform.
//! Generic enough to live here rather than in the ferment-tracker domain
//! layer, same reasoning as `title_case`.

/// Title-cases a hyphen/space-separated slug: `"vegetable-ferment"` ->
/// `"Vegetable Ferment"`. Collapses empty segments, so repeated separators
/// (`"a--b"`) don't produce blank words.
pub fn title_case(slug_like: &str) -> String {
    slug_like
        .split(|c: char| c == '-' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `"Tempeh Batch 1"` -> `"tempeh-batch-1"`: lowercases, then collapses
/// every run of characters outside `[a-z0-9]` (spaces, punctuation,
/// non-ASCII letters that survive lowercasing) into a single hyphen, then
/// trims any leading/trailing hyphen. Ported from the source's
/// `name.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g,
/// '')`.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_hyphen = false;
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            last_was_hyphen = false;
        } else if !last_was_hyphen && !out.is_empty() {
            out.push('-');
            last_was_hyphen = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_cases_hyphenated_type_slugs_for_display() {
        assert_eq!(title_case("vegetable-ferment"), "Vegetable Ferment");
        assert_eq!(title_case("hot-sauce-mash"), "Hot Sauce Mash");
        assert_eq!(title_case("jun"), "Jun");
    }

    #[test]
    fn handles_spaces_and_collapses_empty_segments() {
        assert_eq!(title_case("cheese aging"), "Cheese Aging");
        assert_eq!(title_case("a--b"), "A B");
    }

    #[test]
    fn slugifies_a_display_name() {
        assert_eq!(slugify("Tempeh Batch 1"), "tempeh-batch-1");
        assert_eq!(slugify("Kombucha F1 — G"), "kombucha-f1-g");
    }

    #[test]
    fn collapses_runs_of_non_alphanumeric_characters() {
        assert_eq!(slugify("a!!!b   c"), "a-b-c");
    }

    #[test]
    fn trims_leading_and_trailing_separators() {
        assert_eq!(slugify("  --Weird Name--  "), "weird-name");
    }

    #[test]
    fn is_empty_for_a_name_with_no_alphanumeric_characters() {
        assert_eq!(slugify("---"), "");
        assert_eq!(slugify(""), "");
    }
}
