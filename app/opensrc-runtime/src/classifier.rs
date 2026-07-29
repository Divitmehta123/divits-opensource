use opensrc_core::ExecutionMode;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModeDecision {
    pub mode: ExecutionMode,
    pub reasons: Vec<&'static str>,
}

#[derive(Debug, Clone, Default)]
pub struct ModeClassifier;

impl ModeClassifier {
    #[must_use]
    pub fn classify(request: &str) -> ModeDecision {
        let lower = request.to_ascii_lowercase();
        let agentic_markers = [
            "architecture",
            "large feature",
            "multi-module",
            "entire repository",
            "unknown area",
            "parallel",
            "browser",
            "full agent",
            "migrate",
            "redesign",
            "screen recording",
            "replicate complete",
            "complete webpage",
            "smooth animations",
            "3d structure",
        ];
        let action_markers = [
            "edit",
            "fix",
            "change",
            "implement",
            "refactor",
            "test",
            "run ",
            "patch",
            "create file",
            "analyze",
            "analyse",
            "replicate",
            "build",
            "make",
            "webpage",
            "website",
            "html",
            "css",
            "javascript",
            "continue",
            "carry on",
            "go ahead",
            "start execution",
            "start the execution",
            "proceed",
            "finish it",
            "complete it",
            "as instructed",
            "as requested",
        ];
        let focused_markers = [
            "one-line",
            "small",
            "localized",
            "known file",
            ".rs",
            ".ts",
            ".py",
            ".go",
            "line ",
        ];
        let agentic_hits = agentic_markers
            .iter()
            .filter(|marker| lower.contains(**marker))
            .count();
        if agentic_hits > 0 || request.len() > 2_000 {
            return ModeDecision {
                mode: ExecutionMode::Agentic,
                reasons: vec!["request spans an unknown, broad, or multi-component area"],
            };
        }
        let has_action = action_markers.iter().any(|marker| lower.contains(marker));
        if has_action {
            let reason = if focused_markers.iter().any(|marker| lower.contains(marker)) {
                "request names a localized coding action"
            } else if is_media_request(&lower) {
                "request requires local media/file handling"
            } else {
                "request requires local actions but not a full task graph"
            };
            return ModeDecision {
                mode: ExecutionMode::Focused,
                reasons: vec![reason],
            };
        }
        if is_filesystem_request(&lower) {
            return ModeDecision {
                mode: ExecutionMode::Focused,
                reasons: vec!["request requires local filesystem access"],
            };
        }
        ModeDecision {
            mode: ExecutionMode::Direct,
            reasons: vec!["request requires no local action"],
        }
    }
}

fn is_filesystem_request(lower: &str) -> bool {
    let filesystem_nouns = [
        " drive",
        "folder",
        "directory",
        "directories",
        "filesystem",
        "file system",
        " disk",
    ];
    let filesystem_actions = [
        "analyze",
        "analyse",
        "scan",
        "inspect",
        "list",
        "show",
        "browse",
        "check",
        "look",
        "tell me",
        "what's",
        "whats",
        "what is in",
        "what is there",
    ];
    let has_volume_reference = lower
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0].is_ascii_alphabetic() && pair[1] == b':')
        || lower
            .as_bytes()
            .windows(7)
            .any(|value| value[0].is_ascii_alphabetic() && value[1..] == *b" drive");
    if has_volume_reference {
        return true;
    }
    let names_filesystem = filesystem_nouns.iter().any(|marker| lower.contains(marker));
    let requests_action = filesystem_actions
        .iter()
        .any(|marker| lower.contains(marker));
    names_filesystem && requests_action
}

fn is_media_request(lower: &str) -> bool {
    [
        "image",
        "screenshot",
        "video",
        "audio",
        "recording",
        "mp4",
        "mp3",
        "wav",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::ModeClassifier;
    use opensrc_core::ExecutionMode;

    #[test]
    fn classifies_direct_question() {
        assert_eq!(
            ModeClassifier::classify("Explain ownership in Rust").mode,
            ExecutionMode::Direct
        );
    }

    #[test]
    fn classifies_local_edit() {
        assert_eq!(
            ModeClassifier::classify("Fix the one-line bug in src/main.rs").mode,
            ExecutionMode::Focused
        );
    }

    #[test]
    fn classifies_architecture_work() {
        assert_eq!(
            ModeClassifier::classify("Redesign the architecture across the entire repository").mode,
            ExecutionMode::Agentic
        );
    }

    #[test]
    fn classifies_media_build_requests_as_local_work() {
        assert_eq!(
            ModeClassifier::classify(
                "Analyze this video properly and replicate complete in webpage"
            )
            .mode,
            ExecutionMode::Agentic
        );
        assert_eq!(
            ModeClassifier::classify("Analyze this screenshot and make the HTML").mode,
            ExecutionMode::Focused
        );
    }

    #[test]
    fn classifies_execution_continuations_as_local_work() {
        for request in [
            "Continue",
            "ok start the execution then as instructed",
            "go ahead and finish it",
        ] {
            assert_eq!(
                ModeClassifier::classify(request).mode,
                ExecutionMode::Focused,
                "{request}"
            );
        }
    }

    #[test]
    fn classifies_natural_drive_inspection_as_local_work() {
        for request in [
            "Analyze F drive and tell all folders name",
            "can you whats there in my F: drive",
            "F drive?",
            "you go and tell me all folders name",
            "show the directories on E:\\",
        ] {
            assert_eq!(
                ModeClassifier::classify(request).mode,
                ExecutionMode::Focused,
                "{request}"
            );
        }
    }
}
