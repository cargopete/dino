/// Lower-cases a project name and keeps only letters, digits, `-` and `_`.
#[must_use]
pub fn normalise_project(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// `header-layout` from `Header layout!`.
#[must_use]
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for character in text.chars() {
        if character.is_alphanumeric() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            slug.extend(character.to_lowercase());
            gap = false;
        } else {
            gap = true;
        }
    }
    slug
}
