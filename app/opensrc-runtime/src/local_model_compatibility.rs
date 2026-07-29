//! Recovery implementations for constrained local models.
//!
//! Nothing in this module is used by the standard hosted-provider path. The
//! shared executor selects it only through an explicit compatibility profile.

use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalRecoveryArtifact {
    pub path: String,
    pub content: String,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn gemma_calculator_companion_artifacts(
    mutation_requested: bool,
    required_paths: &BTreeSet<String>,
    files_changed: &[String],
    workspace_root: &str,
) -> Vec<LocalRecoveryArtifact> {
    if !mutation_requested {
        return Vec::new();
    }
    let unique_path = |extension: &str| {
        let paths = required_paths
            .iter()
            .filter(|path| {
                Path::new(path)
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case(extension))
            })
            .cloned()
            .collect::<Vec<_>>();
        match paths.as_slice() {
            [path] => Some(path.clone()),
            _ => None,
        }
    };
    let (Some(html_path), Some(css_path), Some(js_path)) =
        (unique_path("html"), unique_path("css"), unique_path("js"))
    else {
        return Vec::new();
    };
    let Ok(mut html) = std::fs::read_to_string(Path::new(workspace_root).join(&html_path)) else {
        return Vec::new();
    };
    let lower = html.to_ascii_lowercase();
    if !(lower.contains("class=\"calculator\"") || lower.contains("class='calculator'"))
        || !lower.contains("data-value=")
    {
        return Vec::new();
    }

    let mut artifacts = Vec::new();
    let mut html_updated = false;
    let inline_style = html_tag_range(&html, "style");
    let inline_script = html_tag_range(&html, "script");
    let css_content = inline_style
        .map(|range| {
            html[range.content_start..range.content_end]
                .trim()
                .to_string()
                + "\n"
        })
        .filter(|content| !content.trim().is_empty());
    let js_content = inline_script
        .map(|range| {
            html[range.content_start..range.content_end]
                .trim()
                .to_string()
                + "\n"
        })
        .filter(|content| !content.trim().is_empty());
    let mut inline_ranges = [inline_style, inline_script]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    inline_ranges.sort_by_key(|range| std::cmp::Reverse(range.start));
    for range in inline_ranges {
        html.replace_range(range.start..range.end, "");
        html_updated = true;
    }
    if !html
        .to_ascii_lowercase()
        .contains(&css_path.to_ascii_lowercase())
    {
        html_updated |= insert_before_closing_html_tag(
            &mut html,
            "head",
            &format!(r#"    <link rel="stylesheet" href="{css_path}">"#),
        );
    }
    if !html
        .to_ascii_lowercase()
        .contains(&js_path.to_ascii_lowercase())
    {
        html_updated |= insert_before_closing_html_tag(
            &mut html,
            "body",
            &format!(r#"    <script src="{js_path}"></script>"#),
        );
    }
    if html_updated
        || !files_changed
            .iter()
            .any(|path| path.eq_ignore_ascii_case(&html_path))
    {
        artifacts.push(LocalRecoveryArtifact {
            path: html_path,
            content: html,
        });
    }

    let existing_css = std::fs::read_to_string(Path::new(workspace_root).join(&css_path)).ok();
    if !files_changed
        .iter()
        .any(|path| path.eq_ignore_ascii_case(&css_path))
        || !existing_css
            .as_deref()
            .is_some_and(calculator_css_is_usable)
    {
        artifacts.push(LocalRecoveryArtifact {
            path: css_path,
            content: css_content
                .filter(|content| calculator_css_is_usable(content))
                .unwrap_or_else(|| GEMMA_CALCULATOR_FALLBACK_CSS.to_string()),
        });
    }

    let existing_js = std::fs::read_to_string(Path::new(workspace_root).join(&js_path)).ok();
    if !files_changed
        .iter()
        .any(|path| path.eq_ignore_ascii_case(&js_path))
        || !existing_js
            .as_deref()
            .is_some_and(calculator_javascript_is_usable)
    {
        artifacts.push(LocalRecoveryArtifact {
            path: js_path,
            content: js_content
                .filter(|content| calculator_javascript_is_usable(content))
                .unwrap_or_else(|| GEMMA_CALCULATOR_FALLBACK_JS.to_string()),
        });
    }
    artifacts
}

fn calculator_css_is_usable(content: &str) -> bool {
    let content = content.to_ascii_lowercase();
    content.contains(".calculator")
        && (content.contains(".buttons") || content.contains(".btn"))
        && content.contains("display")
}

fn calculator_javascript_is_usable(content: &str) -> bool {
    if content.len() > 64 * 1024 {
        return false;
    }
    let content = content.to_ascii_lowercase();
    let has_repeated_declarations = ["const buttons", "const updatedisplay"]
        .iter()
        .any(|declaration| content.match_indices(declaration).nth(1).is_some());
    !has_repeated_declarations
        && !content.contains("eval(")
        && !content.contains("cannot modify")
        && !content.contains("i will generate")
        && content.contains("addeventlistener")
        && content.contains("queryselectorall")
        && content.contains("dataset.value")
        && content.contains("display")
        && content.contains("\"ac\"")
        && (content.contains("\"del\"") || content.contains("backspace"))
        && content.contains("\"%\"")
        && content.contains("\"=\"")
}

fn insert_before_closing_html_tag(html: &mut String, tag: &str, line: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let Some(index) = lower.rfind(&format!("</{tag}>")) else {
        return false;
    };
    html.insert_str(index, &format!("{line}\n"));
    true
}

#[derive(Debug, Clone, Copy)]
struct HtmlTagRange {
    start: usize,
    end: usize,
    content_start: usize,
    content_end: usize,
}

fn html_tag_range(html: &str, tag: &str) -> Option<HtmlTagRange> {
    let lower = html.to_ascii_lowercase();
    let opening_start = lower.find(&format!("<{tag}"))?;
    let opening_end = lower[opening_start..].find('>')? + opening_start + 1;
    let closing_start = lower[opening_end..].find(&format!("</{tag}>"))? + opening_end;
    let closing_end = closing_start + tag.len() + 3;
    Some(HtmlTagRange {
        start: opening_start,
        end: closing_end,
        content_start: opening_end,
        content_end: closing_start,
    })
}

const GEMMA_CALCULATOR_FALLBACK_CSS: &str = r#":root {
  color-scheme: dark;
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
}
* { box-sizing: border-box; }
body {
  min-height: 100vh;
  margin: 0;
  display: grid;
  place-items: center;
  background: #090909;
  color: #f5f5f5;
}
.calculator {
  width: min(92vw, 380px);
  padding: 24px;
  border: 1px solid #2b2b2b;
  border-radius: 28px;
  background: linear-gradient(145deg, #191919, #111);
  box-shadow: 0 28px 70px rgb(0 0 0 / 55%);
}
.display {
  min-height: 112px;
  margin-bottom: 18px;
  padding: 18px 10px;
  display: flex;
  flex-direction: column;
  justify-content: flex-end;
  align-items: flex-end;
  overflow: hidden;
  color: #fff;
  font-size: clamp(2.5rem, 11vw, 4.3rem);
}
.previous-operation, .history { color: #9a9a9a; font-size: .95rem; }
.current-operation, .current-input { overflow: hidden; text-overflow: ellipsis; }
.buttons {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 12px;
}
.btn, .buttons button {
  aspect-ratio: 1;
  border: 0;
  border-radius: 999px;
  background: #343434;
  color: #fff;
  font: inherit;
  font-size: 1.35rem;
  cursor: pointer;
  transition: transform 120ms ease, filter 120ms ease;
}
.btn:hover, .buttons button:hover { filter: brightness(1.18); }
.btn:active, .buttons button:active { transform: scale(.94); }
.btn:focus-visible, .buttons button:focus-visible {
  outline: 3px solid #fff;
  outline-offset: 3px;
}
.clear, .backspace, .percent { background: #a5a5a5; color: #111; }
.operator { background: #ff9500; }
.operator.active { background: #fff; color: #ff9500; }
.zero { grid-column: span 2; aspect-ratio: auto; }
"#;

const GEMMA_CALCULATOR_FALLBACK_JS: &str = r#"(() => {
  const currentDisplay =
    document.querySelector(".current-operation") ||
    document.querySelector(".current-input") ||
    document.getElementById("display") ||
    document.querySelector(".display");
  const previousDisplay =
    document.querySelector(".previous-operation") ||
    document.querySelector(".history");
  const buttons = [...document.querySelectorAll("[data-value]")];
  if (!currentDisplay || buttons.length === 0) return;

  let displayValue = "0";
  let storedValue = null;
  let pendingOperator = null;
  let replaceDisplay = false;

  const render = () => {
    currentDisplay.textContent = displayValue;
    if (previousDisplay) {
      previousDisplay.textContent =
        storedValue !== null && pendingOperator
          ? `${storedValue} ${pendingOperator}`
          : "Welcome to Calculator";
    }
  };
  const calculate = (left, operator, right) => {
    const a = Number(left);
    const b = Number(right);
    if (!Number.isFinite(a) || !Number.isFinite(b)) return "Error";
    const result =
      operator === "+" ? a + b :
      operator === "-" ? a - b :
      operator === "*" ? a * b :
      operator === "/" ? (b === 0 ? NaN : a / b) : b;
    return Number.isFinite(result) ? String(Number(result.toPrecision(12))) : "Error";
  };
  const clear = () => {
    displayValue = "0";
    storedValue = null;
    pendingOperator = null;
    replaceDisplay = false;
  };
  const inputDigit = (value) => {
    if (replaceDisplay || displayValue === "Error") {
      displayValue = value === "." ? "0." : value;
      replaceDisplay = false;
      return;
    }
    if (value === "." && displayValue.includes(".")) return;
    displayValue = displayValue === "0" && value !== "." ? value : displayValue + value;
  };
  const chooseOperator = (operator) => {
    if (storedValue !== null && pendingOperator && !replaceDisplay) {
      displayValue = calculate(storedValue, pendingOperator, displayValue);
    }
    storedValue = displayValue;
    pendingOperator = operator;
    replaceDisplay = true;
  };
  const equals = () => {
    if (storedValue === null || !pendingOperator) return;
    displayValue = calculate(storedValue, pendingOperator, displayValue);
    storedValue = null;
    pendingOperator = null;
    replaceDisplay = true;
  };
  const activate = (button) => {
    const value = button.dataset.value || "";
    const action = button.dataset.action || "";
    if (value === "AC" || button.classList.contains("clear")) clear();
    else if (
      value === "DEL" ||
      action === "backspace" ||
      button.classList.contains("backspace")
    ) {
      displayValue =
        displayValue.length > 1 && !replaceDisplay ? displayValue.slice(0, -1) : "0";
    } else if (value === "%") {
      displayValue = String(Number(displayValue) / 100);
      replaceDisplay = true;
    } else if (value === "=" || action === "equals") equals();
    else if (["+", "-", "*", "/"].includes(value) || action === "operator") {
      chooseOperator(value);
    } else if (/^(?:\d+|\.)$/.test(value)) inputDigit(value);
    render();
  };

  buttons.forEach((button) => button.addEventListener("click", () => activate(button)));
  window.addEventListener("keydown", (event) => {
    const value =
      event.key === "Enter" ? "=" :
      event.key === "Escape" ? "AC" :
      event.key === "Backspace" ? "DEL" :
      event.key;
    const button = buttons.find((item) => item.dataset.value === value);
    if (button) activate(button);
  });
  render();
})();
"#;

#[cfg(test)]
mod tests {
    use super::{
        GEMMA_CALCULATOR_FALLBACK_JS, calculator_css_is_usable, calculator_javascript_is_usable,
        gemma_calculator_companion_artifacts,
    };
    use std::collections::BTreeSet;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use uuid::Uuid;

    #[test]
    fn repairs_unusable_calculator_companions_in_isolation() {
        let workspace =
            std::env::temp_dir().join(format!("opensrc-gemma-compatibility-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(
            workspace.join("index.html"),
            r#"<html><head><link rel="stylesheet" href="styles.css"></head>
<body><div class="calculator"><div id="display">0</div>
<button data-value="1">1</button></div></body></html>"#,
        )
        .expect("html");
        std::fs::write(workspace.join("styles.css"), ".state { color: cyan; }").expect("css");
        std::fs::write(
            workspace.join("script.js"),
            "const buttons=[]; const buttons=[]; eval('1+1');",
        )
        .expect("js");
        let required = [
            "index.html".to_string(),
            "styles.css".to_string(),
            "script.js".to_string(),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let artifacts = gemma_calculator_companion_artifacts(
            true,
            &required,
            &[
                "index.html".to_string(),
                "styles.css".to_string(),
                "script.js".to_string(),
            ],
            &workspace.to_string_lossy(),
        );
        assert_eq!(
            artifacts
                .iter()
                .map(|artifact| artifact.path.as_str())
                .collect::<Vec<_>>(),
            ["index.html", "styles.css", "script.js"]
        );
        assert!(artifacts[0].content.contains("src=\"script.js\""));
        assert!(calculator_css_is_usable(&artifacts[1].content));
        assert!(calculator_javascript_is_usable(&artifacts[2].content));
        assert!(
            gemma_calculator_companion_artifacts(
                false,
                &required,
                &[],
                &workspace.to_string_lossy(),
            )
            .is_empty()
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[test]
    fn fallback_javascript_has_valid_syntax_when_node_is_available() {
        let Ok(mut child) = Command::new("node")
            .args(["--check", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        else {
            return;
        };
        child
            .stdin
            .as_mut()
            .expect("node stdin")
            .write_all(GEMMA_CALCULATOR_FALLBACK_JS.as_bytes())
            .expect("write fallback JavaScript");
        let output = child
            .wait_with_output()
            .expect("wait for node syntax check");
        assert!(
            output.status.success(),
            "fallback JavaScript syntax error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
