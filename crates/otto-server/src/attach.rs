//! `resolve_attachment` — read a workspace-relative path into either an
//! inlined text envelope, a base64 image data-URL, or a typed rejection.
//!
//! Self-contained (no dependency on `otto-tools`); mirrors the format and
//! limits of `otto-tools`' `read.rs`.
//!
//! Wired into the prompt handler (`session_prompt` in `lib.rs`), which resolves
//! each `files[].path` via [`resolve_attachment`] before starting a run.

use base64::Engine as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

const MAX_TEXT_BYTES: usize = 50 * 1024;
const MAX_LINES: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
const MAX_IMAGE_B64: usize = 5 * 1024 * 1024;
/// Raw byte cap for image reads, derived from [`MAX_IMAGE_B64`]: base64
/// expands 3 raw bytes into 4 encoded chars, so any file whose raw size
/// exceeds this bound necessarily encodes past the base64 cap. Checking this
/// *before* encoding avoids allocating the base64 string for oversized files.
const MAX_IMAGE_RAW: usize = MAX_IMAGE_B64 * 3 / 4;

pub enum ResolvedAttachment {
    Text(String),
    Image {
        mime: String,
        filename: String,
        data_url: String,
    },
}

#[derive(Debug)]
pub enum AttachError {
    NotFound,
    Traversal,
    Binary,
    /// Reserved for a future extension-based rejection (e.g. an explicitly
    /// disallowed non-image binary type); not yet constructed by
    /// `resolve_attachment`, which currently only distinguishes "binary" from
    /// "text". Narrowly allowed rather than dropped so the message-formatting
    /// arm in `AttachError::message` stays exhaustive for callers that already
    /// match on it.
    #[allow(dead_code)]
    Unsupported(String),
    TooLarge,
    Io(String),
}

impl AttachError {
    pub fn message(&self, path: &str) -> String {
        match self {
            AttachError::NotFound => format!("attachment \"{path}\": not found"),
            AttachError::Traversal => format!("attachment \"{path}\": path escapes workspace"),
            AttachError::Binary => format!("attachment \"{path}\": appears to be binary"),
            AttachError::Unsupported(t) => format!("attachment \"{path}\": unsupported type {t}"),
            AttachError::TooLarge => format!("attachment \"{path}\": exceeds 5 MB"),
            AttachError::Io(e) => format!("attachment \"{path}\": {e}"),
        }
    }
}

fn image_mime_for(path: &str) -> Option<&'static str> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// Binary sniff mirroring otto-tools read.rs: NUL byte or >30% non-printable in the sample.
fn is_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.contains(&0) {
        return true;
    }
    let non_printable = sample
        .iter()
        .filter(|&&b| b < 0x09 || (b > 0x0d && b < 0x20))
        .count();
    non_printable * 100 / sample.len() > 30
}

/// Read at most `cap + 1` bytes of `path` into memory, so a file far larger
/// than `cap` is never fully slurped just to be rejected. Returns the bytes
/// read plus whether the read hit the cap (i.e. the file is larger than
/// `cap`).
fn read_capped(path: &Path, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut buf)?;
    let truncated = buf.len() > cap;
    Ok((buf, truncated))
}

/// Join `rel_path` under `root`, rejecting absolute paths and any result that escapes `root`.
fn safe_join(root: &Path, rel_path: &str) -> Result<PathBuf, AttachError> {
    let rel = Path::new(rel_path);
    if rel.is_absolute() {
        return Err(AttachError::Traversal);
    }
    // Reject any `..` component lexically, before touching the filesystem.
    // `canonicalize()` below only resolves paths that already exist, so a
    // traversal attempt against a not-yet-existing target (e.g.
    // `../escape.txt`) would otherwise fall through to `NotFound` instead of
    // being flagged as `Traversal`.
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AttachError::Traversal);
    }
    let joined = root.join(rel);
    let canon_root = root
        .canonicalize()
        .map_err(|e| AttachError::Io(e.to_string()))?;
    let canon = match joined.canonicalize() {
        Ok(c) => c,
        Err(_) => return Err(AttachError::NotFound),
    };
    if !canon.starts_with(&canon_root) {
        return Err(AttachError::Traversal);
    }
    Ok(canon)
}

