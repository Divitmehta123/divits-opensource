use std::path::{Path, PathBuf};

/// Recognize a new deliverable, not an instruction to modify the current checkout.
pub(crate) fn requests_new_project(request: &str) -> bool {
    let text = request.to_ascii_lowercase();
    let create = [
        "make a ",
        "make an ",
        "build a ",
        "build an ",
        "create a ",
        "create an ",
        "make me ",
        "build me ",
        "new project",
        "new app",
    ]
    .iter()
    .any(|word| text.contains(word));
    let existing = ["existing", "current", "this repo", "this app", "our app"]
        .iter()
        .any(|word| text.contains(word));
    let application = [
        "app",
        "website",
        "webpage",
        "calculator",
        "calculkator",
        "dashboard",
        "game",
        "tool",
        "project",
    ]
    .iter()
    .any(|word| text.contains(word));
    create && application && !existing
}

pub(crate) fn desktop_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // KnownFolder honors OneDrive and redirected/localized Desktop locations.
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
                "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; [Environment]::GetFolderPath('Desktop')"])
            .creation_flags(0x0800_0000)
            .output().ok()?;
        let value = String::from_utf8(output.stdout).ok()?;
        let path = PathBuf::from(value.trim().trim_start_matches('\u{feff}'));
        (output.status.success() && path.is_absolute() && path.is_dir()).then_some(path)
    }
    #[cfg(not(windows))]
    {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let config =
            std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
        if let Ok(directories) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
            for line in directories.lines() {
                if let Some(value) = line.strip_prefix("XDG_DESKTOP_DIR=") {
                    let value = value
                        .trim_matches('"')
                        .replace("$HOME", &home.to_string_lossy());
                    let path = PathBuf::from(value);
                    if path.is_absolute() && path.is_dir() {
                        return Some(path);
                    }
                }
            }
        }
        let path = home.join("Desktop");
        path.is_dir().then_some(path)
    }
}

/// Retain explicit destinations even when the planner needs its fallback.
pub(crate) fn explicit_project_directory(request: &str) -> Option<PathBuf> {
    let quoted = regex::Regex::new(r#"(?i)["'`]([a-z]:[\\/][^"'`\r\n]+)["'`]"#).ok()?;
    let unquoted = regex::Regex::new(r#"(?i)\b([a-z]:[\\/][^\s"'`,;]+)"#).ok()?;
    for pattern in [&quoted, &unquoted] {
        for capture in pattern.captures_iter(request) {
            let path = PathBuf::from(capture[1].trim_end_matches('.'));
            if path.extension().is_none() {
                return Some(path);
            }
        }
    }
    let drive = regex::Regex::new(r"(?i)\b([a-z])\s+drive\b")
        .ok()?
        .captures(request)?
        .get(1)?
        .as_str();
    let name = regex::Regex::new(
        r#"(?i)(?:folder|directory)\s+(?:named|called)\s+["']?([a-z0-9][a-z0-9_-]*)"#,
    )
    .ok()?
    .captures(request)?
    .get(1)?
    .as_str();
    Some(PathBuf::from(format!(
        "{}:/{name}",
        drive.to_ascii_uppercase()
    )))
}

pub(crate) fn default_project_directory(desktop: &Path, request: &str, suffix: &str) -> PathBuf {
    let text = request.to_ascii_lowercase();
    let name = if text.contains("calcul") {
        "calculator"
    } else if text.contains("dashboard") {
        "dashboard"
    } else if text.contains("website") || text.contains("webpage") {
        "website"
    } else {
        "divit-project"
    };
    let candidate = desktop.join(name);
    if candidate.exists() {
        desktop.join(format!("{name}-{suffix}"))
    } else {
        candidate
    }
}

pub(crate) fn validate_delivery_directory(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() || path.parent().is_none() {
        return Err(
            "delivery_directory must be an absolute project folder, not a drive root".into(),
        );
    }
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("delivery_directory must not contain parent traversal".into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_apps_are_separate_from_existing_edits() {
        assert!(requests_new_project(
            "Make a working minimalist calculkator, light background"
        ));
        assert!(requests_new_project("Build a todo app on my Desktop"));
        assert!(!requests_new_project("Make a change to this app"));
        assert!(!requests_new_project("Fix our existing calculator"));
        assert!(!requests_new_project("Explain how to modify this repo"));
    }

    #[test]
    fn delivery_rejects_relative_roots_and_traversal() {
        assert!(validate_delivery_directory(".").is_err());
        assert!(validate_delivery_directory("../calculator").is_err());
        let temp = std::env::temp_dir().join("divit-delivery");
        assert_eq!(
            validate_delivery_directory(&temp.to_string_lossy()).unwrap(),
            temp
        );
        assert!(validate_delivery_directory(&temp.join("../escape").to_string_lossy()).is_err());
    }

    #[test]
    fn fallback_honors_explicit_c_drive_folder_and_ignores_image_paths() {
        assert_eq!(
            explicit_project_directory(
                "Make a calculator and save it in C drive making a folder named calc."
            ),
            Some(PathBuf::from("C:/calc"))
        );
        assert_eq!(
            explicit_project_directory(r#"Make an app in "C:\My Apps\calc""#),
            Some(PathBuf::from(r"C:\My Apps\calc"))
        );
        assert_eq!(
            explicit_project_directory(r"Make a calculator matching C:\sample.png"),
            None
        );
    }
}
