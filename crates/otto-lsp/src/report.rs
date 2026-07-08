use crate::protocol::Diagnostic;

const MAX_PER_FILE: usize = 20;

fn severity_label(sev: u8) -> &'static str {
    match sev {
        1 => "ERROR",
        2 => "WARN",
        3 => "INFO",
        4 => "HINT",
        _ => "ERROR",
    }
}

/// `SEVERITY [line+1:col+1] message` (1-based). Port of diagnostic.ts pretty().
fn pretty(d: &Diagnostic) -> String {
    let sev = d.severity.unwrap_or(1);
    format!(
        "{} [{}:{}] {}",
        severity_label(sev),
        d.range.start.line + 1,
        d.range.start.character + 1,
        d.message
    )
}

/// Port of diagnostic.ts report(): errors only, cap 20/file, overflow suffix.
pub fn report(file: &str, issues: &[Diagnostic]) -> String {
    let errors: Vec<&Diagnostic> = issues.iter().filter(|d| d.severity == Some(1)).collect();
    if errors.is_empty() {
        return String::new();
    }
    let limited = &errors[..errors.len().min(MAX_PER_FILE)];
    let more = errors.len().saturating_sub(MAX_PER_FILE);
    let body = limited
        .iter()
        .map(|d| pretty(d))
        .collect::<Vec<_>>()
        .join("\n");
    let suffix = if more > 0 {
        format!("\n... and {more} more")
    } else {
        String::new()
    };
    format!("<diagnostics file=\"{file}\">\n{body}{suffix}\n</diagnostics>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Diagnostic, Position, Range};

    fn diag(sev: u8, line: u32, col: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: col,
                },
                end: Position {
                    line,
                    character: col,
                },
            },
            severity: Some(sev),
            code: None,
            source: None,
            message: msg.into(),
        }
    }

    #[test]
    fn errors_only_and_one_based() {
        let out = report("src/x.rs", &[diag(1, 2, 4, "boom"), diag(2, 0, 0, "warn")]);
        assert_eq!(
            out,
            "<diagnostics file=\"src/x.rs\">\nERROR [3:5] boom\n</diagnostics>"
        );
    }

    #[test]
    fn no_errors_returns_empty() {
        assert_eq!(report("x", &[diag(2, 0, 0, "warn")]), "");
    }

    #[test]
    fn caps_at_twenty_with_overflow() {
        let issues: Vec<_> = (0..25).map(|i| diag(1, i, 0, "e")).collect();
        let out = report("x", &issues);
        assert_eq!(out.matches("ERROR").count(), 20);
        assert!(out.contains("... and 5 more"));
    }
}
