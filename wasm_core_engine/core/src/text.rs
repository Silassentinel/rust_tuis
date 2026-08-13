//! Generic text-formatting helpers.
//!
//! `title_case` is ported from `titleCase` in
//! `website/features/ferment-tracker-app/server/fermentData.ts` — a plain
//! slug-to-display-name transform, not tied to any one domain.

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
}