fn text_envelope(rel_path: &str, content: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut bytes_used = 0usize;
    let mut truncated = false;
    let mut total = 0usize;
    for (i, raw) in content.lines().enumerate() {
        if i >= MAX_LINES {
            truncated = true;
            break;
        }
        let line: String = raw.chars().take(MAX_LINE_LEN).collect();
        let numbered = format!("{}: {}", i + 1, line);
        // +1 accounts for the "\n" that will join this line to the previous one.
        let size = numbered.len() + usize::from(!lines.is_empty());
        if bytes_used + size > MAX_TEXT_BYTES {
            truncated = true;
            break;
        }
        bytes_used += size;
        lines.push(numbered);
        total = i + 1;
    }
    let footer = if truncated {
        format!("(File truncated - showing first {total} lines)")
    } else {
        format!("(End of file - total {total} lines)")
    };
    format!(
        "<path>{rel_path}</path>\n<type>file</type>\n<content>\n{}\n{footer}\n</content>",
        lines.join("\n"),
    )
}

pub fn resolve_attachment(root: &Path, rel_path: &str) -> Result<ResolvedAttachment, AttachError> {
    let abs = safe_join(root, rel_path)?;

    if let Some(mime) = image_mime_for(rel_path) {
        let (bytes, over_cap) =
            read_capped(&abs, MAX_IMAGE_RAW).map_err(|e| AttachError::Io(e.to_string()))?;
        // Reject before encoding: a file over the raw cap necessarily exceeds
        // MAX_IMAGE_B64 once base64-encoded, so there is no reason to pay for
        // the ~1.33x allocation just to reject it.
        if over_cap {
            return Err(AttachError::TooLarge);
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        // Belt-and-suspenders: the raw-byte check above should already make
        // this unreachable, but keep it in case the 3/4 ratio ever drifts.
        if b64.len() > MAX_IMAGE_B64 {
            return Err(AttachError::TooLarge);
        }
        let filename = Path::new(rel_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(rel_path)
            .to_string();
        return Ok(ResolvedAttachment::Image {
            data_url: format!("data:{mime};base64,{b64}"),
            mime: mime.to_string(),
            filename,
        });
    }

    // Bounded read: the envelope's line-accumulation loop already enforces
    // the MAX_TEXT_BYTES *output* budget, but without this cap a
    // multi-gigabyte text file would still be fully read into memory (and
    // `from_utf8_lossy`-allocated) before that trimming ever runs. A
    // possibly mid-char/mid-line cutoff is fine: `from_utf8_lossy` handles a
    // truncated multi-byte tail, and the line-accumulation loop below only
    // ever emits whole lines, so a cut mid-line simply becomes the last
    // (truncated-footer) line rather than corrupting output.
    let (bytes, _over_cap) =
        read_capped(&abs, MAX_TEXT_BYTES).map_err(|e| AttachError::Io(e.to_string()))?;
    if is_binary(&bytes) {
        return Err(AttachError::Binary);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(ResolvedAttachment::Text(text_envelope(rel_path, &text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("otto-att-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn resolves_text_file_to_envelope() {
        let d = tmpdir("txt");
        fs::write(d.join("hi.txt"), "line one\nline two\n").unwrap();
        let r = resolve_attachment(&d, "hi.txt").unwrap();
        match r {
            ResolvedAttachment::Text(s) => {
                assert!(s.contains("<path>"), "has path tag");
                assert!(s.contains("hi.txt"));
                assert!(s.contains("<content>"));
                assert!(s.contains("1: line one"), "numbered line");
                assert!(s.contains("2: line two"));
            }
            _ => panic!("expected Text"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolves_png_to_base64_data_url() {
        let d = tmpdir("png");
        // 1x1 PNG (contains a NUL byte -> would sniff as binary, but image path wins by extension)
        let png: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        fs::write(d.join("p.png"), png).unwrap();
        let r = resolve_attachment(&d, "p.png").unwrap();
        match r {
            ResolvedAttachment::Image {
                mime,
                filename,
                data_url,
            } => {
                assert_eq!(mime, "image/png");
                assert_eq!(filename, "p.png");
                assert!(data_url.starts_with("data:image/png;base64,"));
            }
            _ => panic!("expected Image"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_binary_non_image() {
        let d = tmpdir("bin");
        fs::write(d.join("a.bin"), [0u8, 1, 2, 3, 0, 0, 0, 7]).unwrap();
        assert!(matches!(
            resolve_attachment(&d, "a.bin"),
            Err(AttachError::Binary)
        ));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_missing_and_traversal() {
        let d = tmpdir("miss");
        assert!(matches!(
            resolve_attachment(&d, "nope.txt"),
            Err(AttachError::NotFound)
        ));
        assert!(matches!(
            resolve_attachment(&d, "../escape.txt"),
            Err(AttachError::Traversal)
        ));
        assert!(matches!(
            resolve_attachment(&d, "/etc/passwd"),
            Err(AttachError::Traversal)
        ));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn truncates_at_byte_cap_without_panicking_on_multibyte_char() {
        let d = tmpdir("multibyte");
        // Many short lines, each containing a multi-byte '€' (3 bytes in
        // UTF-8), whose cumulative byte total crosses MAX_TEXT_BYTES well
        // before MAX_LINES is reached. A euro sign lands right around the
        // byte-51200 region regardless of exact line width, so if the old
        // code path (`String::truncate(MAX_TEXT_BYTES)` on the raw bytes)
        // were still in play, it would risk splitting a multi-byte char and
        // panicking. The new line-accumulation approach must instead stop
        // cleanly on a line boundary and report the file as truncated.
        let line = "x".repeat(150) + "€";
        let content: String = std::iter::repeat_n(line, 500).map(|l| l + "\n").collect();
        assert!(content.len() > MAX_TEXT_BYTES);
        fs::write(d.join("big.txt"), &content).unwrap();

        let r = resolve_attachment(&d, "big.txt").unwrap();
        match r {
            ResolvedAttachment::Text(s) => {
                assert!(
                    s.contains("File truncated"),
                    "expected truncation footer, got: {}",
                    &s[s.len().saturating_sub(200)..]
                );
            }
            _ => panic!("expected Text"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn truncates_at_max_lines() {
        let d = tmpdir("manylines");
        let content: String = (1..=(MAX_LINES + 500))
            .map(|i| format!("line {i}\n"))
            .collect();
        fs::write(d.join("many.txt"), &content).unwrap();

        let r = resolve_attachment(&d, "many.txt").unwrap();
        match r {
            ResolvedAttachment::Text(s) => {
                assert!(
                    s.contains("File truncated"),
                    "expected truncation footer for line-count cap"
                );
                assert!(s.contains(&format!("first {MAX_LINES} lines")));
            }
            _ => panic!("expected Text"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn read_capped_reports_truncation_without_reading_past_cap_plus_one() {
        let d = tmpdir("readcapped");
        let path = d.join("over.bin");
        fs::write(&path, vec![7u8; 1000]).unwrap();

        let (bytes, over_cap) = read_capped(&path, 200).unwrap();
        assert!(over_cap, "1000-byte file must report over_cap for cap=200");
        assert_eq!(bytes.len(), 201, "must read at most cap+1 bytes");

        let (bytes, over_cap) = read_capped(&path, 5000).unwrap();
        assert!(
            !over_cap,
            "1000-byte file must not report over_cap for cap=5000"
        );
        assert_eq!(bytes.len(), 1000);

        let _ = fs::remove_dir_all(&d);
    }

    /// A text file well over `MAX_TEXT_BYTES` must resolve to a truncated
    /// envelope without reading (or allocating) the whole file: this locks in
    /// the bounded-read fix that stops a multi-GB text attachment from being
    /// fully slurped into memory before the output trimming ever runs.
    #[test]
    fn bounds_read_of_oversized_text_file() {
        let d = tmpdir("hugetext");
        // ~200KB, well over MAX_TEXT_BYTES (50KB).
        let content: String = (1..=10_000).map(|i| format!("line {i} filler\n")).collect();
        assert!(content.len() > 200 * 1024 / 2, "sanity: content is large");
        fs::write(d.join("huge.txt"), &content).unwrap();

        let r = resolve_attachment(&d, "huge.txt").unwrap();
        match r {
            ResolvedAttachment::Text(s) => {
                assert!(
                    s.contains("File truncated"),
                    "expected truncation footer: {s}"
                );
                // The envelope's content section (between <content> and the
                // footer line) must stay within the MAX_TEXT_BYTES budget —
                // i.e. the bounded read didn't just let everything through.
                let content_start = s.find("<content>\n").unwrap() + "<content>\n".len();
                let content_len = s.len() - content_start;
                assert!(
                    content_len <= MAX_TEXT_BYTES + 4096,
                    "envelope content ({content_len} bytes) should stay near the \
                     MAX_TEXT_BYTES budget, not the full ~200KB source"
                );
            }
            _ => panic!("expected Text"),
        }
        let _ = fs::remove_dir_all(&d);
    }

    /// An image file whose raw size exceeds `MAX_IMAGE_RAW` must be rejected
    /// as `TooLarge` before any base64 encoding is attempted.
    #[test]
    fn rejects_oversized_image_before_encoding() {
        let d = tmpdir("bigimage");
        let bytes = vec![0u8; MAX_IMAGE_RAW + 100];
        fs::write(d.join("big.png"), &bytes).unwrap();

        let r = resolve_attachment(&d, "big.png");
        assert!(
            matches!(r, Err(AttachError::TooLarge)),
            "expected TooLarge, got {}",
            match &r {
                Ok(_) => "Ok(_)".to_string(),
                Err(e) => format!("{e:?}"),
            }
        );
        let _ = fs::remove_dir_all(&d);
    }
}
