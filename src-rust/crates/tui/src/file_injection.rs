use std::fs;
use std::path::{Path, PathBuf};

/// Issues that can occur when processing an @file reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtFileIssue {
    TooLarge(usize), // size in KB that exceeds limit
    Binary,
    Unreadable(String), // error message
    IsDirectory,        // Path points to a directory, not a file
}

/// A parsed file reference from the user's input (e.g., "@src/main.rs").
#[derive(Debug, Clone)]
pub struct AtFileRef {
    /// The token as it appears in the text: "@src/main.rs"
    pub token: String,
    /// Resolved absolute path
    pub path: PathBuf,
    /// File size in KB
    pub size_kb: usize,
    /// File contents (None if issue is Some)
    pub contents: Option<String>,
    /// Issue encountered (None if contents is Some)
    pub issue: Option<AtFileIssue>,
}

/// Parse word-boundary @ tokens from text.
/// Returns (within_limit, oversized).
/// If max_size_kb == 0, oversized is always empty (accept all).
pub fn parse_at_refs(
    text: &str,
    cwd: &Path,
    max_size_kb: usize,
) -> (Vec<AtFileRef>, Vec<AtFileRef>) {
    let mut within_limit = Vec::new();
    let mut oversized = Vec::new();

    let words: Vec<&str> = text.split_whitespace().collect();

    for word in words {
        // Check if word starts with @
        if !word.starts_with('@') {
            continue;
        }

        // Extract the token: "@path/to/file" might be part of "@path/to/file," or "@path/to/file."
        // For now, we'll be permissive and accept the @ and everything after, trimming punctuation.
        let mut token = word.to_string();

        // Remove trailing punctuation, but never strip the leading '@' itself.
        while token.len() > 1
            && token.ends_with(|c: char| c.is_ascii_punctuation())
            && !token.ends_with('/')
        {
            token.pop();
        }

        let path_part = &token[1..]; // Skip the '@'
        if path_part.is_empty() {
            continue; // bare '@' with no path — skip
        }

        // Expand ~ to home directory
        let expanded_path = if let Some(rest) = path_part.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                home.join(rest)
            } else {
                cwd.join(path_part)
            }
        } else if path_part.starts_with('/') {
            PathBuf::from(path_part)
        } else {
            cwd.join(path_part)
        };

        // Check if path exists and is a file (not a directory)
        if !expanded_path.exists() {
            continue; // Skip non-existent paths
        }

        if expanded_path.is_dir() {
            oversized.push(AtFileRef {
                token: token.clone(),
                path: expanded_path,
                size_kb: 0,
                contents: None,
                issue: Some(AtFileIssue::IsDirectory),
            });
            continue;
        }

        let size_kb = match fs::metadata(&expanded_path) {
            Ok(meta) => (meta.len() as usize).div_ceil(1024), // Round up to KB
            Err(e) => {
                oversized.push(AtFileRef {
                    token: token.clone(),
                    path: expanded_path,
                    size_kb: 0,
                    contents: None,
                    issue: Some(AtFileIssue::Unreadable(e.to_string())),
                });
                continue;
            }
        };

        // Check if file is binary
        let contents = match fs::read_to_string(&expanded_path) {
            Ok(contents) => contents,
            Err(_) => {
                oversized.push(AtFileRef {
                    token: token.clone(),
                    path: expanded_path,
                    size_kb,
                    contents: None,
                    issue: Some(AtFileIssue::Binary),
                });
                continue;
            }
        };

        // Check size limit
        if max_size_kb > 0 && size_kb > max_size_kb {
            oversized.push(AtFileRef {
                token,
                path: expanded_path,
                size_kb,
                contents: None,
                issue: Some(AtFileIssue::TooLarge(size_kb)),
            });
        } else {
            within_limit.push(AtFileRef {
                token,
                path: expanded_path,
                size_kb,
                contents: Some(contents),
                issue: None,
            });
        }
    }

    (within_limit, oversized)
}

/// Build XML file blocks from a slice of resolved refs.
/// Returns a string like:
/// ```text
/// <file path="src/main.rs">
/// ...contents...
/// </file>
/// ```
pub fn build_file_blocks(files: &[AtFileRef]) -> String {
    let mut result = String::new();

    for file in files {
        if let Some(contents) = &file.contents {
            result.push_str("<file path=\"");
            result.push_str(&file.path.display().to_string());
            result.push_str("\">\n");
            result.push_str(contents);
            if !contents.ends_with('\n') {
                result.push('\n');
            }
            result.push_str("</file>\n");
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_parse_at_refs_simple() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.txt");
        fs::write(&file_path, "test content").unwrap();

        let input = format!("Please check @{}", file_path.display());
        let cwd = temp.path();

        let (within, oversized) = parse_at_refs(&input, cwd, 1024);
        assert_eq!(within.len(), 1);
        assert_eq!(oversized.len(), 0);
        assert_eq!(within[0].contents.as_ref().unwrap(), "test content");
    }

    #[test]
    fn test_parse_at_refs_nonexistent() {
        let temp = TempDir::new().unwrap();
        let input = format!(
            "Please check @{}",
            temp.path().join("nonexistent.txt").display()
        );

        let (within, oversized) = parse_at_refs(&input, temp.path(), 1024);
        assert_eq!(within.len(), 0);
        assert_eq!(oversized.len(), 0);
    }

    #[test]
    fn test_parse_at_refs_size_limit() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("large.txt");
        let large_content = "x".repeat(10 * 1024); // 10 KB
        fs::write(&file_path, &large_content).unwrap();

        let input = format!("Check @{}", file_path.display());
        let (within, oversized) = parse_at_refs(&input, temp.path(), 5); // 5 KB limit

        assert_eq!(within.len(), 0);
        assert_eq!(oversized.len(), 1);
        match &oversized[0].issue {
            Some(AtFileIssue::TooLarge(kb)) => assert!(*kb >= 10),
            _ => panic!("Expected TooLarge issue"),
        }
    }

    #[test]
    fn test_parse_at_refs_zero_limit_accepts_all() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("large.txt");
        let large_content = "x".repeat(10 * 1024); // 10 KB
        fs::write(&file_path, &large_content).unwrap();

        let input = format!("Check @{}", file_path.display());
        let (within, oversized) = parse_at_refs(&input, temp.path(), 0); // 0 = no limit

        assert_eq!(within.len(), 1);
        assert_eq!(oversized.len(), 0);
    }

    #[test]
    fn test_build_file_blocks() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.rs");
        let file2 = temp.path().join("file2.rs");
        fs::write(&file1, "fn main() {}").unwrap();
        fs::write(&file2, "fn foo() {}").unwrap();

        let refs = vec![
            AtFileRef {
                token: "@file1.rs".to_string(),
                path: file1.clone(),
                size_kb: 1,
                contents: Some("fn main() {}".to_string()),
                issue: None,
            },
            AtFileRef {
                token: "@file2.rs".to_string(),
                path: file2.clone(),
                size_kb: 1,
                contents: Some("fn foo() {}".to_string()),
                issue: None,
            },
        ];

        let blocks = build_file_blocks(&refs);
        assert!(blocks.contains("<file path=\""));
        assert!(blocks.contains("fn main() {}"));
        assert!(blocks.contains("fn foo() {}"));
        assert!(blocks.contains("</file>"));
    }
}
