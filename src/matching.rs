//! Deterministic name-only fuzzy matching; native path is a separate tie-break.
pub fn rank(name: &str, query: &str) -> Option<u8> {
    let name = name.to_lowercase();
    let query = query.to_lowercase();
    if name == query {
        return Some(0);
    }
    if name.starts_with(&query) {
        return Some(1);
    }
    if name.contains(&query) {
        return Some(2);
    }
    let mut chars = name.chars();
    query
        .chars()
        .all(|q| chars.by_ref().any(|c| c == q))
        .then_some(3)
}
