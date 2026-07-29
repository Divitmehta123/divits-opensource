use regex::Regex;
use std::collections::BTreeSet;

#[must_use]
pub fn selected_file_paths(request: &str, maximum: usize) -> Vec<String> {
    let quoted = Regex::new(r#"@"([^"]+)""#).ok();
    let Ok(expression) = Regex::new(
        r"(?i)(?:[a-z]:[\\/])?(?:[a-z0-9_.-]+[\\/])+[a-z0-9_.-]+\.(?:rs|toml|md|json|ya?ml|ts|tsx|js|jsx|py|go|java|kt|swift|cs|cpp|c|h|html|css|scss|sql|png|jpe?g|gif|webp|bmp|svg|mp3|wav|m4a|aac|ogg|flac|mp4|mov|mkv|webm|avi)",
    ) else {
        return Vec::new();
    };
    let mut paths = BTreeSet::new();
    if let Some(quoted) = quoted {
        for captures in quoted.captures_iter(request) {
            if let Some(path) = captures.get(1) {
                paths.insert(path.as_str().replace('\\', "/"));
                if paths.len() >= maximum {
                    return paths.into_iter().collect();
                }
            }
        }
    }
    for matched in expression.find_iter(request) {
        paths.insert(matched.as_str().replace('\\', "/"));
        if paths.len() >= maximum {
            break;
        }
    }
    paths.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::selected_file_paths;

    #[test]
    fn extracts_and_deduplicates_relative_file_paths() {
        assert_eq!(
            selected_file_paths("Fix src/main.rs and src\\lib.rs, then src/main.rs.", 10),
            vec!["src/lib.rs", "src/main.rs"]
        );
        assert_eq!(
            selected_file_paths(r#"Review @"docs/design notes.md" next."#, 10),
            vec!["docs/design notes.md"]
        );
    }
}
