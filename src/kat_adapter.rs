use std::path::Path;

use anyhow::{Context, Result};
use kat::HighlightStyle;

/// 浏览差异时避免解析异常大的文件。
pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SyntaxRange {
    pub start: usize,
    pub end: usize,
    pub style: HighlightStyle,
}

pub(crate) fn display_language_name(language_hint: Option<&str>, path: &str) -> Option<String> {
    let language = kat::detected_language_name(Some(Path::new(path)), "")
        .or_else(|| language_hint.and_then(|hint| kat::canonical_language_name(hint).ok()))?;
    Some(match language {
        "bash" => "Bash".to_owned(),
        "cpp" => "C++".to_owned(),
        "css" => "CSS".to_owned(),
        "go" => "Go".to_owned(),
        "html" => "HTML".to_owned(),
        "javascript" => "JavaScript".to_owned(),
        "json" => "JSON".to_owned(),
        "jsx" => "JSX".to_owned(),
        "markdown" => "Markdown".to_owned(),
        "python" => "Python".to_owned(),
        "ruby" => "Ruby".to_owned(),
        "rust" => "Rust".to_owned(),
        "toml" => "TOML".to_owned(),
        "tsx" => "TSX".to_owned(),
        "typescript" => "TypeScript".to_owned(),
        "yaml" => "YAML".to_owned(),
        "just" => "Just".to_owned(),
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        }
    })
}

#[cfg(test)]
pub(crate) fn syntax_lines_for_test(path: &str, source: &str) -> Result<Vec<Vec<SyntaxRange>>> {
    let document = prepare_document(path, source)?;
    Ok(split_spans(source, document.as_ref()))
}

pub(crate) fn prepare_document(path: &str, source: &str) -> Result<Option<kat::PreparedDocument>> {
    if source.len() > MAX_HIGHLIGHT_BYTES {
        return Ok(None);
    }
    kat::PreparedDocument::detect(
        Some(Path::new(path)),
        source,
        kat::RenderOptions::with_colors((248, 248, 242), (40, 42, 54)),
    )
    .map(Some)
    .with_context(|| format!("failed to highlight {path}"))
}

pub(crate) fn split_spans(
    source: &str,
    document: Option<&kat::PreparedDocument>,
) -> Vec<Vec<SyntaxRange>> {
    let starts = line_starts(source);
    let mut lines = vec![Vec::new(); starts.len()];
    let Some(document) = document else {
        return lines;
    };
    for span in document.spans() {
        let mut line_index = starts
            .partition_point(|start| *start <= span.range.start)
            .saturating_sub(1);
        while let Some(&line_start) = starts.get(line_index) {
            if line_start >= span.range.end {
                break;
            }
            let next_start = starts.get(line_index + 1).copied();
            let mut line_end = next_start.map_or(source.len(), |next| next - 1);
            if source.as_bytes().get(line_end.saturating_sub(1)) == Some(&b'\r') {
                line_end -= 1;
            }
            let start = span.range.start.max(line_start);
            let end = span.range.end.min(line_end);
            if start < end {
                lines[line_index].push(SyntaxRange {
                    start: start - line_start,
                    end: end - line_start,
                    style: span.style,
                });
            }
            line_index += 1;
        }
    }
    lines
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        source
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    starts
}

#[cfg(test)]
mod tests {
    use super::{MAX_HIGHLIGHT_BYTES, display_language_name, syntax_lines_for_test};

    #[test]
    fn splits_kat_spans_at_source_line_and_utf8_boundaries() {
        let source = "fn main() {\r\n    let café = \"hello\";\r\n}\r\n";
        let lines = syntax_lines_for_test("src/main.rs", source).unwrap();
        assert_eq!(lines.len(), 4);
        for (line, ranges) in source.lines().zip(&lines) {
            for range in ranges {
                assert!(line.get(range.start..range.end).is_some());
            }
        }
        assert!(
            lines[1]
                .iter()
                .any(|range| range.style.foreground == (241, 250, 140))
        );
    }

    #[test]
    fn keeps_unknown_and_large_files_plain() {
        assert!(
            syntax_lines_for_test("mystery.unknown", "hello\n")
                .unwrap()
                .iter()
                .all(Vec::is_empty)
        );
        let oversized = "x".repeat(MAX_HIGHLIGHT_BYTES + 1);
        assert!(
            syntax_lines_for_test("huge.rs", &oversized)
                .unwrap()
                .iter()
                .all(Vec::is_empty)
        );
    }

    #[test]
    fn uses_kat_language_detection_for_labels() {
        assert_eq!(
            display_language_name(None, "justfile"),
            Some("Just".to_owned())
        );
        assert_eq!(
            display_language_name(None, "README.md"),
            Some("Markdown".to_owned())
        );
    }
}
